//! Native branch switching. SAFE checkout checks all file conflicts before it
//! writes; newly created refs and tracking configuration are rolled back on error.

use std::{
    collections::BTreeSet,
    error::Error,
    fmt, fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use git2::{
    BranchType, CheckoutNotificationType, Config, DiffOptions, Direction, ErrorCode, Oid,
    Reference, Repository, RepositoryState, build::CheckoutBuilder,
};

#[path = "checkout/index_transaction.rs"]
pub(crate) mod index_transaction;
use index_transaction::IndexTransaction;

#[derive(Clone, Debug)]
pub struct CheckoutRequest {
    /// Local short name/refs/heads/name, or remote/name/refs/remotes/remote/name.
    pub branch: String,
    pub remote: bool,
    /// None denotes a detached HEAD, not an unchecked branch identity.
    pub expected_branch: Option<String>,
    pub expected_head: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CheckoutOutcome {
    pub branch: String,
    pub head: String,
    pub created: bool,
    pub upstream: Option<String>,
    pub summary: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckoutErrorKind {
    InvalidRepository,
    InvalidRequest,
    ConcurrentChange,
    DetachedHead,
    UnbornHead,
    DirtyWorkingTree,
    InProgress,
    BranchInUse,
    BranchConflict,
    NotFound,
    CheckoutFailed,
    RollbackFailed,
}

#[derive(Clone, Debug)]
pub struct CheckoutError {
    pub kind: CheckoutErrorKind,
    pub message: String,
}

impl CheckoutError {
    pub fn new(kind: CheckoutErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for CheckoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl Error for CheckoutError {}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HeadState {
    reference: Option<String>,
    oid: Oid,
}

struct Target {
    name: String,
    reference: String,
    oid: Oid,
    create: bool,
    upstream: Option<String>,
    remote: Option<(String, String, String)>, // remote name, server ref, tracking ref
    tracking_oid: Option<Oid>,
}

pub(super) fn execute(
    root: &Path,
    request: &CheckoutRequest,
) -> Result<CheckoutOutcome, CheckoutError> {
    execute_inner(root, request, None)
}

type CheckoutProgress<'a> = dyn FnMut(Option<&Path>, usize, usize) + 'a;

// Exercise real libgit2 failures after its internal preflight, without timing races or
// changing the production SAFE/overwrite_ignored options.
#[cfg(test)]
#[allow(dead_code)] // Used by the standalone native integration fixture.
pub(super) fn execute_with_progress(
    root: &Path,
    request: &CheckoutRequest,
    progress: &mut CheckoutProgress<'_>,
) -> Result<CheckoutOutcome, CheckoutError> {
    execute_inner(root, request, Some(progress))
}

fn execute_inner(
    root: &Path,
    request: &CheckoutRequest,
    progress: Option<&mut CheckoutProgress<'_>>,
) -> Result<CheckoutOutcome, CheckoutError> {
    let canonical = fs::canonicalize(root).map_err(|_| invalid_repository())?;
    let repository = Repository::open(&canonical).map_err(|_| invalid_repository())?;
    if repository.is_bare()
        || repository
            .workdir()
            .and_then(|path| fs::canonicalize(path).ok())
            .as_ref()
            != Some(&canonical)
    {
        return Err(invalid_repository());
    }
    let original = expected_head(&repository, request)?;
    let mut index_transaction = IndexTransaction::prepare(&repository)?;
    require_checkout_ready(&repository)?;
    let target = target(&repository, request)?;
    ensure_available(&repository, &canonical, &target.reference)?;

    let mut transaction = repository.transaction().map_err(map_git_error)?;
    transaction.lock_ref("HEAD").map_err(map_git_error)?;
    if let Some(reference) = &original.reference
        && reference != &target.reference
    {
        transaction.lock_ref(reference).map_err(map_git_error)?;
    }
    if !target.create {
        transaction
            .lock_ref(&target.reference)
            .map_err(map_git_error)?;
    }
    if let Some((_, _, tracking)) = &target.remote {
        transaction.lock_ref(tracking).map_err(map_git_error)?;
    }
    expected_head(&repository, request)?;
    ensure_target(&repository, &target)?;
    ensure_available(&repository, &canonical, &target.reference)?;
    require_checkout_ready(&repository)?;
    let commit = repository.find_commit(target.oid).map_err(map_git_error)?;
    let tree = commit.tree().map_err(map_git_error)?;
    let original_tree = repository
        .find_commit(original.oid)
        .and_then(|commit| commit.tree())
        .map_err(map_git_error)?;
    require_nonoverlapping_changes(&repository, &original_tree, &tree)?;

    // The config lock is retained across the entire operation, including any
    // rollback. Writes use a separate staging file, preserving config.lock as
    // the sentinel honored by Git and libgit2 writers.
    let mut config = if target.create {
        let (remote, merge, _) = target.remote.as_ref().expect("new branches have a remote");
        Some(ConfigEdit::prepare(
            &repository,
            &target.name,
            remote,
            merge,
        )?)
    } else {
        None
    };
    let mut created = false;
    let mut checked_out = false;
    let result = (|| {
        if target.create {
            repository
                .branch(&target.name, &commit, false)
                .map_err(map_git_error)?;
            created = true;
            transaction
                .lock_ref(&target.reference)
                .map_err(map_git_error)?;
            if let Some(config) = &mut config {
                config.apply()?;
            }
            // Worktree-specific configuration may override common config.
            let reopened = Repository::open(&canonical).map_err(map_git_error)?;
            let branch = reopened
                .find_branch(&target.name, BranchType::Local)
                .map_err(map_git_error)?;
            if branch
                .upstream()
                .ok()
                .and_then(|branch| branch.get().name().ok().map(str::to_owned))
                != target
                    .remote
                    .as_ref()
                    .map(|(_, _, tracking)| tracking.clone())
            {
                return Err(CheckoutError::new(
                    CheckoutErrorKind::BranchConflict,
                    "该分支存在冲突的跟踪配置，未切换。",
                ));
            }
        }
        expected_head(&repository, request)?;
        if repository
            .refname_to_id(&target.reference)
            .map_err(map_git_error)?
            != target.oid
        {
            return Err(concurrent_change());
        }
        ensure_available(&repository, &canonical, &target.reference)?;
        require_checkout_ready(&repository)?;
        require_nonoverlapping_changes(&repository, &original_tree, &tree)?;
        let mut collisions = Vec::new();
        let mut dirty_paths = Vec::new();
        let checkout = {
            // git2's dry_run() selects CHECKOUT_NONE, which does not check
            // working-file conflicts. SAFE performs its own complete plan and
            // conflict check before its file-write passes; collect those paths
            // from that real preflight instead of doing another tree walk.
            let mut options = CheckoutBuilder::new();
            options
                .safe()
                .overwrite_ignored(false)
                .notify_on(CheckoutNotificationType::CONFLICT | CheckoutNotificationType::DIRTY)
                .notify(|kind, path, _, _, _| {
                    let paths = if kind.contains(CheckoutNotificationType::CONFLICT) {
                        &mut collisions
                    } else {
                        &mut dirty_paths
                    };
                    if paths.len() < 8
                        && let Some(path) = path
                    {
                        paths.push(path.to_string_lossy().into_owned());
                    }
                    true
                });
            if let Some(progress) = progress {
                options.progress(progress);
            }
            repository.checkout_tree(tree.as_object(), Some(&mut options))
        };
        match checkout {
            Ok(()) => checked_out = true,
            Err(error) => {
                // SAFE without allow_conflicts stops before writes for these
                // preflight failures. An I/O failure may have applied a subset.
                checked_out = !matches!(
                    error.code(),
                    ErrorCode::Conflict
                        | ErrorCode::Uncommitted
                        | ErrorCode::IndexDirty
                        | ErrorCode::Unmerged
                );
                let mut error = map_git_error(error);
                if collisions.is_empty() {
                    collisions = dirty_paths;
                }
                if error.kind == CheckoutErrorKind::DirtyWorkingTree && !collisions.is_empty() {
                    error
                        .message
                        .push_str(" 请先提交、储藏或备份这些文件后重试：");
                    error.message.push_str(&collisions.join("、"));
                }
                return Err(error);
            }
        }
        let signature =
            git2::Signature::now("GitViews", "gitviews@localhost").map_err(map_git_error)?;
        transaction
            .set_symbolic_target(
                "HEAD",
                &target.reference,
                Some(&signature),
                "gitviews: checkout branch",
            )
            .map_err(map_git_error)?;
        transaction.commit().map_err(map_git_error)?;
        // includeIf.onbranch is evaluated against the new HEAD. Reopen only
        // after it has changed so conditional config cannot redirect tracking.
        let reopened = Repository::open(&canonical).map_err(map_git_error)?;
        let head = reopened.find_reference("HEAD").map_err(map_git_error)?;
        if head.symbolic_target().map_err(map_git_error)? != Some(target.reference.as_str())
            || head.resolve().map_err(map_git_error)?.target() != Some(target.oid)
        {
            return Err(concurrent_change());
        }
        ensure_upstream(&reopened, &target)?;
        // The real index stays locked and untouched until all HEAD/config
        // validation has passed. No fallible repository reads follow commit.
        index_transaction.commit()
    })();
    // transaction.commit consumes the transaction on success and on error;
    // an error before commit drops it as the closure unwinds, releasing locks.
    if let Err(error) = result {
        let mut failed = false;
        if checked_out {
            failed |= restore_checkout(&repository, &original, &target, &original_tree).is_err();
        }
        if created {
            failed |= remove_created_branch(&repository, &canonical, &target).is_err();
        }
        if let Some(config) = &mut config {
            failed |= config.restore().is_err();
        }
        if failed {
            return Err(CheckoutError::new(
                CheckoutErrorKind::RollbackFailed,
                "切换失败，且无法完整恢复先前状态；已保留现有文件，请检查 HEAD、工作目录及分支配置后再继续。",
            ));
        }
        return Err(error);
    }
    Ok(CheckoutOutcome {
        branch: target.name,
        head: target.oid.to_string(),
        created,
        upstream: target.upstream,
        summary: if created {
            "已创建本地跟踪分支并安全切换。"
        } else {
            "已安全切换本地分支。"
        }
        .into(),
    })
}

fn expected_head(
    repository: &Repository,
    request: &CheckoutRequest,
) -> Result<HeadState, CheckoutError> {
    if let Err(error) = repository.head() {
        if error.code() == ErrorCode::UnbornBranch {
            return Err(CheckoutError::new(
                CheckoutErrorKind::UnbornHead,
                "当前分支尚无提交；请先建立初始提交，再切换分支。",
            ));
        }
        return Err(map_git_error(error));
    }
    let head = repository.find_reference("HEAD").map_err(map_git_error)?;
    let reference = head
        .symbolic_target()
        .map_err(map_git_error)?
        .map(str::to_owned);
    let branch = reference
        .as_deref()
        .and_then(|name| name.strip_prefix("refs/heads/"));
    let expected_branch = request
        .expected_branch
        .as_deref()
        .map(|name| name.strip_prefix("refs/heads/").unwrap_or(name));
    let expected = request
        .expected_head
        .as_deref()
        .filter(|id| id.len() == 40)
        .and_then(|id| Oid::from_str(id).ok())
        .ok_or_else(|| {
            CheckoutError::new(
                CheckoutErrorKind::InvalidRequest,
                "缺少切换开始时的完整 HEAD，请刷新后重试。",
            )
        })?;
    let oid = head
        .resolve()
        .map_err(map_git_error)?
        .target()
        .ok_or_else(concurrent_change)?;
    if branch != expected_branch || oid != expected || (reference.is_some() && branch.is_none()) {
        return Err(concurrent_change());
    }
    Ok(HeadState { reference, oid })
}

fn target(repository: &Repository, request: &CheckoutRequest) -> Result<Target, CheckoutError> {
    let branch = request.branch.as_str();
    if branch.contains('\0') {
        return Err(invalid_request());
    }
    if !request.remote {
        let name = branch.strip_prefix("refs/heads/").unwrap_or(branch);
        validate_name(name)?;
        let branch = repository
            .find_branch(name, BranchType::Local)
            .map_err(map_git_error)?;
        let oid = branch.get().target().ok_or_else(invalid_request)?;
        let upstream = branch
            .upstream()
            .ok()
            .and_then(|branch| branch.name().ok().flatten().map(str::to_owned));
        return Ok(Target {
            name: name.into(),
            reference: format!("refs/heads/{name}"),
            oid,
            create: false,
            upstream,
            remote: None,
            tracking_oid: None,
        });
    }
    let short = branch.strip_prefix("refs/remotes/").unwrap_or(branch);
    let tracking = format!("refs/remotes/{short}");
    if !Reference::is_valid_name(&tracking) {
        return Err(invalid_request());
    }
    let reference = repository
        .find_reference(&tracking)
        .map_err(map_git_error)?;
    if reference
        .symbolic_target()
        .map_err(map_git_error)?
        .is_some()
    {
        return Err(invalid_request());
    }
    let oid = reference.target().ok_or_else(invalid_request)?;
    let remote_name = repository
        .branch_remote_name(&tracking)
        .map_err(map_git_error)?;
    let remote_name = remote_name.as_str().map_err(map_git_error)?;
    let name = short
        .strip_prefix(remote_name)
        .and_then(|name| name.strip_prefix('/'))
        .ok_or_else(invalid_request)?;
    validate_name(name)?;
    let remote = repository.find_remote(remote_name).map_err(map_git_error)?;
    let mut merge = None;
    for spec in remote.refspecs() {
        if spec.direction() == Direction::Fetch && spec.dst_matches(&tracking) {
            let transformed = spec.rtransform(&tracking).map_err(map_git_error)?;
            merge = transformed
                .as_str()
                .ok()
                .filter(|name| name.starts_with("refs/heads/"))
                .map(str::to_owned);
            if merge.is_some() {
                break;
            }
        }
    }
    let merge = merge.ok_or_else(|| {
        CheckoutError::new(
            CheckoutErrorKind::InvalidRequest,
            "远端分支没有明确的获取映射，无法安全设置跟踪分支。",
        )
    })?;
    let (create, local_oid) = match repository.find_branch(name, BranchType::Local) {
        Ok(local) => {
            if local
                .upstream()
                .ok()
                .and_then(|upstream| upstream.get().name().ok().map(str::to_owned))
                != Some(tracking.clone())
            {
                return Err(CheckoutError::new(
                    CheckoutErrorKind::BranchConflict,
                    "同名本地分支已存在，且未跟踪所选远端分支；不会覆盖或重设该分支。",
                ));
            }
            (false, local.get().target().ok_or_else(invalid_request)?)
        }
        Err(error) if error.code() == ErrorCode::NotFound => (true, oid),
        Err(error) => return Err(map_git_error(error)),
    };
    Ok(Target {
        name: name.into(),
        reference: format!("refs/heads/{name}"),
        oid: local_oid,
        create,
        upstream: Some(short.into()),
        remote: Some((remote_name.into(), merge, tracking)),
        tracking_oid: Some(oid),
    })
}

fn validate_name(name: &str) -> Result<(), CheckoutError> {
    if name.starts_with("refs/") || !git2::Branch::name_is_valid(name).map_err(map_git_error)? {
        return Err(invalid_request());
    }
    Ok(())
}

fn ensure_target(repository: &Repository, target: &Target) -> Result<(), CheckoutError> {
    if let Some((_, _, tracking)) = &target.remote {
        let reference = repository.find_reference(tracking).map_err(map_git_error)?;
        if reference
            .symbolic_target()
            .map_err(map_git_error)?
            .is_some()
            || reference.target() != target.tracking_oid
        {
            return Err(concurrent_change());
        }
    }
    if target.create {
        match repository.find_reference(&target.reference) {
            Err(error) if error.code() == ErrorCode::NotFound => {}
            _ => return Err(concurrent_change()),
        }
    } else if repository
        .refname_to_id(&target.reference)
        .map_err(map_git_error)?
        != target.oid
    {
        return Err(concurrent_change());
    }
    Ok(())
}

fn ensure_upstream(repository: &Repository, target: &Target) -> Result<(), CheckoutError> {
    if let Some((_, _, tracking)) = &target.remote {
        let branch = repository
            .find_branch(&target.name, BranchType::Local)
            .map_err(map_git_error)?;
        if branch
            .upstream()
            .ok()
            .and_then(|upstream| upstream.get().name().ok().map(str::to_owned))
            != Some(tracking.clone())
        {
            return Err(CheckoutError::new(
                CheckoutErrorKind::BranchConflict,
                "该分支存在冲突的跟踪配置，未保留本次切换。",
            ));
        }
    }
    Ok(())
}

fn require_checkout_ready(repository: &Repository) -> Result<(), CheckoutError> {
    if repository.state() != RepositoryState::Clean {
        return Err(CheckoutError::new(
            CheckoutErrorKind::InProgress,
            "仓库正在合并、变基或执行其他未完成操作；请先完成或中止。",
        ));
    }
    let mut index = repository.index().map_err(map_git_error)?;
    index.read(true).map_err(map_git_error)?;
    if index.has_conflicts() {
        return Err(CheckoutError::new(
            CheckoutErrorKind::DirtyWorkingTree,
            "暂存区存在冲突；请先解决冲突后再切换。",
        ));
    }
    // SAFE checkout protects normal local edits and untracked collisions. These
    // index flags can hide edited files from its filesystem checks, so require
    // users to expose them before switching instead of risking hidden data.
    if index
        .iter()
        .any(|entry| entry.flags & 0x8000 != 0 || entry.flags_extended & 0x4000 != 0)
    {
        return Err(CheckoutError::new(
            CheckoutErrorKind::DirtyWorkingTree,
            "暂存区包含 assume-unchanged 或 skip-worktree 文件；请先取消这些标记并检查本地修改。",
        ));
    }
    Ok(())
}

fn require_nonoverlapping_changes(
    repository: &Repository,
    original: &git2::Tree<'_>,
    target: &git2::Tree<'_>,
) -> Result<(), CheckoutError> {
    let target_changes = repository
        .diff_tree_to_tree(Some(original), Some(target), None)
        .map_err(map_git_error)?;
    let ignore_case = cfg!(windows)
        || repository
            .config()
            .ok()
            .and_then(|config| config.get_bool("core.ignorecase").ok())
            .unwrap_or(false);
    let target_paths = target_changes
        .deltas()
        .flat_map(|delta| {
            [
                delta.old_file().path().map(Path::to_owned),
                delta.new_file().path().map(Path::to_owned),
            ]
        })
        .flatten()
        .filter(|path| !path.as_os_str().is_empty())
        .collect::<BTreeSet<_>>();
    let changed_paths = target_paths
        .iter()
        .map(|path| path_key(path, ignore_case))
        .collect::<BTreeSet<_>>();
    if changed_paths.is_empty() {
        return Ok(());
    }
    let mut index = repository.index().map_err(map_git_error)?;
    index.read(true).map_err(map_git_error)?;
    let mut options = DiffOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(true)
        .include_typechange(true)
        .include_typechange_trees(true)
        .skip_binary_check(true)
        .disable_pathspec_match(true);
    // Only inspect changed paths and their directory ancestors. Exact (not
    // glob) pathspecs also handle literal wildcard characters in filenames.
    // Ancestors expose file/directory obstructions; ignored directories remain
    // shallow, rather than traversing unrelated dependency/build artifacts.
    let pathspecs = target_paths
        .iter()
        .flat_map(|path| path.ancestors())
        .filter(|path| !path.as_os_str().is_empty())
        .collect::<BTreeSet<_>>();
    for path in pathspecs {
        options.pathspec(path);
    }
    let staged = repository
        .diff_tree_to_index(Some(original), Some(&index), Some(&mut options))
        .map_err(map_git_error)?;
    let unstaged = repository
        .diff_index_to_workdir(Some(&index), Some(&mut options))
        .map_err(map_git_error)?;
    let mut overlaps = Vec::new();
    // SAFE permits local files already equal to the target (including deleted
    // files). A subsequent rollback could then mistake those original edits
    // for clean target files. Keep both sides of each staged/unstaged delta and
    // refuse any target overlap, while allowing unrelated edits to survive.
    for delta in staged.deltas().chain(unstaged.deltas()) {
        for path in [delta.old_file().path(), delta.new_file().path()]
            .into_iter()
            .flatten()
        {
            if paths_overlap(&changed_paths, &path_key(path, ignore_case)) {
                let path = path.to_string_lossy().into_owned();
                if !overlaps.contains(&path) {
                    overlaps.push(path);
                }
            }
            if overlaps.len() == 8 {
                break;
            }
        }
        if overlaps.len() == 8 {
            break;
        }
    }
    if !overlaps.is_empty() {
        return Err(CheckoutError::new(
            CheckoutErrorKind::DirtyWorkingTree,
            format!(
                "本地改动与目标分支要更新的路径重叠；请先提交、储藏或备份这些文件后重试：{}",
                overlaps.join("、")
            ),
        ));
    }
    Ok(())
}

fn path_key(path: &Path, ignore_case: bool) -> String {
    let mut key = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if ignore_case {
        key.make_ascii_lowercase();
    }
    key
}

fn paths_overlap(changed: &BTreeSet<String>, path: &str) -> bool {
    if changed.contains(path) {
        return true;
    }
    let mut ancestor = path;
    while let Some((parent, _)) = ancestor.rsplit_once('/') {
        if changed.contains(parent) {
            return true;
        }
        ancestor = parent;
    }
    let prefix = format!("{path}/");
    changed
        .range(prefix.clone()..)
        .next()
        .is_some_and(|changed| changed.starts_with(&prefix))
}

fn ensure_available(
    repository: &Repository,
    current: &Path,
    target: &str,
) -> Result<(), CheckoutError> {
    let main = Repository::open(repository.commondir()).map_err(map_git_error)?;
    if !main.is_bare() && another_worktree_uses(&main, current, target)? {
        return Err(branch_in_use());
    }
    for name in repository.worktrees().map_err(map_git_error)?.iter() {
        let name = name.map_err(map_git_error)?.ok_or_else(invalid_request)?;
        let worktree = repository.find_worktree(name).map_err(map_git_error)?;
        // Missing registrations still reserve their branch. Read their own
        // gitdir HEAD rather than discovering an ancestor through the workdir.
        let metadata = repository.commondir().join("worktrees").join(name);
        if fs::canonicalize(worktree.path()).ok().as_deref() == Some(current) {
            continue;
        }
        let head = fs::read(metadata.join("HEAD")).map_err(|_| {
            CheckoutError::new(
                CheckoutErrorKind::BranchInUse,
                "无法确认其他工作树的分支占用状态，请先检查工作树注册。",
            )
        })?;
        if head
            .strip_prefix(b"ref: ")
            .is_some_and(|name| name.trim_ascii() == target.as_bytes())
        {
            return Err(branch_in_use());
        }
    }
    Ok(())
}

fn another_worktree_uses(
    repository: &Repository,
    current: &Path,
    target: &str,
) -> Result<bool, CheckoutError> {
    if repository
        .workdir()
        .and_then(|path| fs::canonicalize(path).ok())
        .as_deref()
        == Some(current)
    {
        return Ok(false);
    }
    Ok(repository
        .find_reference("HEAD")
        .map_err(map_git_error)?
        .symbolic_target()
        .map_err(map_git_error)?
        == Some(target))
}

fn restore_checkout(
    repository: &Repository,
    original: &HeadState,
    target: &Target,
    old_tree: &git2::Tree<'_>,
) -> Result<(), CheckoutError> {
    let mut transaction = rollback_locks(repository, original, target)?;
    let head = repository.find_reference("HEAD").map_err(map_git_error)?;
    let current_reference = head.symbolic_target().map_err(map_git_error)?;
    let current_oid = head
        .resolve()
        .map_err(map_git_error)?
        .target()
        .ok_or_else(concurrent_change)?;
    let original_identity =
        current_reference == original.reference.as_deref() && current_oid == original.oid;
    let target_identity =
        current_reference == Some(target.reference.as_str()) && current_oid == target.oid;
    if !original_identity && !target_identity {
        return Err(concurrent_change());
    }
    // SAFE rollback needs the target tree as its baseline. git2's safe wrapper
    // exposes no baseline setter, so temporarily detach at that exact commit.
    // It still refuses any intervening user edits; no force checkout is used.
    let signature =
        git2::Signature::now("GitViews", "gitviews@localhost").map_err(map_git_error)?;
    transaction
        .set_target(
            "HEAD",
            target.oid,
            Some(&signature),
            "gitviews: prepare checkout recovery",
        )
        .map_err(map_git_error)?;
    transaction.commit().map_err(map_git_error)?;
    let mut transaction = rollback_locks(repository, original, target)?;
    let head = repository.find_reference("HEAD").map_err(map_git_error)?;
    if head.symbolic_target().map_err(map_git_error)?.is_some() || head.target() != Some(target.oid)
    {
        return Err(concurrent_change());
    }
    let mut options = CheckoutBuilder::new();
    options.safe().overwrite_ignored(false);
    let restored = repository.checkout_tree(old_tree.as_object(), Some(&mut options));
    if let Some(reference) = &original.reference {
        transaction
            .set_symbolic_target(
                "HEAD",
                reference,
                Some(&signature),
                "gitviews: restore checkout",
            )
            .map_err(map_git_error)?;
    } else {
        transaction
            .set_target(
                "HEAD",
                original.oid,
                Some(&signature),
                "gitviews: restore checkout",
            )
            .map_err(map_git_error)?;
    }
    transaction.commit().map_err(map_git_error)?;
    restored.map_err(map_git_error)?;
    require_checkout_ready(repository)
}

fn rollback_locks<'a>(
    repository: &'a Repository,
    original: &HeadState,
    target: &Target,
) -> Result<git2::Transaction<'a>, CheckoutError> {
    let mut transaction = repository.transaction().map_err(map_git_error)?;
    transaction.lock_ref("HEAD").map_err(map_git_error)?;
    transaction
        .lock_ref(&target.reference)
        .map_err(map_git_error)?;
    if let Some(reference) = &original.reference {
        if reference != &target.reference {
            transaction.lock_ref(reference).map_err(map_git_error)?;
        }
        if repository.refname_to_id(reference).map_err(map_git_error)? != original.oid {
            return Err(concurrent_change());
        }
    }
    if repository
        .refname_to_id(&target.reference)
        .map_err(map_git_error)?
        != target.oid
    {
        return Err(concurrent_change());
    }
    Ok(transaction)
}

fn remove_created_branch(
    repository: &Repository,
    current: &Path,
    target: &Target,
) -> Result<(), CheckoutError> {
    ensure_available(repository, current, &target.reference)?;
    if repository
        .find_reference("HEAD")
        .map_err(map_git_error)?
        .symbolic_target()
        .map_err(map_git_error)?
        == Some(target.reference.as_str())
    {
        return Err(concurrent_change());
    }
    let mut transaction = repository.transaction().map_err(map_git_error)?;
    transaction
        .lock_ref(&target.reference)
        .map_err(map_git_error)?;
    if repository
        .refname_to_id(&target.reference)
        .map_err(map_git_error)?
        != target.oid
    {
        return Err(concurrent_change());
    }
    transaction
        .remove(&target.reference)
        .map_err(map_git_error)?;
    transaction.commit().map_err(map_git_error)
}

static NEXT_CONFIG: AtomicU64 = AtomicU64::new(0);

struct ConfigEdit {
    path: PathBuf,
    lock: PathBuf,
    marker: Vec<u8>,
    staging: PathBuf,
    original: Vec<u8>,
    updated: Vec<u8>,
    permissions: fs::Permissions,
    applied: bool,
}

impl ConfigEdit {
    fn prepare(
        repository: &Repository,
        branch: &str,
        remote: &str,
        merge: &str,
    ) -> Result<Self, CheckoutError> {
        let path = repository.commondir().join("config");
        let metadata = fs::symlink_metadata(&path).map_err(|_| config_error())?;
        if !metadata.is_file() || metadata.permissions().readonly() {
            return Err(config_error());
        }
        let lock = path.with_extension("lock");
        let marker = format!(
            "gitviews-checkout-{}-{}",
            std::process::id(),
            NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
        )
        .into_bytes();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .map_err(|_| concurrent_change())?;
        if file.write_all(&marker).is_err() {
            drop(file);
            let _ = fs::remove_file(&lock);
            return Err(config_error());
        }
        drop(file);
        let staging = repository.commondir().join(format!(
            ".gitviews-checkout-{}-{}.tmp",
            std::process::id(),
            NEXT_CONFIG.fetch_add(1, Ordering::Relaxed)
        ));
        let mut edit = Self {
            path,
            lock,
            marker,
            staging,
            original: Vec::new(),
            updated: Vec::new(),
            permissions: metadata.permissions(),
            applied: false,
        };
        edit.original = fs::read(&edit.path).map_err(|_| config_error())?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&edit.staging)
            .map_err(|_| config_error())?;
        file.write_all(&edit.original).map_err(|_| config_error())?;
        drop(file);
        let mut config = Config::open(&edit.staging).map_err(map_git_error)?;
        config
            .set_str(&format!("branch.{branch}.remote"), remote)
            .map_err(map_git_error)?;
        config
            .set_str(&format!("branch.{branch}.merge"), merge)
            .map_err(map_git_error)?;
        drop(config);
        edit.updated = fs::read(&edit.staging).map_err(|_| config_error())?;
        fs::set_permissions(&edit.staging, edit.permissions.clone()).map_err(|_| config_error())?;
        OpenOptions::new()
            .write(true)
            .open(&edit.staging)
            .and_then(|file| file.sync_all())
            .map_err(|_| config_error())?;
        Ok(edit)
    }

    fn apply(&mut self) -> Result<(), CheckoutError> {
        if fs::read(&self.path).map_err(|_| config_error())? != self.original {
            return Err(concurrent_change());
        }
        fs::rename(&self.staging, &self.path).map_err(|_| config_error())?;
        self.applied = true;
        Ok(())
    }

    fn restore(&mut self) -> Result<(), CheckoutError> {
        if !self.applied {
            return Ok(());
        }
        if fs::read(&self.path).map_err(|_| config_error())? != self.updated {
            return Err(concurrent_change());
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.staging)
            .map_err(|_| config_error())?;
        file.write_all(&self.original).map_err(|_| config_error())?;
        file.sync_all().map_err(|_| config_error())?;
        drop(file);
        fs::set_permissions(&self.staging, self.permissions.clone()).map_err(|_| config_error())?;
        fs::rename(&self.staging, &self.path).map_err(|_| config_error())?;
        self.applied = false;
        Ok(())
    }
}

impl Drop for ConfigEdit {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.staging);
        if fs::read(&self.lock).ok().as_deref() == Some(self.marker.as_slice()) {
            let _ = fs::remove_file(&self.lock);
        }
    }
}

fn invalid_repository() -> CheckoutError {
    CheckoutError::new(
        CheckoutErrorKind::InvalidRepository,
        "所选路径不是可用的非裸仓库根目录。",
    )
}
fn invalid_request() -> CheckoutError {
    CheckoutError::new(
        CheckoutErrorKind::InvalidRequest,
        "分支名称或远端引用无效，未执行切换。",
    )
}
fn concurrent_change() -> CheckoutError {
    CheckoutError::new(
        CheckoutErrorKind::ConcurrentChange,
        "HEAD、暂存区、分支引用或配置在切换前发生变化或被占用；请刷新后重试。",
    )
}
fn branch_in_use() -> CheckoutError {
    CheckoutError::new(
        CheckoutErrorKind::BranchInUse,
        "目标分支正在其他工作树中使用，不能在当前工作树重复检出。",
    )
}
fn config_error() -> CheckoutError {
    CheckoutError::new(
        CheckoutErrorKind::CheckoutFailed,
        "无法安全更新分支跟踪配置；未覆盖现有配置。",
    )
}
fn map_git_error(error: git2::Error) -> CheckoutError {
    let (kind, message) = match error.code() {
        ErrorCode::NotFound => (
            CheckoutErrorKind::NotFound,
            "目标分支或提交不存在，请刷新后重试。",
        ),
        ErrorCode::Locked | ErrorCode::Modified => return concurrent_change(),
        ErrorCode::Conflict
        | ErrorCode::Uncommitted
        | ErrorCode::IndexDirty
        | ErrorCode::Unmerged => (
            CheckoutErrorKind::DirtyWorkingTree,
            "切换会覆盖工作目录中的文件或修改，已停止安全检出。",
        ),
        _ => (
            CheckoutErrorKind::CheckoutFailed,
            "原生分支切换失败；请检查仓库与文件权限后重试。",
        ),
    };
    CheckoutError::new(kind, message)
}
