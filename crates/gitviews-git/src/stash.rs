//! Native stash operations. Public requests identify a stash by immutable OID,
//! never by its shifting ordinal in the shared stash reflog.

use std::{collections::BTreeSet, error::Error, fmt, fs, path::Path};

use git2::{
    ErrorCode, Oid, Repository, RepositoryState, StashApplyOptions, StashFlags, StatusOptions,
    Transaction, TreeWalkMode, TreeWalkResult, build::CheckoutBuilder,
};

use crate::checkout::index_transaction::IndexTransaction;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StashEntry {
    pub id: String,
    pub index: usize,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StashAction {
    Save {
        message: String,
        include_untracked: bool,
        include_ignored: bool,
    },
    Apply {
        id: String,
        reinstate_index: bool,
    },
    Pop {
        id: String,
        reinstate_index: bool,
    },
    Drop {
        id: String,
    },
}

#[derive(Clone, Debug)]
pub struct StashRequest {
    /// The displayed HEAD. None only matches an unborn HEAD.
    pub expected_head: Option<String>,
    pub action: StashAction,
}

#[derive(Clone, Debug)]
pub struct StashOutcome {
    pub summary: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StashErrorKind {
    Unsupported,
    InvalidRepository,
    InvalidRequest,
    ConcurrentChange,
    InProgress,
    UnbornHead,
    NotFound,
    NoChanges,
    IdentityRequired,
    Conflict,
    Failed,
}

#[derive(Clone, Debug)]
pub struct StashError {
    pub kind: StashErrorKind,
    pub message: String,
}

impl StashError {
    pub fn new(kind: StashErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}
impl fmt::Display for StashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl Error for StashError {}

pub(super) fn list(root: &Path) -> Result<Vec<StashEntry>, StashError> {
    let mut repository = open(root)?;
    let mut entries = Vec::new();
    repository
        .stash_foreach(|index, message, id| {
            entries.push(StashEntry {
                id: id.to_string(),
                index,
                message: message.to_owned(),
            });
            true
        })
        .map_err(map_error)?;
    Ok(entries)
}

pub(super) fn execute(root: &Path, request: &StashRequest) -> Result<StashOutcome, StashError> {
    let mut repository = open(root)?;
    validate_head(&repository, request)?;
    require_ready(&repository)?;
    if !matches!(request.action, StashAction::Drop { .. }) {
        require_visible_index(&repository)?;
    }
    // Locks both a symbolic HEAD and its referent, so checkout/commit cannot
    // change the base while stash cleanup or application is in progress.
    let head_name = repository
        .head()
        .ok()
        .and_then(|head| head.name().ok().map(str::to_owned));
    let guard_repository = open(root)?;
    let mut head_lock = guard_repository.transaction().map_err(map_error)?;
    head_lock.lock_ref("HEAD").map_err(map_error)?;
    if let Some(name) = &head_name
        && name != "HEAD"
    {
        head_lock.lock_ref(name).map_err(map_error)?;
    }
    validate_head(&repository, request)?;

    match &request.action {
        StashAction::Save {
            message,
            include_untracked,
            include_ignored,
        } => {
            if message.contains('\0') || message.len() > 4096 {
                return Err(StashError::new(
                    StashErrorKind::InvalidRequest,
                    "储藏说明不能包含空字符，长度不能超过 4096 字节。",
                ));
            }
            if request.expected_head.is_none() {
                return Err(StashError::new(
                    StashErrorKind::UnbornHead,
                    "仓库尚无首次提交，请先提交后再储藏。",
                ));
            }
            let signature = repository.signature().map_err(|_| {
                StashError::new(
                    StashErrorKind::IdentityRequired,
                    "请先在 Git 配置中填写用户名和邮箱，再保存储藏。",
                )
            })?;
            validate_save_paths(&repository, *include_untracked, *include_ignored)?;
            let mut index = IndexTransaction::prepare(&repository).map_err(index_error)?;
            let before = repository.refname_to_id("refs/stash").ok();
            let mut flags = StashFlags::empty();
            if *include_untracked {
                flags |= StashFlags::INCLUDE_UNTRACKED;
            }
            if *include_ignored {
                flags |= StashFlags::INCLUDE_IGNORED;
            }
            // libgit2 writes refs/stash before it resets files. Never attempt a
            // destructive rollback when cleanup fails; the stash is recovery.
            let saved = repository.stash_save2(
                &signature,
                (!message.trim().is_empty()).then_some(message.trim()),
                Some(flags),
            );
            let id = match saved {
                Ok(id) => id,
                Err(error) => {
                    let after = repository.refname_to_id("refs/stash").ok();
                    if after.is_some() && after != before {
                        return Err(StashError::new(
                            StashErrorKind::Failed,
                            "储藏记录已保存，但工作区清理未完成；记录仍然保留，请重新读取并检查文件后恢复。",
                        ));
                    }
                    if error.code() == ErrorCode::NotFound {
                        return Err(StashError::new(
                            StashErrorKind::NoChanges,
                            "当前没有符合所选范围的改动可储藏。未跟踪或忽略文件需明确勾选后才包含。",
                        ));
                    }
                    return Err(map_error(error));
                }
            };
            index.commit().map_err(|error| {
                StashError::new(
                    StashErrorKind::Failed,
                    format!(
                        "储藏 {} 已保存，但暂存区更新失败；记录仍保留。{}",
                        &id.to_string()[..8],
                        error
                    ),
                )
            })?;
            Ok(StashOutcome {
                summary: format!(
                    "已保存储藏 {}。子模块内部的改动需进入子模块单独储藏。",
                    &id.to_string()[..8]
                ),
            })
        }
        StashAction::Apply {
            id,
            reinstate_index,
        }
        | StashAction::Pop {
            id,
            reinstate_index,
        } => {
            let oid = parse_id(id)?;
            let stash_repository = open(root)?;
            let mut stash_lock = stash_repository.transaction().map_err(map_error)?;
            stash_lock.lock_ref("refs/stash").map_err(map_error)?;
            let position = position(&stash_repository, oid)?;
            validate_apply_paths(&repository, oid)?;
            require_nonoverlapping_local_changes(&repository, oid)?;
            let mut index = IndexTransaction::prepare(&repository).map_err(index_error)?;
            let mut options = StashApplyOptions::new();
            options.checkout_options({
                let mut checkout = CheckoutBuilder::new();
                checkout
                    .safe()
                    .allow_conflicts(true)
                    .conflict_style_merge(true)
                    .overwrite_ignored(false);
                checkout
            });
            if *reinstate_index {
                options.reinstantiate_index();
            }
            let applied = repository.stash_apply(position, Some(&mut options));
            drop(options);
            let conflicts = repository.index().map_err(map_error)?.has_conflicts();
            if applied.is_ok() || conflicts {
                index.commit().map_err(|error| {
                    StashError::new(
                        StashErrorKind::Failed,
                        format!(
                            "恢复时暂存区写入失败，储藏记录仍保留；请检查文件和暂存区。{error}"
                        ),
                    )
                })?;
            }
            if conflicts {
                return Err(StashError::new(
                    StashErrorKind::Conflict,
                    "恢复产生冲突；储藏记录已保留，请在工作区解决冲突并重新读取。",
                ));
            }
            applied.map_err(|error| {
                StashError::new(
                    if matches!(error.code(), ErrorCode::Conflict | ErrorCode::Modified) {
                        StashErrorKind::Conflict
                    } else {
                        StashErrorKind::Failed
                    },
                    format!(
                        "未能完成恢复，储藏记录已保留。请检查工作区，现有改动可能与储藏冲突：{}",
                        error.message()
                    ),
                )
            })?;
            if matches!(request.action, StashAction::Pop { .. }) {
                drop_locked(&stash_repository, stash_lock, oid).map_err(|error| {
                    StashError::new(
                        error.kind,
                        format!("改动已恢复，但储藏记录移除失败；不要重复应用。{error}"),
                    )
                })?;
                Ok(StashOutcome {
                    summary: "已恢复改动并移除该储藏。".into(),
                })
            } else {
                Ok(StashOutcome {
                    summary: "已恢复改动，储藏记录仍保留。".into(),
                })
            }
        }
        StashAction::Drop { id } => {
            let oid = parse_id(id)?;
            let mut transaction = repository.transaction().map_err(map_error)?;
            transaction.lock_ref("refs/stash").map_err(map_error)?;
            drop_locked(&repository, transaction, oid)?;
            Ok(StashOutcome {
                summary: "已删除所选储藏记录。".into(),
            })
        }
    }
}

fn drop_locked(
    repository: &Repository,
    mut transaction: Transaction<'_>,
    oid: Oid,
) -> Result<(), StashError> {
    let mut log = repository.reflog("refs/stash").map_err(map_error)?;
    let index = (0..log.len())
        .find(|&i| log.get(i).is_some_and(|entry| entry.id_new() == oid))
        .ok_or_else(missing)?;
    let old_len = log.len();
    log.remove(index, true).map_err(map_error)?;
    let top = log.get(0).map(|entry| entry.id_new());
    transaction
        .set_reflog("refs/stash", log)
        .map_err(map_error)?;
    if old_len == 1 {
        transaction.remove("refs/stash").map_err(map_error)?;
    } else if index == 0 {
        transaction
            .set_target(
                "refs/stash",
                top.expect("nonempty stash log"),
                None,
                "stash drop",
            )
            .map_err(map_error)?;
    }
    transaction.commit().map_err(map_error)
}

fn position(repository: &Repository, oid: Oid) -> Result<usize, StashError> {
    let log = repository.reflog("refs/stash").map_err(|error| {
        if error.code() == ErrorCode::NotFound {
            missing()
        } else {
            map_error(error)
        }
    })?;
    (0..log.len())
        .find(|&i| log.get(i).is_some_and(|entry| entry.id_new() == oid))
        .ok_or_else(missing)
}

fn parse_id(id: &str) -> Result<Oid, StashError> {
    if id.len() != 40 {
        return Err(StashError::new(
            StashErrorKind::InvalidRequest,
            "储藏标识无效，请重新读取列表。",
        ));
    }
    Oid::from_str(id).map_err(|_| {
        StashError::new(
            StashErrorKind::InvalidRequest,
            "储藏标识无效，请重新读取列表。",
        )
    })
}

fn missing() -> StashError {
    StashError::new(
        StashErrorKind::NotFound,
        "所选储藏已被移除，请重新读取列表。",
    )
}

fn open(root: &Path) -> Result<Repository, StashError> {
    let invalid = || {
        StashError::new(
            StashErrorKind::InvalidRepository,
            "请使用有效仓库或工作树的根目录。",
        )
    };
    let canonical = fs::canonicalize(root).map_err(|_| invalid())?;
    let repository = Repository::open(&canonical).map_err(|_| invalid())?;
    if repository.is_bare()
        || repository
            .workdir()
            .and_then(|path| fs::canonicalize(path).ok())
            .as_ref()
            != Some(&canonical)
    {
        return Err(invalid());
    }
    Ok(repository)
}

fn validate_head(repository: &Repository, request: &StashRequest) -> Result<(), StashError> {
    let current = match repository.head() {
        Ok(head) => head.target().map(|id| id.to_string()),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => None,
        Err(error) => return Err(map_error(error)),
    };
    if current != request.expected_head {
        return Err(StashError::new(
            StashErrorKind::ConcurrentChange,
            "仓库 HEAD 已变化，请重新读取后再操作储藏。",
        ));
    }
    Ok(())
}

fn require_ready(repository: &Repository) -> Result<(), StashError> {
    if repository.state() != RepositoryState::Clean
        || repository.index().map_err(map_error)?.has_conflicts()
    {
        return Err(StashError::new(
            StashErrorKind::InProgress,
            "仓库正在合并、变基或存在未解决冲突；请先完成或中止该操作。",
        ));
    }
    for directory in [repository.path(), repository.commondir()] {
        for lock in [
            "index.lock",
            "HEAD.lock",
            "refs/stash.lock",
            "logs/refs/stash.lock",
        ] {
            if fs::symlink_metadata(directory.join(lock)).is_ok() {
                return Err(StashError::new(
                    StashErrorKind::ConcurrentChange,
                    "仓库被其他 Git 操作锁定，请等待完成后重新读取。",
                ));
            }
        }
    }
    Ok(())
}

fn require_visible_index(repository: &Repository) -> Result<(), StashError> {
    if repository
        .index()
        .map_err(map_error)?
        .iter()
        .any(|entry| entry.flags & 0x8000 != 0 || entry.flags_extended & 0x4000 != 0)
    {
        return Err(StashError::new(
            StashErrorKind::Unsupported,
            "暂存区包含 assume-unchanged 或 skip-worktree 文件；请先取消这些标记并检查本地修改，再储藏或恢复。",
        ));
    }
    Ok(())
}

// In particular, a Windows junction may look like an ordinary directory to
// libgit2. Do not let opt-in ignored/untracked cleanup follow such a directory
// into another worktree. Only candidate paths are inspected; untouched ignored
// trees do not prevent saving normal tracked modifications.
fn validate_save_paths(
    repository: &Repository,
    untracked: bool,
    ignored: bool,
) -> Result<(), StashError> {
    let mut options = StatusOptions::new();
    options
        .include_untracked(untracked)
        .include_ignored(ignored)
        .recurse_untracked_dirs(untracked)
        .recurse_ignored_dirs(ignored)
        .exclude_submodules(true);
    for entry in repository
        .statuses(Some(&mut options))
        .map_err(map_error)?
        .iter()
    {
        let path = entry.path().map_err(|_| {
            StashError::new(
                StashErrorKind::Unsupported,
                "储藏中存在无法安全识别的文件路径，请使用 Git 专门处理该文件。",
            )
        })?;
        validate_path(repository, path)?;
    }
    Ok(())
}

fn validate_apply_paths(repository: &Repository, oid: Oid) -> Result<(), StashError> {
    let commit = repository.find_commit(oid).map_err(map_error)?;
    let mut trees = vec![commit.tree().map_err(map_error)?];
    // All tracked, staged, deleted and optional untracked paths can be touched.
    for index in 0..commit.parent_count() {
        trees.push(
            commit
                .parent(index)
                .and_then(|parent| parent.tree())
                .map_err(map_error)?,
        );
    }
    for tree in trees {
        let mut failure = None;
        tree.walk(TreeWalkMode::PreOrder, |prefix, entry| {
            let Ok(name) = entry.name() else {
                failure = Some(StashError::new(
                    StashErrorKind::Unsupported,
                    "储藏中存在无法安全识别的路径。",
                ));
                return TreeWalkResult::Abort;
            };
            if let Err(error) = validate_path(repository, &format!("{prefix}{name}")) {
                failure = Some(error);
                return TreeWalkResult::Abort;
            }
            if entry.filemode() == 0o160000 {
                TreeWalkResult::Skip
            } else {
                TreeWalkResult::Ok
            }
        })
        .map_err(|error| failure.clone().unwrap_or_else(|| map_error(error)))?;
    }
    Ok(())
}

// ALLOW_CONFLICTS is needed to publish genuine three-way merge conflicts, but
// libgit2 may also permit replacing existing workdir edits with that flag.
// Detect local overlaps before writing anything. Unrelated edits remain usable.
fn require_nonoverlapping_local_changes(
    repository: &Repository,
    oid: Oid,
) -> Result<(), StashError> {
    let commit = repository.find_commit(oid).map_err(map_error)?;
    let base = commit
        .parent(0)
        .and_then(|parent| parent.tree())
        .map_err(map_error)?;
    let working = commit.tree().map_err(map_error)?;
    let staged = commit
        .parent(1)
        .and_then(|parent| parent.tree())
        .map_err(map_error)?;
    let mut paths = BTreeSet::new();
    for tree in [&working, &staged] {
        for delta in repository
            .diff_tree_to_tree(Some(&base), Some(tree), None)
            .map_err(map_error)?
            .deltas()
        {
            for file in [delta.old_file(), delta.new_file()] {
                if let Some(path) = file.path() {
                    let text = path.to_str().ok_or_else(|| {
                        StashError::new(
                            StashErrorKind::Unsupported,
                            "储藏中存在无法安全识别的文件路径。",
                        )
                    })?;
                    paths.insert(text.to_owned());
                }
            }
        }
    }
    if commit.parent_count() > 2 {
        let tree = commit
            .parent(2)
            .and_then(|parent| parent.tree())
            .map_err(map_error)?;
        tree.walk(TreeWalkMode::PreOrder, |prefix, entry| {
            if let Ok(name) = entry.name() {
                paths.insert(format!("{prefix}{name}"));
            }
            TreeWalkResult::Ok
        })
        .map_err(map_error)?;
    }
    if paths.is_empty() {
        return Ok(());
    }
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .include_ignored(true)
        .recurse_untracked_dirs(true)
        .recurse_ignored_dirs(true)
        .disable_pathspec_match(true)
        .exclude_submodules(true);
    for path in &paths {
        options.pathspec(path);
    }
    let statuses = repository.statuses(Some(&mut options)).map_err(map_error)?;
    if !statuses.is_empty() {
        return Err(StashError::new(
            StashErrorKind::Conflict,
            "储藏涉及的路径已有本地改动或文件；请先保存这些改动再恢复。储藏和现有文件均已保留。",
        ));
    }
    Ok(())
}

fn validate_path(repository: &Repository, path: &str) -> Result<(), StashError> {
    use std::path::Component;
    if path.contains(['\0', '\\']) {
        return Err(StashError::new(
            StashErrorKind::InvalidRequest,
            "储藏路径包含无效字符。",
        ));
    }
    let mut current = repository
        .workdir()
        .expect("validated worktree")
        .to_path_buf();
    let components: Vec<_> = Path::new(path).components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(StashError::new(
                StashErrorKind::InvalidRequest,
                "储藏路径超出当前工作树。",
            ));
        };
        let text = name.to_str().ok_or_else(|| {
            StashError::new(
                StashErrorKind::Unsupported,
                "储藏中存在无法安全识别的文件路径。",
            )
        })?;
        if text
            .trim_end_matches([' ', '.'])
            .eq_ignore_ascii_case(".git")
            || cfg!(windows) && (text.contains(':') || text.ends_with([' ', '.']))
        {
            return Err(StashError::new(
                StashErrorKind::InvalidRequest,
                "储藏路径包含 Git 元数据目录或 Windows 路径别名，无法安全处理。",
            ));
        }
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if is_link(&metadata) => {
                // A tracked leaf symlink is a Git blob; libgit2 treats its
                // contents as link text. Directory links and ancestor links
                // must never be traversed by cleanup/restore.
                if index + 1 != components.len()
                    || metadata.is_dir()
                    || !metadata.file_type().is_symlink()
                {
                    return Err(StashError::new(
                        StashErrorKind::Unsupported,
                        format!(
                            "路径 {path} 包含目录链接或重解析点；请排除该路径后储藏，避免影响链接目标。"
                        ),
                    ));
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => {
                return Err(StashError::new(
                    StashErrorKind::Failed,
                    format!("无法安全读取储藏路径 {path}。"),
                ));
            }
        }
    }
    Ok(())
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn index_error(error: crate::CheckoutError) -> StashError {
    let kind = if error.kind == crate::CheckoutErrorKind::ConcurrentChange {
        StashErrorKind::ConcurrentChange
    } else {
        StashErrorKind::Failed
    };
    StashError::new(kind, error.message)
}
fn map_error(error: git2::Error) -> StashError {
    let kind = match error.code() {
        ErrorCode::Locked => StashErrorKind::ConcurrentChange,
        ErrorCode::NotFound => StashErrorKind::NotFound,
        ErrorCode::Conflict | ErrorCode::Modified | ErrorCode::Unmerged => StashErrorKind::Conflict,
        _ => StashErrorKind::Failed,
    };
    StashError::new(kind, format!("储藏操作未完成：{}", error.message()))
}
