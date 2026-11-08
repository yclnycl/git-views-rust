//! Conservative worktree administration implemented with libgit2 and bounded
//! metadata edits. No installed Git executable or forced checkout is used.

use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use git2::{
    BranchType, ErrorCode, Oid, Reference, Repository, RepositoryState, Status, StatusOptions,
    WorktreeAddOptions,
};

use crate::Head;

mod safety;
mod submodules;
use safety::{
    Registration, Workspace, atomic_replace, check_plain_tree, destination, io_error,
    list_registrations, read_small, safe_path, same_path,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorktreeAction {
    Create {
        path: PathBuf,
        branch: String,
        create_branch: bool,
    },
    Remove {
        path: PathBuf,
        /// Explicit consent to discard working files, including staged,
        /// untracked, ignored and submodule changes. Ownership/lock checks
        /// still apply and absorbed submodule Git data is retained.
        force: bool,
    },
    Move {
        path: PathBuf,
        destination: PathBuf,
    },
    Lock {
        path: PathBuf,
        reason: String,
    },
    Unlock {
        path: PathBuf,
    },
    Prune {
        path: PathBuf,
    },
    /// Repair a previously registered worktree moved outside GitViews. The new
    /// directory must still contain its original .git link to this registration.
    Repair {
        path: PathBuf,
        destination: PathBuf,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeRequest {
    pub expected_head: Head,
    pub expected_commit: Option<String>,
    pub action: WorktreeAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeOutcome {
    pub message: String,
    pub previous_path: Option<PathBuf>,
    pub path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorktreeErrorKind {
    InvalidRequest,
    Dirty,
    Locked,
    Busy,
    Changed,
    Io,
    Git,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeError {
    pub kind: WorktreeErrorKind,
    pub message: String,
}

impl WorktreeError {
    pub fn new(kind: WorktreeErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}
impl fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl Error for WorktreeError {}

type Result<T> = std::result::Result<T, WorktreeError>;

/// The service serializes mutations for all worktrees sharing a common repo.
/// A create-new filesystem lease also excludes another GitViews process.
pub(crate) fn execute(root: &Path, request: &WorktreeRequest) -> Result<WorktreeOutcome> {
    let context = Workspace::open(root)?;
    expected_head(&context.repo, request)?;
    require_idle(&context.repo)?;
    let _lease = safety::Lease::acquire(context.common.join("gitviews-worktrees.lock"))?;
    let mut transaction = context.repo.transaction().map_err(git_error)?;
    transaction.lock_ref("HEAD").map_err(git_error)?;
    if let Ok(head) = context.repo.find_reference("HEAD")
        && let Some(branch) = head.symbolic_target().map_err(git_error)?
    {
        transaction.lock_ref(branch).map_err(git_error)?;
    }
    expected_head(&context.repo, request)?;
    match &request.action {
        WorktreeAction::Create {
            path,
            branch,
            create_branch,
        } => create(&context, request, path, branch, *create_branch),
        WorktreeAction::Remove { path, force } => {
            remove(&context, request, path, *force).map_err(|mut error| {
                if !*force && error.kind == WorktreeErrorKind::Dirty {
                    error
                        .message
                        .push_str(" 如确认丢弃这些内容，可勾选“强制移除”后重新确认。");
                }
                error
            })
        }
        WorktreeAction::Move { path, destination } => {
            move_worktree(&context, request, path, destination)
        }
        WorktreeAction::Lock { path, reason } => lock(&context, request, path, reason),
        WorktreeAction::Unlock { path } => unlock(&context, request, path),
        WorktreeAction::Prune { path } => prune(&context, request, path),
        WorktreeAction::Repair { path, destination } => {
            repair(&context, request, path, destination)
        }
    }
}

fn expected_head(repo: &Repository, request: &WorktreeRequest) -> Result<()> {
    let (head, commit) = head_state(repo)?;
    if let Some(commit) = &request.expected_commit
        && (commit.len() != 40 || Oid::from_str(commit).is_err())
    {
        return Err(invalid("缺少有效的完整 HEAD，请刷新后重试。"));
    }
    if head != request.expected_head || commit != request.expected_commit {
        return Err(changed());
    }
    Ok(())
}

fn head_state(repo: &Repository) -> Result<(Head, Option<String>)> {
    let head = repo.find_reference("HEAD").map_err(git_error)?;
    let symbolic = head.symbolic_target().map_err(git_error)?;
    match head.resolve() {
        Ok(resolved) => {
            let id = resolved
                .peel_to_commit()
                .map_err(git_error)?
                .id()
                .to_string();
            let state = match symbolic {
                Some(name) => Head::Branch(
                    name.strip_prefix("refs/heads/")
                        .ok_or_else(|| invalid("HEAD 未指向本地分支。"))?
                        .into(),
                ),
                None => Head::Detached(id.clone()),
            };
            Ok((state, Some(id)))
        }
        Err(error) if error.code() == ErrorCode::NotFound => {
            let name = symbolic
                .and_then(|name| name.strip_prefix("refs/heads/"))
                .ok_or_else(|| invalid("无法读取工作树 HEAD。"))?;
            Ok((Head::Unborn(name.into()), None))
        }
        Err(error) => Err(git_error(error)),
    }
}

fn require_idle(repo: &Repository) -> Result<()> {
    require_idle_inner(repo, false)
}

fn require_idle_inner(repo: &Repository, owned_head_and_index: bool) -> Result<()> {
    if repo.state() != RepositoryState::Clean {
        return Err(WorktreeError::new(
            WorktreeErrorKind::Busy,
            "仓库有正在进行的合并、变基或拣选，请先完成后重试。",
        ));
    }
    let mut paths = vec![
        repo.commondir().join("config.lock"),
        repo.commondir().join("packed-refs.lock"),
        repo.commondir().join("shallow.lock"),
    ];
    if !owned_head_and_index {
        paths.extend([
            repo.path().join("index.lock"),
            repo.path().join("HEAD.lock"),
        ]);
    }
    for path in paths {
        if path.try_exists().map_err(io_error)? {
            return Err(WorktreeError::new(
                WorktreeErrorKind::Busy,
                "Git 正在写入仓库（存在锁文件），请稍后重试。",
            ));
        }
    }
    Ok(())
}

fn require_clean(repo: &Repository) -> Result<()> {
    require_clean_inner(repo, false)
}

fn require_clean_inner(repo: &Repository, owned_head_and_index: bool) -> Result<()> {
    require_clean_for_removal(repo, owned_head_and_index, &[])
}

fn require_clean_for_removal(
    repo: &Repository,
    owned_head_and_index: bool,
    checked_gitlinks: &[PathBuf],
) -> Result<()> {
    require_idle_inner(repo, owned_head_and_index)?;
    let mut index = repo.index().map_err(git_error)?;
    // libgit2 retains an index in the repository. Reload after acquiring our
    // lease so an external stage completed between the two checks is visible.
    index.read(true).map_err(git_error)?;
    if index
        .iter()
        .any(|entry| entry.flags & 0x8000 != 0 || entry.flags_extended & 0x4000 != 0)
    {
        return Err(WorktreeError::new(
            WorktreeErrorKind::Unsupported,
            "工作树使用 assume-unchanged 或 skip-worktree 标记，无法确认全部文件安全；请先取消这些标记再移动或删除。",
        ));
    }
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(true)
        .recurse_ignored_dirs(true)
        .include_unmodified(false)
        .exclude_submodules(false);
    let statuses = repo.statuses(Some(&mut options)).map_err(git_error)?;
    let workdir = repo.workdir().map(safe_path).transpose()?;
    // libgit2 can report absent/uninitialized gitlinks as WT_DELETED. Only
    // suppress workdir status for exact gitlinks checked independently, never
    // staged/index changes. Each initialized child has its own full check.
    let dirty = statuses.iter().any(|entry| {
        let checked = entry
            .path()
            .ok()
            .zip(workdir.as_ref())
            .is_some_and(|(path, root)| {
                checked_gitlinks
                    .iter()
                    .any(|link| same_path(link, &root.join(path)))
            });
        !(checked && (Status::WT_MODIFIED | Status::WT_DELETED).contains(entry.status()))
    });
    if dirty {
        return Err(WorktreeError::new(
            WorktreeErrorKind::Dirty,
            "工作树包含未提交、未跟踪或忽略的文件；请先保留这些文件，再执行移动或删除。",
        ));
    }
    Ok(())
}

fn require_removal_state(
    repo: &Repository,
    owned_head_and_index: bool,
    checked_gitlinks: &[PathBuf],
    force: bool,
) -> Result<()> {
    if force {
        require_idle_inner(repo, owned_head_and_index)
    } else {
        require_clean_for_removal(repo, owned_head_and_index, checked_gitlinks)
    }
}

fn owned_lock_paths(repo: &Repository) -> Result<Vec<PathBuf>> {
    let mut paths = vec![
        repo.path().join("HEAD.lock"),
        repo.path().join("index.lock"),
    ];
    let head = repo.find_reference("HEAD").map_err(git_error)?;
    if let Some(branch) = head.symbolic_target().map_err(git_error)? {
        paths.push(repo.commondir().join(format!("{branch}.lock")));
    }
    paths.iter().map(|path| safe_path(path)).collect()
}

/// Retain all locks honored by ordinary Git checkout/reset/stage writers until
/// the final status checks and filesystem mutation have completed. The caller
/// must drop these before pruning the admin directory containing the locks.
fn target_locks(repo: &Repository) -> Result<(git2::Transaction<'_>, safety::Lease)> {
    require_idle(repo)?;
    let index = safety::Lease::acquire(repo.path().join("index.lock"))?;
    let mut refs = repo.transaction().map_err(git_error)?;
    refs.lock_ref("HEAD").map_err(git_error)?;
    let head = repo.find_reference("HEAD").map_err(git_error)?;
    if let Some(branch) = head.symbolic_target().map_err(git_error)? {
        refs.lock_ref(branch).map_err(git_error)?;
    }
    Ok((refs, index))
}

fn require_no_submodules(repo: &Repository) -> Result<()> {
    if !repo.submodules().map_err(git_error)?.is_empty()
        || repo
            .index()
            .map_err(git_error)?
            .iter()
            .any(|entry| entry.mode == 0o160000)
    {
        return Err(WorktreeError::new(
            WorktreeErrorKind::Unsupported,
            "包含子模块的工作树暂不支持移动；请使用 Git 专门处理子模块。",
        ));
    }
    Ok(())
}

fn registration(context: &Workspace, path: &Path) -> Result<Registration> {
    let path = safe_path(path)?;
    if same_path(&path, &context.main) {
        return Err(invalid("主工作树不能执行此管理操作。"));
    }
    list_registrations(&context.common)?
        .into_iter()
        .find(|registration| same_path(&registration.path, &path))
        .ok_or_else(|| invalid("该路径不是此仓库精确登记的关联工作树，请刷新后重试。"))
}

fn require_not_current(context: &Workspace, registration: &Registration) -> Result<()> {
    if same_path(&context.root, &registration.path) {
        return Err(invalid("不能移动或删除当前工作树；请先切换到其他工作树。"));
    }
    safety::source_boundaries(context, registration)?;
    Ok(())
}

fn unlocked(_context: &Workspace, registration: &Registration) -> Result<()> {
    if locked(registration)? {
        return Err(WorktreeError::new(
            WorktreeErrorKind::Locked,
            "此工作树已锁定，请先解锁后重试。",
        ));
    }
    Ok(())
}

fn locked(registration: &Registration) -> Result<bool> {
    let path = safe_path(&registration.admin.join("locked"))?;
    if !path.try_exists().map_err(io_error)? {
        return Ok(false);
    }
    // git2's lock-reason decoder assumes UTF-8. The lock is an opaque sentinel
    // here, so malformed external reasons cannot panic the application.
    read_small(&path)?;
    Ok(true)
}

fn valid_repository(context: &Workspace, registration: &Registration) -> Result<Repository> {
    registration.validate_links(&context.common, &registration.path)?;
    let repo = Repository::open(&registration.path).map_err(git_error)?;
    if repo.is_bare()
        || !same_path(&safe_path(repo.commondir())?, &context.common)
        || !same_path(&safe_path(repo.path())?, &registration.admin)
        || repo
            .workdir()
            .map(safe_path)
            .transpose()?
            .as_ref()
            .is_none_or(|root| !same_path(root, &registration.path))
    {
        return Err(invalid("工作树与仓库登记信息不一致，未执行操作。"));
    }
    Ok(repo)
}

fn ensure_same(context: &Workspace, original: &Registration) -> Result<()> {
    let current = registration(context, &original.path)?;
    if current != *original {
        return Err(changed());
    }
    Ok(())
}

fn branch_available(context: &Workspace, reference: &str) -> Result<()> {
    let main = Repository::open(&context.common).map_err(git_error)?;
    if main
        .find_reference("HEAD")
        .ok()
        .and_then(|head| head.symbolic_target().ok().flatten().map(str::to_owned))
        .as_deref()
        == Some(reference)
    {
        return Err(invalid(
            "该分支已被主工作树使用，请选择其他本地分支或创建新分支。",
        ));
    }
    for registered in list_registrations(&context.common)? {
        let head = read_small(&registered.admin.join("HEAD"))?;
        if std::str::from_utf8(&head)
            .ok()
            .and_then(|text| text.trim().strip_prefix("ref: "))
            == Some(reference)
        {
            return Err(invalid(
                "该分支已被其他工作树使用（包括缺失或锁定的登记），请先处理对应工作树。",
            ));
        }
    }
    Ok(())
}

fn create(
    context: &Workspace,
    request: &WorktreeRequest,
    path: &Path,
    branch: &str,
    create_branch: bool,
) -> Result<WorktreeOutcome> {
    let path = destination(context, path)?;
    let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    let reference_name = format!("refs/heads/{branch}");
    if branch.is_empty()
        || branch.starts_with("refs/")
        || branch.starts_with('-')
        || branch == "HEAD"
        || !Reference::is_valid_name(&reference_name)
    {
        return Err(invalid("请输入有效的本地分支名称。"));
    }
    branch_available(context, &reference_name)?;
    let existing = match context.repo.find_branch(branch, BranchType::Local) {
        Ok(branch) => Some(branch),
        Err(error) if error.code() == ErrorCode::NotFound => None,
        Err(error) => return Err(git_error(error)),
    };
    if create_branch == existing.is_some() {
        return Err(invalid(if create_branch {
            "同名本地分支已存在；可选择使用现有分支。"
        } else {
            "本地分支不存在；请刷新或选择创建新分支。"
        }));
    }
    if create_branch && request.expected_commit.is_none() {
        return Err(invalid("仓库尚无提交，暂时无法创建关联工作树。"));
    }
    let name = safety::unused_name(&context.common)?;
    expected_head(&context.repo, request)?;
    // HEAD and its branch are already held by this operation's transaction.
    // Checking their .lock sentinels here would mistake our own lease for Git.
    if context.repo.state() != RepositoryState::Clean {
        return Err(WorktreeError::new(
            WorktreeErrorKind::Busy,
            "仓库正在执行其他 Git 操作，请稍后重试。",
        ));
    }
    destination(context, &path)?;
    branch_available(context, &reference_name)?;
    let mut target_guard = context.repo.transaction().map_err(git_error)?;
    let target = if let Some(branch) = existing {
        branch
    } else {
        let oid = Oid::from_str(
            request
                .expected_commit
                .as_deref()
                .ok_or_else(|| invalid("缺少初始提交。"))?,
        )
        .map_err(git_error)?;
        let commit = context.repo.find_commit(oid).map_err(git_error)?;
        context
            .repo
            .branch(branch, &commit, false)
            .map_err(git_error)?
    };
    let target_id = target
        .get()
        .target()
        .ok_or_else(|| invalid("分支未指向有效提交。"))?;
    let reference = target.into_reference();
    let result = (|| {
        target_guard.lock_ref(&reference_name).map_err(git_error)?;
        if context
            .repo
            .refname_to_id(&reference_name)
            .map_err(git_error)?
            != target_id
        {
            return Err(changed());
        }
        expected_head(&context.repo, request)?;
        branch_available(context, &reference_name)?;
        destination(context, &path)?;
        let mut options = WorktreeAddOptions::new();
        options.reference(Some(&reference));
        context
            .repo
            .worktree(&name, &path, Some(&options))
            .map_err(|error| {
                WorktreeError::new(
                    WorktreeErrorKind::Git,
                    format!("创建工作树失败；已保留可能产生的目录与分支，未强制清理。{error}"),
                )
            })?;
        let registered = registration(context, &path)?;
        let created = valid_repository(context, &registered)?;
        if created
            .head()
            .map_err(git_error)?
            .peel_to_commit()
            .map_err(git_error)?
            .id()
            != target_id
        {
            return Err(changed());
        }
        Ok(outcome("已创建关联工作树。", None, Some(path.clone())))
    })();
    // Release the target ref before any rollback. Never remove a branch after
    // a partially created directory/registration or a concurrent ref change.
    drop(target_guard);
    if result.is_err()
        && create_branch
        && !path.try_exists().map_err(io_error)?
        && !context
            .common
            .join("worktrees")
            .join(&name)
            .try_exists()
            .map_err(io_error)?
        && let Ok(mut branch) = context.repo.find_branch(branch, BranchType::Local)
        && branch.get().target() == Some(target_id)
        && branch_available(context, &reference_name).is_ok()
        && branch.delete().is_err()
    {
        return Err(WorktreeError::new(
            WorktreeErrorKind::Git,
            "创建工作树未完成，且无法回滚新建分支；目录未创建，请检查该分支后重试。",
        ));
    }
    result
}

fn remove(
    context: &Workspace,
    request: &WorktreeRequest,
    path: &Path,
    force: bool,
) -> Result<WorktreeOutcome> {
    let registration = registration(context, path)?;
    require_not_current(context, &registration)?;
    unlocked(context, &registration)?;
    let repo = valid_repository(context, &registration)?;
    require_idle(&repo)?;
    let modules = submodules::RemovalModules::inspect(&repo, &registration, force)?;
    let gitlink_roots = modules.gitlink_roots();
    require_removal_state(&repo, false, &gitlink_roots, force)?;
    for module in &modules.repositories {
        require_removal_state(module, false, &gitlink_roots, force)
            .map_err(|error| module_error(module, error))?;
    }
    let module_roots = modules.roots();
    safety::check_removable_tree(&registration.path, &module_roots, force)?;
    check_plain_tree(&registration.admin, false)?;
    let retention = safety::prepare_retention(&context.common, &registration)?;
    let original_head = head_state(&repo)?;
    let parent_locks = target_locks(&repo)?;
    let module_locks: Vec<_> = modules
        .repositories
        .iter()
        .map(|module| target_locks(module).map_err(|error| module_error(module, error)))
        .collect::<Result<_>>()?;
    let owned_module_locks =
        modules
            .repositories
            .iter()
            .try_fold(Vec::new(), |mut paths, module| {
                paths.extend(owned_lock_paths(module)?);
                Ok::<_, WorktreeError>(paths)
            })?;
    expected_head(&context.repo, request)?;
    ensure_same(context, &registration)?;
    unlocked(context, &registration)?;
    require_removal_state(&repo, true, &gitlink_roots, force)?;
    for module in &modules.repositories {
        require_removal_state(module, true, &gitlink_roots, force)
            .map_err(|error| module_error(module, error))?;
    }
    modules.validate(&repo, &registration, force)?;
    safety::check_removable_tree(&registration.path, &module_roots, force)?;
    check_plain_tree(&registration.admin, false)?;
    if let Some(retention) = &retention {
        retention.validate_with_locks(&owned_module_locks)?;
    } else if registration
        .admin
        .join("modules")
        .try_exists()
        .map_err(io_error)?
    {
        return Err(changed());
    }
    registration.validate_links(&context.common, &registration.path)?;
    if head_state(&repo)? != original_head {
        return Err(changed());
    }
    // Metadata is retained until deletion of the exact validated working tree
    // succeeds. A filesystem failure therefore leaves an actionable registration.
    let worktree = context
        .repo
        .find_worktree(&registration.name)
        .map_err(git_error)?;
    // Rust's remover never follows file symlinks or directory junctions. The
    // force-only leaf-link allowance must retain this no-follow deletion path.
    fs::remove_dir_all(&registration.path).map_err(|error| WorktreeError::new(WorktreeErrorKind::Io, format!("删除工作目录 {} 失败，工作树登记已保留；部分已跟踪文件可能已删除，请检查并恢复后再试。{error}", crate::display_path(&registration.path))))?;
    // Their admin files must still exist when transaction and index leases are
    // released. Pruning happens only once the physical worktree is absent.
    drop(module_locks);
    drop(modules);
    drop(parent_locks);
    drop(repo);
    let message = if force {
        "已强制移除关联工作树，分支已保留。"
    } else {
        "已移除干净的关联工作树，分支已保留。"
    };
    if let Some(retention) = retention {
        let archive = retention.archive()?;
        return Ok(outcome(
            &format!(
                "{message}子模块 Git 数据保留在：{}",
                crate::display_path(&archive)
            ),
            Some(registration.path),
            None,
        ));
    }
    // A concurrent submodule initialization can create this store without
    // taking the parent's index lock. Never pass newly arrived module data to
    // libgit2's recursive admin pruning, even after the working tree is gone.
    require_prunable_without_modules(&registration)?;
    if let Err(error) = worktree.prune(None) {
        return Err(WorktreeError::new(
            WorktreeErrorKind::Git,
            format!("工作目录已删除，但登记清理失败；请刷新后对该缺失工作树执行清理。{error}"),
        ));
    }
    Ok(outcome(message, Some(registration.path), None))
}

fn module_error(repo: &Repository, mut error: WorktreeError) -> WorktreeError {
    if let Some(path) = repo.workdir() {
        error.message = format!("子模块 {}：{}", crate::display_path(path), error.message);
    }
    error
}

fn require_prunable_without_modules(registration: &Registration) -> Result<()> {
    let modules = safe_path(&registration.admin.join("modules"))?;
    if modules.try_exists().map_err(io_error)? {
        return Err(WorktreeError::new(
            WorktreeErrorKind::Changed,
            "工作目录已缺失，但检测到新出现的子模块数据；登记和数据已保留，请刷新后重试清理。",
        ));
    }
    Ok(())
}

fn move_worktree(
    context: &Workspace,
    request: &WorktreeRequest,
    path: &Path,
    target: &Path,
) -> Result<WorktreeOutcome> {
    let registration = registration(context, path)?;
    require_not_current(context, &registration)?;
    unlocked(context, &registration)?;
    let target = destination(context, target)?;
    let repo = valid_repository(context, &registration)?;
    require_no_submodules(&repo)?;
    require_clean(&repo)?;
    check_plain_tree(&registration.path, true)?;
    let original_head = head_state(&repo)?;
    let old_dotgit = read_small(&registration.path.join(".git"))?;
    let new_gitdir = safety::path_line(&target.join(".git"), "")?;
    let new_dotgit = safety::path_line(&registration.admin, "gitdir: ")?;
    let _target_locks = target_locks(&repo)?;
    expected_head(&context.repo, request)?;
    ensure_same(context, &registration)?;
    unlocked(context, &registration)?;
    require_clean_inner(&repo, true)?;
    require_no_submodules(&repo)?;
    check_plain_tree(&registration.path, true)?;
    registration.validate_links(&context.common, &registration.path)?;
    if head_state(&repo)? != original_head {
        return Err(changed());
    }
    destination(context, &target)?;
    // std::rename is atomic on a volume. Cross-volume moves fail, leaving the
    // source unchanged; there is deliberately no recursive copy-and-delete.
    fs::rename(&registration.path, &target).map_err(|error| {
        WorktreeError::new(
            WorktreeErrorKind::Io,
            format!("移动失败；请使用同一磁盘上的不存在目录。{error}"),
        )
    })?;
    let result = (|| {
        atomic_replace(&target.join(".git"), &old_dotgit, &new_dotgit)?;
        atomic_replace(
            &registration.admin.join("gitdir"),
            &registration.gitdir,
            &new_gitdir,
        )?;
        let reopened = registration.with_path(target.clone(), new_gitdir.clone());
        valid_repository(context, &reopened)?;
        Ok(())
    })();
    if let Err(error) = result {
        let mut failed = false;
        failed |= safety::restore_if_matches(
            &registration.admin.join("gitdir"),
            &new_gitdir,
            &registration.gitdir,
        )
        .is_err();
        failed |=
            safety::restore_if_matches(&target.join(".git"), &new_dotgit, &old_dotgit).is_err();
        if !registration.path.try_exists().map_err(io_error)? {
            failed |= fs::rename(&target, &registration.path).is_err();
        } else {
            failed = true;
        }
        return Err(if failed {
            WorktreeError::new(
                WorktreeErrorKind::Io,
                "移动失败，且无法完整恢复原路径；文件已保留，请检查新旧路径和工作树链接后再继续。",
            )
        } else {
            error
        });
    }
    Ok(outcome(
        "已移动关联工作树并更新 Git 链接。",
        Some(registration.path),
        Some(target),
    ))
}

fn lock(
    context: &Workspace,
    request: &WorktreeRequest,
    path: &Path,
    reason: &str,
) -> Result<WorktreeOutcome> {
    if reason.len() > 4096
        || reason
            .chars()
            .any(|ch| ch == '\0' || ch == '\r' || ch == '\n')
    {
        return Err(invalid("锁定原因应为不超过 4096 字节的单行文本。"));
    }
    let registration = registration(context, path)?;
    expected_head(&context.repo, request)?;
    ensure_same(context, &registration)?;
    unlocked(context, &registration)?;
    context
        .repo
        .find_worktree(&registration.name)
        .map_err(git_error)?
        .lock((!reason.is_empty()).then_some(reason))
        .map_err(git_error)?;
    Ok(outcome("已锁定关联工作树。", None, Some(registration.path)))
}

fn unlock(context: &Workspace, request: &WorktreeRequest, path: &Path) -> Result<WorktreeOutcome> {
    let registration = registration(context, path)?;
    expected_head(&context.repo, request)?;
    ensure_same(context, &registration)?;
    let worktree = context
        .repo
        .find_worktree(&registration.name)
        .map_err(git_error)?;
    if !locked(&registration)? {
        return Err(invalid("工作树已经解锁，请刷新后重试。"));
    }
    worktree.unlock().map_err(git_error)?;
    Ok(outcome("已解锁关联工作树。", None, Some(registration.path)))
}

fn prune(context: &Workspace, request: &WorktreeRequest, path: &Path) -> Result<WorktreeOutcome> {
    let registration = registration(context, path)?;
    require_not_current(context, &registration)?;
    unlocked(context, &registration)?;
    if registration.path.try_exists().map_err(io_error)? {
        return Err(invalid(
            "工作目录仍然存在，不能清理其登记；如需删除请使用删除工作树。",
        ));
    }
    check_plain_tree(&registration.admin, false)?;
    let retention = safety::prepare_retention(&context.common, &registration)?;
    expected_head(&context.repo, request)?;
    ensure_same(context, &registration)?;
    unlocked(context, &registration)?;
    if registration.path.try_exists().map_err(io_error)? {
        return Err(changed());
    }
    check_plain_tree(&registration.admin, false)?;
    if let Some(retention) = retention {
        retention.validate()?;
        let archive = retention.archive()?;
        return Ok(outcome(
            &format!(
                "已清理缺失工作树的登记，分支已保留。子模块 Git 数据保留在：{}",
                crate::display_path(&archive)
            ),
            Some(registration.path),
            None,
        ));
    }
    require_prunable_without_modules(&registration)?;
    context
        .repo
        .find_worktree(&registration.name)
        .map_err(git_error)?
        .prune(None)
        .map_err(git_error)?;
    Ok(outcome(
        "已清理缺失工作树的登记，分支已保留。",
        Some(registration.path),
        None,
    ))
}

fn repair(
    context: &Workspace,
    request: &WorktreeRequest,
    path: &Path,
    target: &Path,
) -> Result<WorktreeOutcome> {
    let registration = registration(context, path)?;
    require_not_current(context, &registration)?;
    unlocked(context, &registration)?;
    if registration.path.try_exists().map_err(io_error)? {
        return Err(invalid(
            "旧工作目录仍然存在；仅支持修复在应用外移动且原路径缺失的工作树。",
        ));
    }
    let target = safe_path(target)?;
    safety::repair_destination(context, &registration, &target)?;
    let dotgit = read_small(&target.join(".git"))?;
    if !same_path(
        &safety::dotgit_target(&target, &dotgit)?,
        &registration.admin,
    ) {
        return Err(invalid(
            "新目录的 .git 未指向选中的工作树登记，不能自动修复。",
        ));
    }
    let new_gitdir = safety::path_line(&target.join(".git"), "")?;
    expected_head(&context.repo, request)?;
    ensure_same(context, &registration)?;
    unlocked(context, &registration)?;
    if registration.path.try_exists().map_err(io_error)?
        || read_small(&target.join(".git"))? != dotgit
    {
        return Err(changed());
    }
    atomic_replace(
        &registration.admin.join("gitdir"),
        &registration.gitdir,
        &new_gitdir,
    )?;
    let repaired = registration.with_path(target.clone(), new_gitdir.clone());
    if let Err(error) = valid_repository(context, &repaired) {
        if safety::restore_if_matches(
            &registration.admin.join("gitdir"),
            &new_gitdir,
            &registration.gitdir,
        )
        .is_err()
        {
            return Err(WorktreeError::new(
                WorktreeErrorKind::Io,
                "修复未通过验证，且无法恢复登记；文件已保留，请检查 Git 链接。",
            ));
        }
        return Err(error);
    }
    Ok(outcome(
        "已修复工作树登记与新目录之间的 Git 链接。",
        Some(registration.path),
        Some(target),
    ))
}

fn outcome(
    message: &str,
    previous_path: Option<PathBuf>,
    path: Option<PathBuf>,
) -> WorktreeOutcome {
    WorktreeOutcome {
        message: message.into(),
        previous_path,
        path,
    }
}
fn invalid(message: &str) -> WorktreeError {
    WorktreeError::new(WorktreeErrorKind::InvalidRequest, message)
}
fn changed() -> WorktreeError {
    WorktreeError::new(
        WorktreeErrorKind::Changed,
        "仓库 HEAD 或工作树登记已改变；未继续操作，请刷新后重试。",
    )
}
fn git_error(error: git2::Error) -> WorktreeError {
    let kind = if error.code() == ErrorCode::Locked {
        WorktreeErrorKind::Busy
    } else {
        WorktreeErrorKind::Git
    };
    WorktreeError::new(kind, format!("Git 工作树操作失败：{error}"))
}
