//! Native merge/rebase and conflict resolution. No hooks, shell commands or
//! interactive rebase instructions are executed.
use crate::checkout::index_transaction::IndexTransaction;
use git2::{
    ErrorCode, Index, IndexEntry, IndexTime, ObjectType, Oid, RebaseOperationType, RebaseOptions,
    Repository, RepositoryState, StatusOptions, build::CheckoutBuilder,
};
use std::{
    collections::BTreeSet,
    error::Error,
    fmt,
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const LIMIT: usize = 8 * 1024 * 1024;
const MARKER: &str = "gitviews-conflict-operation";
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictOperation {
    Clean,
    Merge,
    Rebase,
    Unsupported,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictFile {
    pub path: String,
}
#[derive(Clone, Debug)]
pub struct ConflictState {
    pub kind: ConflictOperation,
    pub label: String,
    pub head: Option<String>,
    pub files: Vec<ConflictFile>,
    pub can_continue: bool,
    pub can_abort: bool,
    pub backup_ref: Option<String>,
}
#[derive(Clone, Debug)]
pub struct ConflictVersions {
    pub path: String,
    /// None means this side deleted the file. Binary sides carry a display hint.
    pub base: Option<String>,
    pub ours: Option<String>,
    pub theirs: Option<String>,
    pub result: String,
    pub binary: bool,
    pub token: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConflictResolution {
    Ours,
    Theirs,
    Edited(String),
    Delete,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConflictAction {
    Merge {
        target: String,
    },
    Rebase {
        onto: String,
    },
    Resolve {
        path: String,
        token: String,
        resolution: ConflictResolution,
    },
    Continue,
    Abort,
}
#[derive(Clone, Debug)]
pub struct ConflictRequest {
    pub expected_head: Option<String>,
    pub action: ConflictAction,
}
#[derive(Clone, Debug)]
pub struct ConflictOutcome {
    pub summary: String,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConflictErrorKind {
    Unsupported,
    InProgress,
    InvalidRepository,
    InvalidRequest,
    ConcurrentChange,
    Dirty,
    Unresolved,
    NotFound,
    IdentityRequired,
    Failed,
}
#[derive(Clone, Debug)]
pub struct ConflictError {
    pub kind: ConflictErrorKind,
    pub message: String,
}
impl ConflictError {
    pub fn new(kind: ConflictErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}
impl fmt::Display for ConflictError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl Error for ConflictError {}

pub(super) fn state(root: &Path) -> Result<ConflictState, ConflictError> {
    let repository = open(root)?;
    let index = repository.index().map_err(git_error)?;
    let mut files = BTreeSet::new();
    for conflict in index.conflicts().map_err(git_error)? {
        let conflict = conflict.map_err(git_error)?;
        for entry in [conflict.our, conflict.their, conflict.ancestor]
            .into_iter()
            .flatten()
        {
            files.insert(
                String::from_utf8(entry.path)
                    .map_err(|_| unsupported("冲突路径不是 UTF-8，请使用 Git 处理。"))?,
            );
        }
    }
    let marker = read_marker(&repository)?;
    let (kind, label, supported) = match repository.state() {
        RepositoryState::Clean if marker.is_some() => (
            ConflictOperation::Unsupported,
            "上次操作初始化未完成；可中止并保留操作前备份".to_owned(),
            false,
        ),
        RepositoryState::Clean => (
            ConflictOperation::Clean,
            if files.is_empty() {
                "没有进行中的合并或变基"
            } else {
                "工作区存在未解决冲突"
            }
            .to_owned(),
            false,
        ),
        RepositoryState::Merge => (ConflictOperation::Merge, "正在合并".to_owned(), true),
        RepositoryState::RebaseMerge
            if marker
                .as_ref()
                .is_some_and(|marker| marker.kind == "rebase") =>
        {
            validate_owned_rebase(&repository, marker.as_ref().unwrap())?;
            (
                ConflictOperation::Rebase,
                if repository.head_detached().map_err(git_error)? {
                    "正在变基（ours 为目标分支，theirs 为正在重放的提交）"
                } else {
                    "变基分支已经恢复，请中止操作完成剩余清理"
                }
                .to_owned(),
                repository.head_detached().map_err(git_error)?,
            )
        }
        RepositoryState::Rebase
        | RepositoryState::RebaseInteractive
        | RepositoryState::RebaseMerge => (
            ConflictOperation::Unsupported,
            "检测到外部变基；可解决文件冲突，继续或中止请使用发起变基的 Git 工具".to_owned(),
            false,
        ),
        _ => (
            ConflictOperation::Unsupported,
            "此 Git 操作布局暂不支持继续或中止，可先解决文件冲突".to_owned(),
            false,
        ),
    };
    Ok(ConflictState {
        kind,
        label,
        head: head(&repository)?,
        files: files
            .into_iter()
            .map(|path| ConflictFile { path })
            .collect(),
        can_continue: supported && !index.has_conflicts(),
        can_abort: marker.is_some()
            && (supported
                || repository.state() == RepositoryState::Clean
                || repository.state() == RepositoryState::RebaseMerge),
        backup_ref: marker.map(|marker| marker.backup),
    })
}

pub(super) fn versions(root: &Path, path: &str) -> Result<ConflictVersions, ConflictError> {
    let repository = open(root)?;
    versions_in(&repository, path)
}
fn versions_in(repository: &Repository, path: &str) -> Result<ConflictVersions, ConflictError> {
    let full = checked_path(repository, path)?;
    let index = repository.index().map_err(git_error)?;
    let conflict = index.conflict_get(Path::new(path)).map_err(git_error)?;
    let mut binary = false;
    let mut token = String::new();
    let mut side = |entry: Option<&IndexEntry>| -> Result<Option<String>, ConflictError> {
        let Some(entry) = entry else {
            token.push_str("deleted;");
            return Ok(None);
        };
        token.push_str(&format!("{}:{};", entry.id, entry.mode));
        require_regular(entry, path)?;
        let blob = repository.find_blob(entry.id).map_err(git_error)?;
        if blob.size() > LIMIT {
            binary = true;
            return Ok(Some(
                "（内容超过 8 MiB，请选择一个版本或使用外部工具）".into(),
            ));
        }
        if blob.content().contains(&0) {
            binary = true;
            return Ok(Some("（二进制文件，请选择一个版本）".into()));
        }
        match std::str::from_utf8(blob.content()) {
            Ok(text) => Ok(Some(text.into())),
            Err(_) => {
                binary = true;
                Ok(Some("（非 UTF-8 文件，请选择一个版本）".into()))
            }
        }
    };
    let base = side(conflict.ancestor.as_ref())?;
    let ours = side(conflict.our.as_ref())?;
    let theirs = side(conflict.their.as_ref())?;
    let current = read_file(&full)?;
    token.push_str(
        &current
            .as_ref()
            .map(|bytes| Oid::hash_object(ObjectType::Blob, bytes).map(|id| id.to_string()))
            .transpose()
            .map_err(git_error)?
            .unwrap_or_else(|| "deleted".into()),
    );
    let result = current
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .unwrap_or_default()
        .to_owned();
    Ok(ConflictVersions {
        path: path.into(),
        base,
        ours,
        theirs,
        result,
        binary,
        token,
    })
}

pub(super) fn execute(
    root: &Path,
    request: &ConflictRequest,
) -> Result<ConflictOutcome, ConflictError> {
    let repository = open(root)?;
    check_head(&repository, &request.expected_head)?;
    require_visible(&repository)?;
    match &request.action {
        ConflictAction::Resolve {
            path,
            token,
            resolution,
        } => resolve(&repository, request, path, token, resolution),
        ConflictAction::Merge { target } => start_merge(&repository, request, target),
        ConflictAction::Rebase { onto } => start_rebase(root, &repository, request, onto),
        ConflictAction::Continue => continue_operation(root, &repository, request),
        ConflictAction::Abort => abort_operation(&repository, request),
    }
}

fn resolve(
    repository: &Repository,
    request: &ConflictRequest,
    path: &str,
    token: &str,
    resolution: &ConflictResolution,
) -> Result<ConflictOutcome, ConflictError> {
    let mut lease = IndexTransaction::prepare(repository).map_err(index_error)?;
    let guard_repository = Repository::open(repository.workdir().unwrap()).map_err(git_error)?;
    let mut guard = guard_repository.transaction().map_err(git_error)?;
    guard.lock_ref("HEAD").map_err(git_error)?;
    if let Ok(head) = repository.head() {
        let name = head.name().map_err(git_error)?;
        if name != "HEAD" {
            guard.lock_ref(name).map_err(git_error)?;
        }
    }
    check_head(repository, &request.expected_head)?;
    let current = versions_in(repository, path)?;
    if current.token != token {
        return Err(concurrent("冲突内容或工作文件已变化，请重新读取后再解决。"));
    }
    let full = checked_path(repository, path)?;
    let mut index = repository.index().map_err(git_error)?;
    let conflict = index.conflict_get(Path::new(path)).map_err(git_error)?;
    for entry in [&conflict.ancestor, &conflict.our, &conflict.their]
        .into_iter()
        .flatten()
    {
        if entry.path != path.as_bytes() {
            return Err(unsupported(
                "重命名冲突请使用 Git 专门处理，避免丢失其他路径。",
            ));
        }
    }
    let selected = match resolution {
        ConflictResolution::Ours => conflict.our.as_ref(),
        ConflictResolution::Theirs => conflict.their.as_ref(),
        _ => conflict
            .our
            .as_ref()
            .or(conflict.their.as_ref())
            .or(conflict.ancestor.as_ref()),
    };
    let bytes = match resolution {
        ConflictResolution::Delete => None,
        ConflictResolution::Edited(text) => {
            if current.binary || text.len() > LIMIT || text.contains('\0') {
                return Err(unsupported(
                    "此内容不能在文本编辑器中安全保存，请选择版本或使用外部工具。",
                ));
            }
            Some(text.as_bytes().to_vec())
        }
        _ => selected
            .map(|entry| {
                repository
                    .find_blob(entry.id)
                    .map(|blob| blob.content().to_vec())
            })
            .transpose()
            .map_err(git_error)?,
    };
    let backup = backup_file(repository, path, read_file(&full)?.as_deref())?;
    if let Some(bytes) = &bytes {
        replace_file(&full, bytes)?;
    } else if full.exists() {
        fs::remove_file(&full).map_err(io_error)?;
    }
    index.conflict_remove(Path::new(path)).map_err(git_error)?;
    if let Some(bytes) = &bytes {
        let mode = selected.map_or(0o100644, |entry| entry.mode);
        let entry = IndexEntry {
            ctime: IndexTime::new(0, 0),
            mtime: IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            file_size: bytes.len() as u32,
            id: repository.blob(bytes).map_err(git_error)?,
            flags: 0,
            flags_extended: 0,
            path: path.as_bytes().to_vec(),
        };
        index.add(&entry).map_err(git_error)?;
    } else {
        let _ = index.remove_path(Path::new(path));
    }
    index.write().map_err(git_error)?;
    lease.commit().map_err(|error| {
        ConflictError::new(
            ConflictErrorKind::Failed,
            format!(
                "合并结果已保留在工作文件，但暂存区更新失败，请重新读取。原内容备份：{}。{error}",
                backup.display()
            ),
        )
    })?;
    Ok(ConflictOutcome {
        summary: format!(
            "已解决并暂存 {path}；原工作内容备份在 {}。",
            backup.display()
        ),
    })
}

fn start_merge(
    repository: &Repository,
    request: &ConflictRequest,
    target: &str,
) -> Result<ConflictOutcome, ConflictError> {
    require_clean(repository)?;
    let target = target_commit(repository, target)?;
    let original = repository
        .head()
        .and_then(|head| head.peel_to_commit())
        .map_err(git_error)?;
    if target == original.id()
        || repository
            .graph_descendant_of(original.id(), target)
            .map_err(git_error)?
    {
        return Ok(ConflictOutcome {
            summary: "目标提交已经包含在当前分支中。".into(),
        });
    }
    let target_commit = repository.find_commit(target).map_err(git_error)?;
    let candidate = repository
        .merge_commits(&original, &target_commit, None)
        .map_err(git_error)?;
    check_candidate(repository, &candidate)?;
    let marker = create_marker(repository, "merge", target)?;
    let mut lease = IndexTransaction::prepare(repository).map_err(index_error)?;
    let guard_repository = Repository::open(repository.workdir().unwrap()).map_err(git_error)?;
    let mut guard = guard_repository.transaction().map_err(git_error)?;
    guard.lock_ref("HEAD").map_err(git_error)?;
    if marker.branch != "HEAD" {
        guard.lock_ref(&marker.branch).map_err(git_error)?;
    }
    check_head(repository, &request.expected_head)?;
    let annotated = repository
        .find_annotated_commit(target)
        .map_err(git_error)?;
    let result = repository.merge(&[&annotated], None, Some(&mut checkout()));
    if result.is_ok() || repository.index().map_err(git_error)?.has_conflicts() {
        lease.commit().map_err(index_error)?;
    }
    result.map_err(git_error)?;
    let count = conflict_count(repository)?;
    Ok(ConflictOutcome {
        summary: if count > 0 {
            format!(
                "合并遇到 {count} 个冲突，请解决后继续。操作前备份：{}",
                marker.backup
            )
        } else {
            format!(
                "合并内容已准备完成，点击继续创建合并提交。操作前备份：{}",
                marker.backup
            )
        },
    })
}

fn start_rebase(
    root: &Path,
    repository: &Repository,
    request: &ConflictRequest,
    onto: &str,
) -> Result<ConflictOutcome, ConflictError> {
    require_clean(repository)?;
    if repository.head_detached().map_err(git_error)? {
        return Err(unsupported("请先切换到本地分支再发起变基。"));
    }
    let target = target_commit(repository, onto)?;
    let current = repository
        .head()
        .and_then(|head| head.peel_to_commit())
        .map_err(git_error)?;
    let target_tree = repository
        .find_commit(target)
        .and_then(|commit| commit.tree())
        .map_err(git_error)?;
    check_tree_paths(repository, &target_tree)?;
    check_ignored_collisions(
        repository,
        &current.tree().map_err(git_error)?,
        &target_tree,
    )?;
    let _signature = signature(repository)?;
    let marker = create_marker(repository, "rebase", target)?;
    let mut lease = IndexTransaction::prepare(repository).map_err(index_error)?;
    check_head(repository, &request.expected_head)?;
    let annotated = repository
        .find_annotated_commit(target)
        .map_err(git_error)?;
    let mut options = rebase_options();
    let mut rebase = repository
        .rebase(None, Some(&annotated), None, Some(&mut options))
        .map_err(git_error)?;
    validate_picks(&mut rebase)?;
    if let Some(operation) = rebase.nth(0) {
        let commit = repository.find_commit(operation.id()).map_err(git_error)?;
        let current = repository
            .head()
            .and_then(|head| head.peel_to_commit())
            .map_err(git_error)?;
        let candidate = repository
            .cherrypick_commit(&commit, &current, 0, None)
            .map_err(git_error)?;
        if let Err(error) = check_candidate(repository, &candidate) {
            // Rebase init has already checked out onto and created metadata.
            // Publish that clean index before pausing for a file collision.
            lease.commit().map_err(index_error)?;
            return Err(error);
        }
    }
    if let Some(operation) = rebase.next() {
        operation.map_err(git_error)?;
    }
    lease.commit().map_err(index_error)?;
    drop(rebase);
    drop(options);
    drop(lease);
    if conflict_count(repository)? > 0 {
        return Ok(ConflictOutcome {
            summary: format!("变基遇到冲突，请解决后继续。操作前备份：{}", marker.backup),
        });
    }
    advance_rebase(root, head(repository)?)
}

fn continue_operation(
    root: &Path,
    repository: &Repository,
    request: &ConflictRequest,
) -> Result<ConflictOutcome, ConflictError> {
    if repository.index().map_err(git_error)?.has_conflicts() {
        return Err(ConflictError::new(
            ConflictErrorKind::Unresolved,
            "还有未解决冲突，请逐个保存合并结果后继续。",
        ));
    }
    require_no_unstaged(repository)?;
    match repository.state() {
        RepositoryState::Merge => finish_merge(repository, request),
        RepositoryState::RebaseMerge => {
            let marker = read_marker(repository)?
                .ok_or_else(|| unsupported("外部变基不能在此继续，请使用发起变基的 Git 工具。"))?;
            validate_owned_rebase(repository, &marker)?;
            if !repository.head_detached().map_err(git_error)? {
                return Err(unsupported(
                    "HEAD 已经恢复到分支，请中止操作完成清理，不能继续重放提交。",
                ));
            }
            advance_rebase(root, request.expected_head.clone())
        }
        _ => Err(unsupported("当前操作不支持继续，请重新读取状态。")),
    }
}

fn finish_merge(
    repository: &Repository,
    request: &ConflictRequest,
) -> Result<ConflictOutcome, ConflictError> {
    let lease = IndexTransaction::prepare(repository).map_err(index_error)?;
    let mut transaction = repository.transaction().map_err(git_error)?;
    transaction.lock_ref("HEAD").map_err(git_error)?;
    let head = repository.head().map_err(git_error)?;
    let reference = head.name().map_err(git_error)?.to_owned();
    if reference != "HEAD" {
        transaction.lock_ref(&reference).map_err(git_error)?;
    }
    check_head(repository, &request.expected_head)?;
    require_no_unstaged(repository)?;
    let oid = read_oid(&repository.path().join("MERGE_HEAD"))?;
    let first = head.peel_to_commit().map_err(git_error)?;
    let second = repository.find_commit(oid).map_err(git_error)?;
    let mut index = repository.index().map_err(git_error)?;
    if index.has_conflicts() {
        return Err(ConflictError::new(
            ConflictErrorKind::Unresolved,
            "还有未解决冲突。",
        ));
    }
    let tree = repository
        .find_tree(index.write_tree().map_err(git_error)?)
        .map_err(git_error)?;
    let signature = signature(repository)?;
    let message = "Merge commit (GitViews)";
    let commit = repository
        .commit(
            None,
            &signature,
            &signature,
            message,
            &tree,
            &[&first, &second],
        )
        .map_err(git_error)?;
    lease.verify().map_err(index_error)?;
    transaction
        .set_target(&reference, commit, Some(&signature), "merge: GitViews")
        .map_err(git_error)?;
    transaction.commit().map_err(git_error)?;
    repository.cleanup_state().map_err(git_error)?;
    remove_marker(repository)?;
    Ok(ConflictOutcome {
        summary: format!("已完成合并，提交 {}。", &commit.to_string()[..8]),
    })
}

fn advance_rebase(
    root: &Path,
    mut expected_head: Option<String>,
) -> Result<ConflictOutcome, ConflictError> {
    // Publish one index per step. Each step uses a fresh repository so the
    // private index from the preceding step can never be reused after commit.
    for _ in 0..256 {
        let repository = open(root)?;
        check_head(&repository, &expected_head)?;
        let marker =
            read_marker(&repository)?.ok_or_else(|| unsupported("缺少 GitViews 变基记录。"))?;
        validate_owned_rebase(&repository, &marker)?;
        if repository.index().map_err(git_error)?.has_conflicts() {
            return Ok(ConflictOutcome {
                summary: "变基遇到冲突，请解决后继续。".into(),
            });
        }
        require_no_unstaged(&repository)?;
        let mut lease = IndexTransaction::prepare(&repository).map_err(index_error)?;
        let signature = signature(&repository)?;
        let mut options = rebase_options();
        let mut rebase = repository
            .open_rebase(Some(&mut options))
            .map_err(git_error)?;
        if rebase.operation_current().is_some() {
            // Check before libgit2 publishes a new detached HEAD.
            lease.verify().map_err(index_error)?;
            match rebase.commit(None, &signature, None) {
                Ok(id) => expected_head = Some(id.to_string()),
                Err(error) if error.code() == ErrorCode::Applied => {}
                Err(error) => return Err(git_error(error)),
            }
        }
        lease.verify().map_err(index_error)?;
        let next_index = rebase.operation_current().map_or(0, |index| index + 1);
        if let Some(operation) = rebase.nth(next_index) {
            let commit = repository.find_commit(operation.id()).map_err(git_error)?;
            let current = repository
                .head()
                .and_then(|head| head.peel_to_commit())
                .map_err(git_error)?;
            let candidate = repository
                .cherrypick_commit(&commit, &current, 0, None)
                .map_err(git_error)?;
            check_candidate(&repository, &candidate)?;
        }
        match rebase.next() {
            Some(operation) => {
                operation.map_err(git_error)?;
                check_head(&repository, &expected_head)?;
                lease.commit().map_err(index_error)?;
                if repository.index().map_err(git_error)?.has_conflicts() {
                    return Ok(ConflictOutcome {
                        summary: "变基遇到冲突，请解决后继续；操作前备份仍保留。".into(),
                    });
                }
            }
            None => {
                lease.verify().map_err(index_error)?;
                rebase.finish(Some(&signature)).map_err(git_error)?;
                // finish changes refs only; retain the current real index.
                lease.verify().map_err(index_error)?;
                remove_marker(&repository)?;
                return Ok(ConflictOutcome {
                    summary: format!("变基已完成。操作前备份仍保留：{}", marker.backup),
                });
            }
        }
    }
    Ok(ConflictOutcome {
        summary: "已处理 256 个提交，点击继续处理剩余提交。操作进度已保存。".into(),
    })
}

fn abort_operation(
    repository: &Repository,
    request: &ConflictRequest,
) -> Result<ConflictOutcome, ConflictError> {
    let marker = read_marker(repository)?.ok_or_else(|| {
        unsupported("此操作不是由 GitViews 发起，缺少操作前记录；请使用发起操作的 Git 工具中止。")
    })?;
    let original = repository.find_commit(marker.original).map_err(git_error)?;
    let target_tree = original.tree().map_err(git_error)?;
    check_tree_paths(repository, &target_tree)?;
    let current_tree = repository
        .head()
        .and_then(|head| head.peel_to_commit())
        .and_then(|commit| commit.tree())
        .map_err(git_error)?;
    check_ignored_collisions(repository, &current_tree, &target_tree)?;
    let mut lease = IndexTransaction::prepare(repository).map_err(index_error)?;
    check_head(repository, &request.expected_head)?;
    backup_changes(repository)?;
    match repository.state() {
        RepositoryState::Merge | RepositoryState::Clean
            if marker.kind == "merge" || repository.state() == RepositoryState::Clean =>
        {
            let mut references = repository.transaction().map_err(git_error)?;
            references.lock_ref("HEAD").map_err(git_error)?;
            if marker.branch != "HEAD" {
                references.lock_ref(&marker.branch).map_err(git_error)?;
            }
            check_head(repository, &request.expected_head)?;
            if request.expected_head.as_deref() != Some(&marker.original.to_string()) {
                return Err(concurrent("合并期间 HEAD 已变化，不能安全中止。"));
            }
            // Explicit Abort is the only operation that discards this round's
            // resolutions. Preserve all tracked local contents before FORCE.
            let mut checkout = CheckoutBuilder::new();
            checkout.force().overwrite_ignored(false);
            lease.verify().map_err(index_error)?;
            repository
                .checkout_tree(original.as_object(), Some(&mut checkout))
                .map_err(git_error)?;
            lease.commit().map_err(index_error)?;
            repository.cleanup_state().map_err(git_error)?;
        }
        RepositoryState::RebaseMerge if marker.kind == "rebase" => {
            validate_owned_rebase(repository, &marker)?;
            let mut references = repository.transaction().map_err(git_error)?;
            references.lock_ref("HEAD").map_err(git_error)?;
            references.lock_ref(&marker.branch).map_err(git_error)?;
            check_head(repository, &request.expected_head)?;
            if repository.refname_to_id(&marker.branch).ok() != Some(marker.original) {
                return Err(concurrent(
                    "原分支已被其他 Git 操作推进，已保留该分支及当前文件，不能中止覆盖。请先检查操作前备份。",
                ));
            }
            if !repository.head_detached().map_err(git_error)?
                && repository
                    .head()
                    .map_err(git_error)?
                    .name()
                    .map_err(git_error)?
                    != marker.branch
            {
                return Err(concurrent(
                    "HEAD 已被外部操作切换到其他分支，不能安全中止。",
                ));
            }
            validate_rebase_cleanup(repository)?;
            lease.verify().map_err(index_error)?;
            // git_rebase_abort force-resets the original branch without CAS.
            // Leave that locked ref unchanged and only restore files and HEAD.
            let mut checkout = CheckoutBuilder::new();
            checkout.force().overwrite_ignored(false);
            repository
                .checkout_tree(original.as_object(), Some(&mut checkout))
                .map_err(git_error)?;
            lease.commit().map_err(index_error)?;
            let reflog_signature = repository
                .signature()
                .or_else(|_| git2::Signature::now("GitViews", "gitviews@local"))
                .map_err(git_error)?;
            references
                .set_symbolic_target(
                    "HEAD",
                    &marker.branch,
                    Some(&reflog_signature),
                    "rebase: aborting (GitViews)",
                )
                .map_err(git_error)?;
            references.commit().map_err(git_error)?;
            cleanup_owned_rebase(repository)?;
        }
        _ => return Err(unsupported("操作状态与备份不一致，不能安全中止。")),
    }
    remove_marker(repository)?;
    Ok(ConflictOutcome {
        summary: format!(
            "已中止操作并回到原提交。解决过程的文件备份已保留，原提交备份：{}",
            marker.backup
        ),
    })
}

fn validate_rebase_cleanup(repository: &Repository) -> Result<Vec<PathBuf>, ConflictError> {
    let directory = repository.path().join("rebase-merge");
    let parent = fs::canonicalize(repository.path()).map_err(io_error)?;
    let metadata = fs::symlink_metadata(&directory).map_err(io_error)?;
    let canonical = fs::canonicalize(&directory).map_err(io_error)?;
    if !metadata.is_dir()
        || is_link(&metadata)
        || canonical.parent() != Some(parent.as_path())
        || canonical
            .file_name()
            .is_none_or(|name| name != "rebase-merge")
    {
        return Err(unsupported("变基元数据目录发生变化，不能安全清理。"));
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(&canonical).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(io_error)?;
        if !metadata.is_file() || is_link(&metadata) {
            return Err(unsupported(
                "变基元数据含有链接或非普通文件，不能安全清理。",
            ));
        }
        files.push(entry.path());
    }
    Ok(files)
}

fn cleanup_owned_rebase(repository: &Repository) -> Result<(), ConflictError> {
    for path in validate_rebase_cleanup(repository)? {
        fs::remove_file(path).map_err(io_error)?;
    }
    fs::remove_dir(repository.path().join("rebase-merge")).map_err(io_error)
}

#[derive(Clone)]
struct Marker {
    kind: String,
    original: Oid,
    branch: String,
    onto: Oid,
    backup: String,
}
fn create_marker(repository: &Repository, kind: &str, onto: Oid) -> Result<Marker, ConflictError> {
    let head = repository.head().map_err(git_error)?;
    let original = head
        .target()
        .ok_or_else(|| unsupported("仓库尚无首次提交。"))?;
    let branch = head.name().map_err(git_error)?.to_owned();
    let backup = format!(
        "refs/gitviews/backups/{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        original
    );
    repository
        .reference(&backup, original, false, "GitViews operation backup")
        .map_err(git_error)?;
    let marker = Marker {
        kind: kind.into(),
        original,
        branch,
        onto,
        backup,
    };
    let value = format!(
        "gitviews-v1\n{}\n{}\n{}\n{}\n{}\n",
        marker.kind, marker.original, marker.branch, marker.onto, marker.backup
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(repository.path().join(MARKER))
        .map_err(io_error)?;
    file.write_all(value.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(io_error)?;
    Ok(marker)
}
fn read_marker(repository: &Repository) -> Result<Option<Marker>, ConflictError> {
    let path = repository.path().join(MARKER);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    if !metadata.is_file() || is_link(&metadata) || metadata.len() > 16384 {
        return Err(unsupported("GitViews 操作记录异常，请先检查仓库元数据。"));
    }
    let text = fs::read_to_string(path).map_err(io_error)?;
    let lines: Vec<_> = text.lines().collect();
    if lines.len() != 6
        || lines[0] != "gitviews-v1"
        || !matches!(lines[1], "merge" | "rebase")
        || !lines[5].starts_with("refs/gitviews/backups/")
    {
        return Err(unsupported("GitViews 操作记录格式不支持。"));
    }
    let original = Oid::from_str(lines[2]).map_err(git_error)?;
    if repository.refname_to_id(lines[5]).ok() != Some(original) {
        return Err(concurrent("操作前备份引用已变化，不能继续此操作。"));
    }
    Ok(Some(Marker {
        kind: lines[1].into(),
        original,
        branch: lines[3].into(),
        onto: Oid::from_str(lines[4]).map_err(git_error)?,
        backup: lines[5].into(),
    }))
}
fn remove_marker(repository: &Repository) -> Result<(), ConflictError> {
    if read_marker(repository)?.is_some() {
        fs::remove_file(repository.path().join(MARKER)).map_err(io_error)?;
    }
    Ok(())
}
fn validate_owned_rebase(repository: &Repository, marker: &Marker) -> Result<(), ConflictError> {
    if marker.kind != "rebase" || repository.state() != RepositoryState::RebaseMerge {
        return Err(unsupported("此变基布局不受支持。"));
    }
    let directory = repository.path().join("rebase-merge");
    let metadata = fs::symlink_metadata(&directory).map_err(io_error)?;
    if !metadata.is_dir() || is_link(&metadata) || directory.join("interactive").exists() {
        return Err(unsupported(
            "交互式或异常布局的变基不受支持，且不会执行 exec 指令。",
        ));
    }
    if read_oid(&directory.join("orig-head"))? != marker.original
        || read_oid(&directory.join("onto"))? != marker.onto
    {
        return Err(concurrent("变基元数据已被外部操作更改。"));
    }
    let mut options = rebase_options();
    let mut rebase = repository
        .open_rebase(Some(&mut options))
        .map_err(git_error)?;
    if rebase.orig_head_name().map_err(git_error)? != Some(marker.branch.as_str()) {
        return Err(concurrent("变基分支记录已变化。"));
    }
    validate_picks(&mut rebase)
}
fn validate_picks(rebase: &mut git2::Rebase<'_>) -> Result<(), ConflictError> {
    for index in 0..rebase.len() {
        if rebase.nth(index).and_then(|operation| operation.kind())
            != Some(RebaseOperationType::Pick)
        {
            return Err(unsupported(
                "只支持普通 pick 变基；不会执行 exec、squash 或其他交互式指令。",
            ));
        }
    }
    Ok(())
}
fn rebase_options() -> RebaseOptions<'static> {
    let mut options = RebaseOptions::new();
    options.checkout_options(checkout());
    options
}
fn checkout() -> CheckoutBuilder<'static> {
    let mut checkout = CheckoutBuilder::new();
    checkout
        .safe()
        .allow_conflicts(true)
        .conflict_style_merge(true)
        .overwrite_ignored(false);
    checkout
}

fn require_clean(repository: &Repository) -> Result<(), ConflictError> {
    if repository.state() != RepositoryState::Clean || read_marker(repository)?.is_some() {
        return Err(ConflictError::new(
            ConflictErrorKind::InProgress,
            "已有进行中的 Git 操作，请先完成或中止。",
        ));
    }
    let mut options = StatusOptions::new();
    options.include_untracked(true).recurse_untracked_dirs(true);
    if !repository
        .statuses(Some(&mut options))
        .map_err(git_error)?
        .is_empty()
    {
        return Err(ConflictError::new(
            ConflictErrorKind::Dirty,
            "请先提交或储藏当前改动及未跟踪文件，再发起合并或变基。",
        ));
    }
    signature(repository)?;
    Ok(())
}
fn require_no_unstaged(repository: &Repository) -> Result<(), ConflictError> {
    let diff = repository
        .diff_index_to_workdir(None, None)
        .map_err(git_error)?;
    if diff.deltas().len() != 0 {
        return Err(ConflictError::new(
            ConflictErrorKind::Dirty,
            "工作文件和暂存结果不同，请保存并暂存最终解决结果后继续。",
        ));
    }
    Ok(())
}
fn require_visible(repository: &Repository) -> Result<(), ConflictError> {
    let index = repository.index().map_err(git_error)?;
    if index
        .iter()
        .any(|entry| entry.flags & 0x8000 != 0 || entry.flags_extended & 0x4000 != 0)
    {
        return Err(unsupported(
            "请先取消 assume-unchanged 或 skip-worktree 标记，避免遗漏本地改动。",
        ));
    }
    for directory in [repository.path(), repository.commondir()] {
        for file in ["index.lock", "HEAD.lock", "MERGE_HEAD.lock"] {
            if fs::symlink_metadata(directory.join(file)).is_ok() {
                return Err(concurrent("仓库正被其他 Git 操作锁定，请稍后重新读取。"));
            }
        }
    }
    Ok(())
}
fn check_candidate(repository: &Repository, index: &Index) -> Result<(), ConflictError> {
    let mut paths = BTreeSet::new();
    for entry in index.iter() {
        let path = std::str::from_utf8(&entry.path)
            .map_err(|_| unsupported("非 UTF-8 路径不支持安全操作。"))?;
        checked_path(repository, path)?;
        paths.insert(path.to_owned());
    }
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .include_ignored(true)
        .recurse_untracked_dirs(true)
        .recurse_ignored_dirs(true)
        .disable_pathspec_match(true);
    for path in paths {
        options.pathspec(path);
    }
    if !repository
        .statuses(Some(&mut options))
        .map_err(git_error)?
        .is_empty()
    {
        return Err(ConflictError::new(
            ConflictErrorKind::Dirty,
            "目标路径存在本地、未跟踪或忽略文件，已停止操作以保留这些内容。",
        ));
    }
    Ok(())
}
fn check_ignored_collisions(
    repository: &Repository,
    old: &git2::Tree<'_>,
    new: &git2::Tree<'_>,
) -> Result<(), ConflictError> {
    let mut paths = BTreeSet::new();
    for delta in repository
        .diff_tree_to_tree(Some(old), Some(new), None)
        .map_err(git_error)?
        .deltas()
    {
        for file in [delta.old_file(), delta.new_file()] {
            if let Some(path) = file.path() {
                paths.insert(path.to_string_lossy().into_owned());
            }
        }
    }
    new.walk(git2::TreeWalkMode::PreOrder, |prefix, entry| {
        if let Ok(name) = entry.name() {
            paths.insert(format!("{prefix}{name}"));
        }
        if entry.filemode() == 0o160000 {
            git2::TreeWalkResult::Skip
        } else {
            git2::TreeWalkResult::Ok
        }
    })
    .map_err(git_error)?;
    if paths.is_empty() {
        return Ok(());
    }
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .include_ignored(true)
        .recurse_untracked_dirs(true)
        .recurse_ignored_dirs(true)
        .disable_pathspec_match(true);
    for path in paths {
        options.pathspec(path);
    }
    for entry in repository
        .statuses(Some(&mut options))
        .map_err(git_error)?
        .iter()
    {
        if entry
            .status()
            .intersects(git2::Status::WT_NEW | git2::Status::IGNORED)
        {
            return Err(ConflictError::new(
                ConflictErrorKind::Dirty,
                "目标路径存在未跟踪或忽略文件，请先保存这些文件后重试。",
            ));
        }
    }
    Ok(())
}
fn check_tree_paths(repository: &Repository, tree: &git2::Tree<'_>) -> Result<(), ConflictError> {
    let mut failure = None;
    tree.walk(git2::TreeWalkMode::PreOrder, |prefix, entry| {
        let result = entry
            .name()
            .map_err(git_error)
            .and_then(|name| checked_path(repository, &format!("{prefix}{name}")));
        if let Err(error) = result {
            failure = Some(error);
            git2::TreeWalkResult::Abort
        } else if entry.filemode() == 0o160000 {
            git2::TreeWalkResult::Skip
        } else {
            git2::TreeWalkResult::Ok
        }
    })
    .map_err(|error| failure.unwrap_or_else(|| git_error(error)))?;
    Ok(())
}
fn checked_path(repository: &Repository, path: &str) -> Result<PathBuf, ConflictError> {
    if path.is_empty() || path.contains(['\0', '\\']) {
        return Err(unsupported("文件路径无效。"));
    }
    let mut full = repository
        .workdir()
        .ok_or_else(|| unsupported("裸仓库不支持工作文件操作。"))?
        .to_path_buf();
    let parts: Vec<_> = Path::new(path).components().collect();
    for part in parts {
        let Component::Normal(name) = part else {
            return Err(unsupported("文件路径超出工作树。"));
        };
        let text = name
            .to_str()
            .ok_or_else(|| unsupported("文件路径不是 UTF-8。"))?;
        if text
            .trim_end_matches([' ', '.'])
            .eq_ignore_ascii_case(".git")
            || cfg!(windows) && (text.contains(':') || text.ends_with([' ', '.']))
        {
            return Err(unsupported("不能处理 Git 元数据路径或 Windows 路径别名。"));
        }
        full.push(name);
        match fs::symlink_metadata(&full) {
            Ok(metadata) if is_link(&metadata) => {
                return Err(unsupported(
                    "文件路径包含链接或重解析点，请使用 Git 专门处理。",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(full)
}
fn require_regular(entry: &IndexEntry, path: &str) -> Result<(), ConflictError> {
    if !matches!(entry.mode, 0o100644 | 0o100755) {
        return Err(unsupported(format!(
            "{path} 包含符号链接、子模块或类型冲突，请使用 Git 专门处理。"
        )));
    }
    Ok(())
}
fn read_file(path: &Path) -> Result<Option<Vec<u8>>, ConflictError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || is_link(&metadata) || metadata.len() > LIMIT as u64 {
                return Err(unsupported(
                    "工作文件不是普通文件或超过 8 MiB，请使用外部工具处理。",
                ));
            }
            fs::read(path).map(Some).map_err(io_error)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(error)),
    }
}
fn replace_file(path: &Path, bytes: &[u8]) -> Result<(), ConflictError> {
    if fs::metadata(path).is_ok_and(|metadata| metadata.permissions().readonly()) {
        return Err(unsupported(
            "工作文件为只读，已保留原内容和暂存区；请检查文件权限后再保存。",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| unsupported("文件父目录无效。"))?;
    fs::create_dir_all(parent).map_err(io_error)?;
    let temporary = parent.join(format!(
        ".gitviews-merge-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(io_error)?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(io_error(error));
    }
    drop(file);
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(&temporary, metadata.permissions()).map_err(io_error)?;
    }
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        io_error(error)
    })
}
fn backup_file(
    repository: &Repository,
    path: &str,
    bytes: Option<&[u8]>,
) -> Result<PathBuf, ConflictError> {
    let directory = repository.path().join("gitviews-conflict-backups");
    if !directory.exists() {
        fs::create_dir(&directory).map_err(io_error)?;
    }
    let metadata = fs::symlink_metadata(&directory).map_err(io_error)?;
    if !metadata.is_dir() || is_link(&metadata) {
        return Err(unsupported("冲突备份目录包含链接，已停止操作。"));
    }
    let name = format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let target = directory.join(format!(
        "{name}.{}",
        if bytes.is_some() { "blob" } else { "deleted" }
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .map_err(io_error)?;
    file.write_all(bytes.unwrap_or_default())
        .and_then(|()| file.sync_all())
        .map_err(io_error)?;
    let mut manifest = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(format!("{name}.path")))
        .map_err(io_error)?;
    manifest
        .write_all(path.as_bytes())
        .and_then(|()| manifest.sync_all())
        .map_err(io_error)?;
    Ok(target)
}
fn backup_changes(repository: &Repository) -> Result<(), ConflictError> {
    let index_bytes = fs::read(repository.path().join("index")).map_err(io_error)?;
    backup_file(repository, "@@git-index@@", Some(&index_bytes))?;
    let mut options = StatusOptions::new();
    options.include_untracked(false).include_ignored(false);
    for entry in repository
        .statuses(Some(&mut options))
        .map_err(git_error)?
        .iter()
    {
        let path = entry.path().map_err(git_error)?;
        let full = checked_path(repository, path)?;
        backup_file(repository, path, read_file(&full)?.as_deref())?;
    }
    Ok(())
}
fn target_commit(repository: &Repository, value: &str) -> Result<Oid, ConflictError> {
    if value.is_empty() || value.contains(['\0', '\n', '\r']) || value.starts_with('-') {
        return Err(unsupported("请填写有效的分支名或提交 ID。"));
    }
    repository
        .revparse_single(value)
        .and_then(|object| object.peel_to_commit())
        .map(|commit| commit.id())
        .map_err(git_error)
}
fn read_oid(path: &Path) -> Result<Oid, ConflictError> {
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_file() || is_link(&metadata) || metadata.len() > 128 {
        return Err(unsupported("Git 操作元数据无效或包含多个提交。"));
    }
    Oid::from_str(fs::read_to_string(path).map_err(io_error)?.trim()).map_err(git_error)
}
fn conflict_count(repository: &Repository) -> Result<usize, ConflictError> {
    Ok(repository
        .index()
        .map_err(git_error)?
        .conflicts()
        .map_err(git_error)?
        .count())
}
fn signature(repository: &Repository) -> Result<git2::Signature<'static>, ConflictError> {
    match repository
        .config()
        .map_err(git_error)?
        .get_bool("commit.gpgsign")
    {
        Ok(true) => {
            return Err(unsupported(
                "此仓库要求签名提交；应用尚未支持签名，请使用已配置签名的 Git 工具完成合并或变基。",
            ));
        }
        Ok(false) => {}
        Err(error) if error.code() == ErrorCode::NotFound => {}
        Err(error) => return Err(git_error(error)),
    }
    let signature = repository.signature().map_err(|_| {
        ConflictError::new(
            ConflictErrorKind::IdentityRequired,
            "请先在 Git 配置中填写用户名和邮箱。",
        )
    })?;
    if !signature.name().is_ok_and(|value| !value.trim().is_empty())
        || !signature
            .email()
            .is_ok_and(|value| !value.trim().is_empty())
    {
        return Err(ConflictError::new(
            ConflictErrorKind::IdentityRequired,
            "提交姓名或邮箱为空，请先在 Git 配置中补全。",
        ));
    }
    Ok(signature)
}
fn head(repository: &Repository) -> Result<Option<String>, ConflictError> {
    match repository.head() {
        Ok(head) => Ok(head.target().map(|id| id.to_string())),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
            Ok(None)
        }
        Err(error) => Err(git_error(error)),
    }
}
fn check_head(repository: &Repository, expected: &Option<String>) -> Result<(), ConflictError> {
    if &head(repository)? != expected {
        return Err(concurrent("HEAD 已变化，请重新读取操作状态。"));
    }
    Ok(())
}
fn open(root: &Path) -> Result<Repository, ConflictError> {
    let invalid = || {
        ConflictError::new(
            ConflictErrorKind::InvalidRepository,
            "请选择有效仓库或工作树的根目录。",
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
fn index_error(error: crate::CheckoutError) -> ConflictError {
    ConflictError::new(
        if error.kind == crate::CheckoutErrorKind::ConcurrentChange {
            ConflictErrorKind::ConcurrentChange
        } else {
            ConflictErrorKind::Failed
        },
        error.message,
    )
}
fn unsupported(message: impl Into<String>) -> ConflictError {
    ConflictError::new(ConflictErrorKind::Unsupported, message)
}
fn concurrent(message: impl Into<String>) -> ConflictError {
    ConflictError::new(ConflictErrorKind::ConcurrentChange, message)
}
fn io_error(error: std::io::Error) -> ConflictError {
    ConflictError::new(
        ConflictErrorKind::Failed,
        format!("文件操作未完成：{error}"),
    )
}
fn git_error(error: git2::Error) -> ConflictError {
    ConflictError::new(
        match error.code() {
            ErrorCode::Locked | ErrorCode::Modified => ConflictErrorKind::ConcurrentChange,
            ErrorCode::NotFound => ConflictErrorKind::NotFound,
            _ => ConflictErrorKind::Failed,
        },
        format!("Git 操作未完成：{}", error.message()),
    )
}
