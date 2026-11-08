//! Native reference management. Working files and the index are never changed.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt, fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use git2::{BranchType, Config, ErrorCode, Oid, Reference, Repository, Signature};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BranchAction {
    Create {
        name: String,
        start_point: Option<String>,
    },
    Rename {
        old: String,
        new: String,
    },
    Delete {
        name: String,
        force: bool,
    },
    SetUpstream {
        name: String,
        upstream: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub struct BranchRequest {
    /// Full commit ID observed by the caller. None means an unborn HEAD.
    pub expected_head: Option<String>,
    pub action: BranchAction,
}

#[derive(Clone, Debug)]
pub struct BranchOutcome {
    pub summary: String,
    /// Deleted/renamed tips remain reachable here, independently of reflog expiry.
    pub recovery_reference: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BranchErrorKind {
    Unsupported,
    InProgress,
    InvalidRepository,
    InvalidRequest,
    ConcurrentChange,
    BranchInUse,
    NotFound,
    AlreadyExists,
    NotMerged,
    Configuration,
    Failed,
}

#[derive(Clone, Debug)]
pub struct BranchError {
    pub kind: BranchErrorKind,
    pub message: String,
}

impl BranchError {
    pub fn new(kind: BranchErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}
impl fmt::Display for BranchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl Error for BranchError {}

static NEXT: AtomicU64 = AtomicU64::new(0);

pub(super) fn execute(root: &Path, request: &BranchRequest) -> Result<BranchOutcome, BranchError> {
    let canonical = fs::canonicalize(root).map_err(|_| invalid_repository())?;
    let repository = Repository::open(&canonical).map_err(|_| invalid_repository())?;
    if repository.is_bare()
        || repository
            .workdir()
            .and_then(|p| fs::canonicalize(p).ok())
            .as_ref()
            != Some(&canonical)
    {
        return Err(invalid_repository());
    }
    check_head(&repository, request)?;
    require_ready(&repository)?;
    // Honor Git's HEAD locks for every existing registration, including offline worktrees.
    let _heads = HeadLocks::acquire(&repository)?;
    check_head(&repository, request)?;
    require_ready(&repository)?;
    let signature = repository
        .signature()
        .or_else(|_| Signature::now("GitViews", "gitviews@localhost"))
        .map_err(map_git)?;
    let mut transaction = repository.transaction().map_err(map_git)?;
    let mut recovery = None;
    let mut config_edit = None;
    let mut ref_plan = Vec::new();
    let summary = match &request.action {
        BranchAction::Create { name, start_point } => {
            let target = local_ref(name)?;
            transaction.lock_ref(&target).map_err(map_git)?;
            require_absent(&repository, &target)?;
            // An unborn worktree already reserves its symbolic HEAD name.
            require_unused(&repository, &target)?;
            let source = start_point.as_deref().unwrap_or("HEAD");
            if source.trim() != source || source.is_empty() || source.contains('\0') {
                return Err(invalid_request());
            }
            let commit = repository
                .revparse_single(source)
                .and_then(|object| object.peel_to_commit())
                .map_err(|error| {
                    if start_point.is_none() && request.expected_head.is_none() {
                        BranchError::new(
                            BranchErrorKind::InvalidRequest,
                            "当前分支尚无提交，请填写已有分支、标签或提交作为起点。",
                        )
                    } else {
                        map_git(error)
                    }
                })?;
            repository.reference_ensure_log(&target).map_err(map_git)?;
            transaction
                .set_target(
                    &target,
                    commit.id(),
                    Some(&signature),
                    &format!("branch: Created from {source}"),
                )
                .map_err(map_git)?;
            ref_plan.push(RefEdit {
                name: target,
                before: None,
                after: Some(commit.id()),
            });
            format!("已创建分支 {name}；当前检出分支保持不变。")
        }
        BranchAction::Rename { old, new } => {
            let source = local_ref(old)?;
            let target = local_ref(new)?;
            if source == target {
                return Err(invalid_request());
            }
            lock_refs(&mut transaction, [&source, &target])?;
            let oid = local_oid(&repository, &source)?;
            require_absent(&repository, &target)?;
            require_unused(&repository, &source)?;
            require_local_branch_config(&repository, old)?;
            config_edit = Some(ConfigEdit::prepare(&repository, |config| {
                rename_config(config, old, new)
            })?);
            recovery = Some(retain_tip(&repository, &source, oid, &signature)?);
            repository.reference_ensure_log(&target).map_err(map_git)?;
            let mut log = copied_reflog(&repository, &source, &target)?;
            log.append(
                oid,
                &signature,
                Some(&format!("branch: renamed {old} to {new}")),
            )
            .map_err(map_git)?;
            transaction.set_reflog(&target, log).map_err(map_git)?;
            transaction
                .set_target(
                    &target,
                    oid,
                    Some(&signature),
                    &format!("branch: renamed {old} to {new}"),
                )
                .map_err(map_git)?;
            transaction.remove(&source).map_err(map_git)?;
            ref_plan.push(RefEdit {
                name: source,
                before: Some(oid),
                after: None,
            });
            ref_plan.push(RefEdit {
                name: target,
                before: None,
                after: Some(oid),
            });
            format!("已将分支 {old} 重命名为 {new}，跟踪配置和历史记录已保留。")
        }
        BranchAction::Delete { name, force } => {
            let source = local_ref(name)?;
            transaction.lock_ref(&source).map_err(map_git)?;
            let oid = local_oid(&repository, &source)?;
            require_unused(&repository, &source)?;
            require_local_branch_config(&repository, name)?;
            if !force {
                let head = repository
                    .head()
                    .and_then(|head| head.peel_to_commit())
                    .map_err(map_git)?
                    .id();
                if oid != head && !repository.graph_descendant_of(head, oid).map_err(map_git)? {
                    return Err(BranchError::new(
                        BranchErrorKind::NotMerged,
                        "分支尚未合并到当前 HEAD。请先合并，或明确勾选强制删除并再次确认。",
                    ));
                }
            }
            config_edit = Some(ConfigEdit::prepare(&repository, |config| {
                remove_config(config, name)
            })?);
            // Save first: libgit2's multi-ref commit is not atomic on filesystem failures.
            recovery = Some(retain_tip(&repository, &source, oid, &signature)?);
            transaction.remove(&source).map_err(map_git)?;
            ref_plan.push(RefEdit {
                name: source,
                before: Some(oid),
                after: None,
            });
            format!("已删除分支 {name}，分支提交已保留，可从恢复引用重新创建。")
        }
        BranchAction::SetUpstream { name, upstream } => {
            let source = local_ref(name)?;
            transaction.lock_ref(&source).map_err(map_git)?;
            local_oid(&repository, &source)?;
            require_local_branch_config(&repository, name)?;
            let tracking = upstream
                .as_deref()
                .map(|upstream| resolve_upstream(&repository, upstream))
                .transpose()?;
            if tracking
                .as_ref()
                .is_some_and(|(remote, merge)| remote == "." && merge == &source)
            {
                return Err(invalid_request());
            }
            config_edit = Some(ConfigEdit::prepare(&repository, |config| {
                branch_entries(config, name)?;
                for suffix in ["remote", "merge"] {
                    remove_key(config, &format!("branch.{name}.{suffix}"))?;
                }
                if let Some((remote, merge)) = &tracking {
                    config
                        .set_str(&format!("branch.{name}.remote"), remote)
                        .map_err(map_git)?;
                    config
                        .set_str(&format!("branch.{name}.merge"), merge)
                        .map_err(map_git)?;
                }
                Ok(())
            })?);
            if let Some(upstream) = upstream {
                format!("已将分支 {name} 的上游设置为 {upstream}。")
            } else {
                format!("已取消分支 {name} 的上游。")
            }
        }
    };
    check_head(&repository, request)?;
    if let Some(edit) = &mut config_edit {
        edit.apply()?;
    }
    if let Err(error) = transaction.commit() {
        if rollback_refs(&repository, &ref_plan, &signature).is_err() {
            return Err(BranchError::new(
                BranchErrorKind::ConcurrentChange,
                format!(
                    "分支操作中断，部分引用可能已变化；未覆盖外部改动，请重新读取。原分支提交保留在 {}。",
                    recovery.as_deref().unwrap_or("原有提交对象")
                ),
            ));
        }
        if let Some(edit) = &mut config_edit {
            edit.restore()?;
        }
        return Err(map_git(error));
    }
    Ok(BranchOutcome {
        summary,
        recovery_reference: recovery,
    })
}

struct RefEdit {
    name: String,
    before: Option<Oid>,
    after: Option<Oid>,
}
#[cfg(test)]
#[allow(dead_code)]
pub(super) fn rollback_for_test(
    root: &Path,
    edits: &[(String, Option<Oid>, Option<Oid>)],
) -> Result<(), BranchError> {
    let repository = Repository::open(root).map_err(map_git)?;
    let signature = Signature::now("Fixture", "fixture@example.invalid").map_err(map_git)?;
    rollback_refs(
        &repository,
        &edits
            .iter()
            .map(|(name, before, after)| RefEdit {
                name: name.clone(),
                before: *before,
                after: *after,
            })
            .collect::<Vec<_>>(),
        &signature,
    )
}
fn rollback_refs(
    repository: &Repository,
    edits: &[RefEdit],
    signature: &Signature<'_>,
) -> Result<(), BranchError> {
    let mut transaction = repository.transaction().map_err(map_git)?;
    for name in edits.iter().map(|edit| &edit.name).collect::<BTreeSet<_>>() {
        transaction.lock_ref(name).map_err(map_git)?;
    }
    for edit in edits {
        let current = match repository.find_reference(&edit.name) {
            Ok(reference) => {
                if reference.symbolic_target().map_err(map_git)?.is_some() {
                    return Err(concurrent());
                }
                reference.target()
            }
            Err(error) if error.code() == ErrorCode::NotFound => None,
            Err(error) => return Err(map_git(error)),
        };
        if current == edit.before {
            continue;
        }
        if current != edit.after {
            return Err(concurrent());
        }
        if let Some(oid) = edit.before {
            transaction
                .set_target(
                    &edit.name,
                    oid,
                    Some(signature),
                    "GitViews: rollback interrupted branch operation",
                )
                .map_err(map_git)?;
        } else {
            require_unused(repository, &edit.name)?;
            transaction.remove(&edit.name).map_err(map_git)?;
        }
    }
    transaction.commit().map_err(map_git)
}

fn local_ref(name: &str) -> Result<String, BranchError> {
    if name.is_empty()
        || name.trim() != name
        || name.starts_with('-')
        || name == "HEAD"
        || name.starts_with("refs/")
        || name.contains('\0')
    {
        return Err(invalid_request());
    }
    let reference = format!("refs/heads/{name}");
    if !Reference::is_valid_name(&reference) {
        return Err(invalid_request());
    }
    Ok(reference)
}

fn lock_refs<'a>(
    transaction: &mut git2::Transaction<'_>,
    refs: impl IntoIterator<Item = &'a String>,
) -> Result<(), BranchError> {
    for reference in refs.into_iter().collect::<BTreeSet<_>>() {
        transaction.lock_ref(reference).map_err(map_git)?;
    }
    Ok(())
}

fn check_head(repository: &Repository, request: &BranchRequest) -> Result<(), BranchError> {
    let actual = match repository.head() {
        Ok(head) => Some(head.peel_to_commit().map_err(map_git)?.id().to_string()),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => None,
        Err(error) => return Err(map_git(error)),
    };
    if actual != request.expected_head {
        return Err(concurrent());
    }
    Ok(())
}

fn local_oid(repository: &Repository, reference: &str) -> Result<Oid, BranchError> {
    let found = repository.find_reference(reference).map_err(map_git)?;
    if found.symbolic_target().map_err(map_git)?.is_some() {
        return Err(invalid_request());
    }
    found.target().ok_or_else(invalid_request)
}

fn require_absent(repository: &Repository, reference: &str) -> Result<(), BranchError> {
    match repository.find_reference(reference) {
        Ok(_) => Err(BranchError::new(
            BranchErrorKind::AlreadyExists,
            "目标分支已存在，不会覆盖已有分支。",
        )),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(()),
        Err(error) => Err(map_git(error)),
    }
}

fn head_paths(repository: &Repository) -> Result<BTreeSet<PathBuf>, BranchError> {
    let mut paths = BTreeSet::from([repository.commondir().join("HEAD")]);
    for name in repository.worktrees().map_err(map_git)?.iter() {
        let name = name.map_err(map_git)?.ok_or_else(invalid_request)?;
        paths.insert(
            repository
                .commondir()
                .join("worktrees")
                .join(name)
                .join("HEAD"),
        );
    }
    Ok(paths)
}

fn require_unused(repository: &Repository, reference: &str) -> Result<(), BranchError> {
    for path in head_paths(repository)? {
        let bytes = fs::read(&path).map_err(|_| concurrent())?;
        let mut occupied = bytes
            .strip_prefix(b"ref: ")
            .is_some_and(|name| name.trim_ascii() == reference.as_bytes());
        // Rebase/bisect can detach HEAD while reserving the original branch.
        for marker in [
            "rebase-merge/head-name",
            "rebase-apply/head-name",
            "BISECT_START",
        ] {
            match fs::read(path.parent().ok_or_else(concurrent)?.join(marker)) {
                Ok(bytes) => {
                    occupied |= bytes.trim_ascii() == reference.as_bytes()
                        || bytes.trim_ascii()
                            == reference
                                .strip_prefix("refs/heads/")
                                .unwrap_or(reference)
                                .as_bytes();
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(concurrent()),
            }
        }
        if occupied {
            return Err(BranchError::new(
                BranchErrorKind::BranchInUse,
                "此分支正在当前或其他工作树中使用，不能删除或重命名。请先切换该工作树的分支。",
            ));
        }
    }
    Ok(())
}

fn require_ready(repository: &Repository) -> Result<(), BranchError> {
    if repository.state() != git2::RepositoryState::Clean {
        return Err(BranchError::new(
            BranchErrorKind::InProgress,
            "仓库正在合并、变基或其他 Git 操作中，请完成或中止该操作后再管理分支。",
        ));
    }
    Ok(())
}

struct OwnedLock {
    path: PathBuf,
    marker: Vec<u8>,
}
impl OwnedLock {
    fn acquire(path: PathBuf) -> Result<Self, BranchError> {
        let marker = format!(
            "gitviews-branch-lock-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
        .into_bytes();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| concurrent())?;
        if file
            .write_all(&marker)
            .and_then(|()| file.sync_all())
            .is_err()
        {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(concurrent());
        }
        Ok(Self { path, marker })
    }
}
impl Drop for OwnedLock {
    fn drop(&mut self) {
        if fs::read(&self.path).ok().as_deref() == Some(self.marker.as_slice()) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct HeadLocks(Vec<OwnedLock>);
impl HeadLocks {
    fn acquire(repository: &Repository) -> Result<Self, BranchError> {
        let mut locks = Self(Vec::new());
        for path in head_paths(repository)? {
            locks
                .0
                .push(OwnedLock::acquire(path.with_extension("lock"))?);
        }
        Ok(locks)
    }
}

fn retain_tip(
    repository: &Repository,
    source: &str,
    oid: Oid,
    signature: &Signature<'_>,
) -> Result<String, BranchError> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid_request())?
        .as_nanos();
    let reference = format!(
        "refs/gitviews/deleted-branches/{stamp}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    repository
        .reference_ensure_log(&reference)
        .map_err(map_git)?;
    let mut tx = repository.transaction().map_err(map_git)?;
    tx.lock_ref(&reference).map_err(map_git)?;
    tx.set_target(
        &reference,
        oid,
        Some(signature),
        &format!("GitViews retained {source}"),
    )
    .map_err(map_git)?;
    let mut log = copied_reflog(repository, source, &reference)?;
    log.append(oid, signature, Some(&format!("GitViews retained {source}")))
        .map_err(map_git)?;
    tx.set_reflog(&reference, log).map_err(map_git)?;
    tx.commit().map_err(map_git)?;
    Ok(reference)
}

fn copied_reflog(
    repository: &Repository,
    source: &str,
    target: &str,
) -> Result<git2::Reflog, BranchError> {
    // set_reflog retains the log object's original name; construct a log whose
    // owner is the destination, rather than passing the source log unchanged.
    let source = repository.reflog(source).map_err(map_git)?;
    let mut result = repository.reflog(target).map_err(map_git)?;
    for (index, entry) in source.iter().rev().enumerate() {
        if index == 0 && !entry.id_old().is_zero() {
            result
                .append(
                    entry.id_old(),
                    &entry.committer(),
                    Some("GitViews retained reflog history boundary"),
                )
                .map_err(map_git)?;
        }
        result
            .append(
                entry.id_new(),
                &entry.committer(),
                entry.message().map_err(map_git)?,
            )
            .map_err(map_git)?;
    }
    Ok(result)
}

fn resolve_upstream(
    repository: &Repository,
    upstream: &str,
) -> Result<(String, String), BranchError> {
    if upstream.trim() != upstream || upstream.is_empty() || upstream.contains('\0') {
        return Err(invalid_request());
    }
    let (branch, local) = if let Some(name) = upstream.strip_prefix("refs/heads/") {
        (repository.find_branch(name, BranchType::Local), true)
    } else if let Some(name) = upstream.strip_prefix("refs/remotes/") {
        (repository.find_branch(name, BranchType::Remote), false)
    } else if let Ok(branch) = repository.find_branch(upstream, BranchType::Remote) {
        (Ok(branch), false)
    } else {
        (repository.find_branch(upstream, BranchType::Local), true)
    };
    let branch = branch.map_err(map_git)?;
    let reference = branch.get().name().map_err(map_git)?;
    if local {
        return Ok((".".into(), reference.into()));
    }
    let remote = repository.branch_remote_name(reference).map_err(map_git)?;
    let remote = remote.as_str().map_err(map_git)?;
    let remote_config = repository.find_remote(remote).map_err(map_git)?;
    // Respect custom fetch refspecs; never infer origin/<name> by stripping a slash.
    for spec in remote_config.refspecs() {
        if spec.direction() == git2::Direction::Fetch && spec.dst_matches(reference) {
            let source = spec.rtransform(reference).map_err(map_git)?;
            return Ok((remote.into(), source.as_str().map_err(map_git)?.into()));
        }
    }
    Err(BranchError::new(
        BranchErrorKind::InvalidRequest,
        "所选远端分支没有匹配的 fetch 映射，请先检查远端配置。",
    ))
}

fn branch_entries(
    config: &Config,
    branch: &str,
) -> Result<BTreeMap<String, Vec<String>>, BranchError> {
    let prefix = format!("branch.{branch}.");
    let mut result = BTreeMap::<String, Vec<String>>::new();
    let mut entries = config.entries(None).map_err(map_git)?;
    while let Some(entry) = entries.next() {
        let entry = entry.map_err(map_git)?;
        let Some(suffix) = entry.name().ok().and_then(|key| key.strip_prefix(&prefix)) else {
            continue;
        };
        if suffix.contains('.') {
            continue;
        }
        if entry.include_depth() != 0 {
            return Err(config_error(
                "分支配置来自 include 文件，请先将此分支配置迁移到仓库本地配置。",
            ));
        }
        result
            .entry(suffix.into())
            .or_default()
            .push(entry.value().unwrap_or("").into());
    }
    Ok(result)
}

fn require_local_branch_config(repository: &Repository, branch: &str) -> Result<(), BranchError> {
    let config = repository.config().map_err(map_git)?;
    let prefix = format!("branch.{branch}.");
    let mut entries = config.entries(None).map_err(map_git)?;
    while let Some(entry) = entries.next() {
        let entry = entry.map_err(map_git)?;
        if entry
            .name()
            .ok()
            .and_then(|key| key.strip_prefix(&prefix))
            .is_some_and(|suffix| !suffix.contains('.'))
            && entry.level() != git2::ConfigLevel::Local
        {
            return Err(config_error(
                "此分支存在仓库本地配置以外的设置，请先将其迁移到仓库本地配置，避免重命名或取消上游后继续继承旧设置。",
            ));
        }
    }
    Ok(())
}

fn remove_key(config: &mut Config, key: &str) -> Result<(), BranchError> {
    match config.remove_multivar(key, ".*") {
        Ok(()) => Ok(()),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(()),
        Err(error) => Err(map_git(error)),
    }
}
fn remove_config(config: &mut Config, name: &str) -> Result<(), BranchError> {
    for suffix in branch_entries(config, name)?.keys() {
        remove_key(config, &format!("branch.{name}.{suffix}"))?;
    }
    Ok(())
}
fn rename_config(config: &mut Config, old: &str, new: &str) -> Result<(), BranchError> {
    if !branch_entries(config, new)?.is_empty() {
        return Err(config_error(
            "新分支名称已有残留配置，请清理后重试，避免覆盖跟踪信息。",
        ));
    }
    let entries = branch_entries(config, old)?;
    remove_config(config, old)?;
    for (suffix, values) in entries {
        for (index, value) in values.into_iter().enumerate() {
            let key = format!("branch.{new}.{suffix}");
            if index == 0 {
                config.set_str(&key, &value).map_err(map_git)?;
            } else {
                config.set_multivar(&key, "a^", &value).map_err(map_git)?;
            }
        }
    }
    Ok(())
}

struct ConfigEdit {
    path: PathBuf,
    _lock: OwnedLock,
    staging: PathBuf,
    original: Vec<u8>,
    updated: Vec<u8>,
    applied: bool,
    permissions: fs::Permissions,
}
impl ConfigEdit {
    fn prepare(
        repository: &Repository,
        update: impl FnOnce(&mut Config) -> Result<(), BranchError>,
    ) -> Result<Self, BranchError> {
        let path = repository.commondir().join("config");
        let metadata =
            fs::symlink_metadata(&path).map_err(|_| config_error("无法读取仓库配置。"))?;
        if !metadata.is_file() || metadata.permissions().readonly() {
            return Err(config_error("仓库配置不可安全写入。"));
        }
        // Worktree config overrides cannot be renamed by editing the common config.
        for head in head_paths(repository)? {
            let worktree_config = head.with_file_name("config.worktree");
            if worktree_config.exists() {
                let config = Config::open(&worktree_config).map_err(map_git)?;
                let mut entries = config.entries(None).map_err(map_git)?;
                while let Some(entry) = entries.next() {
                    if entry
                        .map_err(map_git)?
                        .name()
                        .is_ok_and(|name| name.starts_with("branch."))
                    {
                        return Err(config_error(
                            "工作树专属配置包含分支设置，请先统一配置后再管理分支。",
                        ));
                    }
                }
            }
        }
        let lock = OwnedLock::acquire(path.with_extension("lock"))?;
        let staging = repository.commondir().join(format!(
            ".gitviews-branches-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut edit = Self {
            path,
            _lock: lock,
            staging,
            original: Vec::new(),
            updated: Vec::new(),
            applied: false,
            permissions: metadata.permissions(),
        };
        edit.original = fs::read(&edit.path).map_err(|_| config_error("无法读取仓库配置。"))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&edit.staging)
            .map_err(|_| config_error("无法准备仓库配置。"))?;
        file.write_all(&edit.original)
            .map_err(|_| config_error("无法准备仓库配置。"))?;
        drop(file);
        let mut config = Config::open(&edit.staging).map_err(map_git)?;
        update(&mut config)?;
        drop(config);
        edit.updated = fs::read(&edit.staging).map_err(|_| config_error("无法准备仓库配置。"))?;
        fs::set_permissions(&edit.staging, metadata.permissions())
            .map_err(|_| config_error("无法保留配置权限。"))?;
        OpenOptions::new()
            .write(true)
            .open(&edit.staging)
            .and_then(|file| file.sync_all())
            .map_err(|_| config_error("无法写入分支配置。"))?;
        Ok(edit)
    }
    fn apply(&mut self) -> Result<(), BranchError> {
        if fs::read(&self.path).map_err(|_| concurrent())? != self.original {
            return Err(concurrent());
        }
        fs::rename(&self.staging, &self.path).map_err(|_| config_error("更新分支配置失败。"))?;
        self.applied = true;
        Ok(())
    }
    fn restore(&mut self) -> Result<(), BranchError> {
        if self.applied {
            if fs::read(&self.path).map_err(|_| concurrent())? != self.updated {
                return Err(concurrent());
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.staging)
                .map_err(|_| config_error("分支配置恢复失败，请检查仓库配置。"))?;
            file.write_all(&self.original)
                .and_then(|()| file.sync_all())
                .map_err(|_| config_error("分支配置恢复失败，请检查仓库配置。"))?;
            drop(file);
            fs::set_permissions(&self.staging, self.permissions.clone())
                .and_then(|()| fs::rename(&self.staging, &self.path))
                .map_err(|_| config_error("分支配置恢复失败，请检查仓库配置。"))?;
        }
        Ok(())
    }
}
impl Drop for ConfigEdit {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.staging);
    }
}

fn invalid_repository() -> BranchError {
    BranchError::new(
        BranchErrorKind::InvalidRepository,
        "所选路径不是可用的非裸仓库根目录。",
    )
}
fn invalid_request() -> BranchError {
    BranchError::new(
        BranchErrorKind::InvalidRequest,
        "请填写有效的本地短分支名称或明确的提交起点。",
    )
}
fn concurrent() -> BranchError {
    BranchError::new(
        BranchErrorKind::ConcurrentChange,
        "HEAD、分支或配置已变化或被其他 Git 操作占用，请刷新后重试。",
    )
}
fn config_error(message: &str) -> BranchError {
    BranchError::new(BranchErrorKind::Configuration, message)
}
fn map_git(error: git2::Error) -> BranchError {
    let kind = match error.code() {
        ErrorCode::NotFound | ErrorCode::UnbornBranch => BranchErrorKind::NotFound,
        ErrorCode::Locked | ErrorCode::Modified => return concurrent(),
        ErrorCode::Exists => BranchErrorKind::AlreadyExists,
        ErrorCode::InvalidSpec | ErrorCode::Invalid => BranchErrorKind::InvalidRequest,
        _ => BranchErrorKind::Failed,
    };
    BranchError::new(kind, format!("分支操作失败：{}", error.message()))
}
