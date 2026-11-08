//! Index-only writes and commits. No worktree file is changed and no hook or
//! external executable is invoked. The real index stays locked while libgit2
//! edits a private copy, and only a complete successful edit is published.

use std::{
    collections::BTreeSet,
    error::Error,
    fmt, fs,
    path::{Component, Path},
};

use git2::{
    ApplyLocation, ApplyOptions, Diff, DiffOptions, ErrorCode, Index, Oid, Patch, Repository,
    RepositoryState, Status, StatusOptions, Tree,
};

use crate::checkout::{CheckoutError, CheckoutErrorKind, index_transaction::IndexTransaction};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WriteAction {
    StagePaths {
        paths: Vec<String>,
    },
    UnstagePaths {
        paths: Vec<String>,
    },
    StageAll,
    UnstageAll,
    /// Full FileDiff patch header and one or more unchanged original hunks.
    StageHunk {
        path: String,
        patch: String,
    },
    /// A forward HEAD-to-index patch; the selected hunks are removed from index.
    UnstageHunk {
        path: String,
        patch: String,
    },
    Commit {
        summary: String,
        description: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteRequest {
    /// None means an unborn HEAD; it never disables the HEAD identity check.
    pub expected_head: Option<String>,
    pub action: WriteAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteOutcome {
    pub summary: String,
    pub commit_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteErrorKind {
    Unsupported,
    InvalidRepository,
    InvalidRequest,
    InvalidPath,
    ConcurrentChange,
    InProgress,
    Conflict,
    IdentityMissing,
    NothingToCommit,
    PatchStale,
    WriteFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteError {
    pub kind: WriteErrorKind,
    pub message: String,
}

impl WriteError {
    pub fn new(kind: WriteErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}
impl fmt::Display for WriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl Error for WriteError {}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HeadState {
    reference: String,
    oid: Option<Oid>,
}

pub(super) fn execute(root: &Path, request: &WriteRequest) -> Result<WriteOutcome, WriteError> {
    let root = fs::canonicalize(root).map_err(|_| invalid_repository())?;
    let repository = Repository::open(&root).map_err(|_| invalid_repository())?;
    if repository.is_bare()
        || repository
            .workdir()
            .and_then(|path| fs::canonicalize(path).ok())
            .as_ref()
            != Some(&root)
    {
        return Err(invalid_repository());
    }
    validate_request(request)?;
    let original = checked_head(&repository, request)?;
    let mut index_transaction = IndexTransaction::prepare(&repository).map_err(index_error)?;
    let mut references = repository.transaction().map_err(git_error)?;
    references.lock_ref("HEAD").map_err(git_error)?;
    if original.reference != "HEAD" {
        references
            .lock_ref(&original.reference)
            .map_err(git_error)?;
    }
    if checked_head(&repository, request)? != original {
        return Err(concurrent_change());
    }
    require_ready(&repository)?;
    let mut index = repository.index().map_err(git_error)?;
    index.read(true).map_err(git_error)?;
    if index.has_conflicts() {
        return Err(WriteError::new(
            WriteErrorKind::Conflict,
            "暂存区仍有冲突，请先解决冲突。",
        ));
    }
    // Hidden entries can conceal worktree modifications. Do not silently stage
    // sparse/assume-unchanged data or remove these flags as a side effect.
    if index
        .iter()
        .any(|entry| entry.flags & 0x8000 != 0 || entry.flags_extended & 0x4000 != 0)
    {
        return Err(WriteError::new(
            WriteErrorKind::Unsupported,
            "暂存区包含 assume-unchanged 或 skip-worktree 文件，请先检查并取消这些标记。",
        ));
    }
    let head_tree = original
        .oid
        .map(|oid| repository.find_commit(oid).and_then(|commit| commit.tree()))
        .transpose()
        .map_err(git_error)?;
    if let WriteAction::Commit {
        summary,
        description,
    } = &request.action
    {
        let config = repository.config().map_err(git_error)?;
        match config.get_bool("commit.gpgsign") {
            Ok(true) => {
                return Err(WriteError::new(
                    WriteErrorKind::Unsupported,
                    "此仓库要求签名提交；应用尚未支持提交签名，请使用已配置签名的 Git 工具提交。",
                ));
            }
            Ok(false) => {}
            Err(error) if error.code() == ErrorCode::NotFound => {}
            Err(error) => return Err(git_error(error)),
        }
        let signature = repository.signature().map_err(|_| {
            WriteError::new(
                WriteErrorKind::IdentityMissing,
                "尚未配置提交身份，请在 Git 配置中填写 user.name 和 user.email。",
            )
        })?;
        if !signature.name().is_ok_and(|value| !value.trim().is_empty())
            || !signature
                .email()
                .is_ok_and(|value| !value.trim().is_empty())
        {
            return Err(WriteError::new(
                WriteErrorKind::IdentityMissing,
                "提交姓名或邮箱为空，请在 Git 配置中填写 user.name 和 user.email。",
            ));
        }
        // ITA is a working-tree placeholder, never staged content. libgit2's
        // tree writer does not interpret that flag, so omit placeholders from
        // this private in-memory index only. The on-disk index stays intact.
        let intent_paths: Vec<_> = index
            .iter()
            .filter(|entry| entry.flags_extended & 0x2000 != 0)
            .map(|entry| String::from_utf8(entry.path))
            .collect::<Result<_, _>>()
            .map_err(|_| {
                WriteError::new(WriteErrorKind::InvalidPath, "暂存区包含非 UTF-8 路径。")
            })?;
        for path in intent_paths {
            index.remove_path(Path::new(&path)).map_err(git_error)?;
        }
        let tree_id = index.write_tree().map_err(git_error)?;
        if head_tree.as_ref().is_some_and(|tree| tree.id() == tree_id)
            || head_tree.is_none() && index.is_empty()
        {
            return Err(WriteError::new(
                WriteErrorKind::NothingToCommit,
                "没有已暂存的改动可提交。",
            ));
        }
        let tree = repository.find_tree(tree_id).map_err(git_error)?;
        let parent = original
            .oid
            .map(|oid| repository.find_commit(oid))
            .transpose()
            .map_err(git_error)?;
        let parents: Vec<_> = parent.iter().collect();
        let message = if description.trim().is_empty() {
            summary.trim().to_owned()
        } else {
            format!("{}\n\n{}", summary.trim(), description.trim())
        };
        // Create an immutable object, then update the single locked ref. Never
        // rewrite index/worktree, and never move HEAD before the commit exists.
        let oid = repository
            .commit(None, &signature, &signature, &message, &tree, &parents)
            .map_err(git_error)?;
        if checked_head(&repository, request)? != original {
            return Err(concurrent_change());
        }
        require_ready(&repository)?;
        index_transaction.verify().map_err(index_error)?;
        let reflog_message = format!("commit: {}", summary.trim());
        // libgit2 appends the symbolic HEAD reflog when its branch is updated.
        // Keep this a single ref update; explicitly writing HEAD's log as well
        // would duplicate entries and ignore logAllRefUpdates configuration.
        references
            .set_target(&original.reference, oid, Some(&signature), &reflog_message)
            .map_err(git_error)?;
        references.commit().map_err(git_error)?;
        return Ok(WriteOutcome {
            summary: format!("已提交 {}", &oid.to_string()[..8]),
            commit_id: Some(oid.to_string()),
        });
    }
    let summary = match &request.action {
        WriteAction::StagePaths { paths } => {
            stage_paths(&repository, &root, &mut index, paths)?;
            format!("已暂存 {} 个文件", paths.len())
        }
        WriteAction::UnstagePaths { paths } => {
            let expanded = unstage_rename_paths(&repository, &index, head_tree.as_ref(), paths)?;
            unstage_paths(&mut index, head_tree.as_ref(), &expanded)?;
            format!("已取消暂存 {} 个文件", paths.len())
        }
        WriteAction::StageAll => {
            let paths = unstaged_paths(&repository)?;
            stage_paths(&repository, &root, &mut index, &paths)?;
            format!("已暂存 {} 个文件", paths.len())
        }
        WriteAction::UnstageAll => {
            match &head_tree {
                Some(tree) => index.read_tree(tree).map_err(git_error)?,
                None => index.clear().map_err(git_error)?,
            }
            "已取消全部暂存".into()
        }
        WriteAction::StageHunk { path, patch } => {
            apply_hunks(
                &repository,
                &root,
                &mut index,
                head_tree.as_ref(),
                path,
                patch,
                false,
            )?;
            // git_apply replaces the repository's in-memory index with a fresh
            // instance. Re-read it so the final write does not undo its result.
            index = repository.index().map_err(git_error)?;
            "已暂存所选代码块".into()
        }
        WriteAction::UnstageHunk { path, patch } => {
            apply_hunks(
                &repository,
                &root,
                &mut index,
                head_tree.as_ref(),
                path,
                patch,
                true,
            )?;
            index = repository.index().map_err(git_error)?;
            "已取消暂存所选代码块".into()
        }
        WriteAction::Commit { .. } => unreachable!(),
    };
    index.write().map_err(git_error)?;
    if checked_head(&repository, request)? != original {
        return Err(concurrent_change());
    }
    require_ready(&repository)?;
    index_transaction.commit().map_err(index_error)?;
    Ok(WriteOutcome {
        summary,
        commit_id: None,
    })
}

fn validate_request(request: &WriteRequest) -> Result<(), WriteError> {
    match &request.action {
        WriteAction::StagePaths { paths } | WriteAction::UnstagePaths { paths } => {
            if paths.is_empty() || paths.len() > 100_000 {
                return Err(WriteError::new(
                    WriteErrorKind::InvalidRequest,
                    "请选择 1 至 100000 个文件。",
                ));
            }
            let mut unique = BTreeSet::new();
            for path in paths {
                validate_path(path)?;
                if !unique.insert(path) {
                    return Err(WriteError::new(
                        WriteErrorKind::InvalidRequest,
                        "文件列表包含重复路径。",
                    ));
                }
            }
        }
        WriteAction::StageHunk { path, patch } | WriteAction::UnstageHunk { path, patch } => {
            validate_path(path)?;
            if patch.is_empty() || patch.len() > 16 * 1024 * 1024 || patch.contains('\0') {
                return Err(WriteError::new(
                    WriteErrorKind::InvalidRequest,
                    "代码块补丁为空、过大或包含无效数据。",
                ));
            }
        }
        WriteAction::Commit {
            summary,
            description,
        } => {
            if summary.trim().is_empty()
                || summary.contains(['\0', '\r', '\n'])
                || summary.len().saturating_add(description.len()) > 1024 * 1024
                || description.contains('\0')
            {
                return Err(WriteError::new(
                    WriteErrorKind::InvalidRequest,
                    "请填写单行提交说明；提交说明不能包含 NUL，且总长度不能超过 1 MiB。",
                ));
            }
        }
        WriteAction::StageAll | WriteAction::UnstageAll => {}
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<(), WriteError> {
    if path.is_empty()
        || path.len() > 32 * 1024
        || path.contains(['\0', '\\', ':', '\r', '\n'])
        || path.chars().any(|value| value.is_control())
        || path.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || part.eq_ignore_ascii_case(".git")
                || part.ends_with([' ', '.'])
                || reserved_device(part)
        })
        || Path::new(path)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(WriteError::new(
            WriteErrorKind::InvalidPath,
            "文件路径必须位于当前仓库内，且不能包含父目录、Git 元数据或不安全的路径组成。",
        ));
    }
    Ok(())
}

fn reserved_device(component: &str) -> bool {
    #[cfg(windows)]
    {
        if component.contains(['<', '>', '"', '|', '?', '*']) {
            return true;
        }
        let stem = component
            .split('.')
            .next()
            .unwrap_or(component)
            .to_ascii_uppercase();
        matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
    }
    #[cfg(not(windows))]
    {
        let _ = component;
        false
    }
}

fn checked_head(repository: &Repository, request: &WriteRequest) -> Result<HeadState, WriteError> {
    let raw = repository.find_reference("HEAD").map_err(git_error)?;
    let reference = raw
        .symbolic_target()
        .map_err(git_error)?
        .unwrap_or("HEAD")
        .to_owned();
    if reference != "HEAD" && !reference.starts_with("refs/heads/") {
        return Err(WriteError::new(
            WriteErrorKind::Unsupported,
            "HEAD 未指向本地分支，无法安全更新。",
        ));
    }
    let oid = match raw.resolve() {
        Ok(reference) => Some(reference.target().ok_or_else(concurrent_change)?),
        Err(error) if error.code() == ErrorCode::NotFound && reference != "HEAD" => None,
        Err(error) => return Err(git_error(error)),
    };
    let expected = request
        .expected_head
        .as_deref()
        .map(Oid::from_str)
        .transpose()
        .map_err(|_| WriteError::new(WriteErrorKind::InvalidRequest, "预期 HEAD 标识无效。"))?;
    if expected != oid {
        return Err(concurrent_change());
    }
    Ok(HeadState { reference, oid })
}

fn require_ready(repository: &Repository) -> Result<(), WriteError> {
    if repository.state() != RepositoryState::Clean {
        return Err(WriteError::new(
            WriteErrorKind::InProgress,
            "仓库正在合并、变基或执行其他未完成操作；请先完成或中止。",
        ));
    }
    Ok(())
}

fn unstaged_paths(repository: &Repository) -> Result<Vec<String>, WriteError> {
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .renames_head_to_index(false)
        .renames_index_to_workdir(false);
    let statuses = repository.statuses(Some(&mut options)).map_err(git_error)?;
    if statuses.len() > 100_000 {
        return Err(WriteError::new(
            WriteErrorKind::Unsupported,
            "改动文件超过 100000 个，请分批暂存。",
        ));
    }
    let mut paths = statuses
        .iter()
        .filter(|status| {
            status.status().intersects(
                Status::WT_NEW
                    | Status::WT_MODIFIED
                    | Status::WT_DELETED
                    | Status::WT_TYPECHANGE
                    | Status::WT_RENAMED,
            )
        })
        .map(|status| {
            status.path().map(str::to_owned).map_err(|_| {
                WriteError::new(
                    WriteErrorKind::InvalidPath,
                    "改动中包含无法表示为 UTF-8 的路径，请单独处理这些文件。",
                )
            })
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    // libgit2 can report an empty intent-to-add entry as INDEX_NEW without a
    // WT_* bit. Git's add -A still stages that empty file (or its deletion).
    let index = repository.index().map_err(git_error)?;
    for entry in index
        .iter()
        .filter(|entry| entry.flags_extended & 0x2000 != 0)
    {
        paths.insert(String::from_utf8(entry.path).map_err(|_| {
            WriteError::new(
                WriteErrorKind::InvalidPath,
                "改动中包含无法表示为 UTF-8 的路径，请单独处理这些文件。",
            )
        })?);
    }
    if paths.len() > 100_000 {
        return Err(WriteError::new(
            WriteErrorKind::Unsupported,
            "改动文件超过 100000 个，请分批暂存。",
        ));
    }
    Ok(paths.into_iter().collect())
}

fn stage_paths(
    repository: &Repository,
    root: &Path,
    index: &mut Index,
    paths: &[String],
) -> Result<(), WriteError> {
    // Validate the entire batch before modifying even the private index.
    for path in paths {
        validate_path(path)?;
        checked_file(root, index, path)?;
        if index.get_path(Path::new(path), 0).is_none()
            && repository
                .is_path_ignored(Path::new(path))
                .map_err(git_error)?
        {
            return Err(WriteError::new(
                WriteErrorKind::InvalidPath,
                "所选文件被 Git 忽略，未强制添加。",
            ));
        }
    }
    for path in paths {
        match fs::symlink_metadata(root.join(path)) {
            Ok(_) => {
                index.add_path(Path::new(path)).map_err(git_error)?;
                // Explicit staging turns the placeholder into a real staged
                // file. Retain all unrelated index flags; hidden flags were
                // rejected before the private-index transaction was edited.
                if let Some(mut entry) = index.get_path(Path::new(path), 0)
                    && entry.flags_extended & 0x2000 != 0
                {
                    entry.flags_extended &= !0x2000;
                    if entry.flags_extended == 0 {
                        entry.flags &= !0x4000;
                    }
                    index.add(&entry).map_err(git_error)?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if index.get_path(Path::new(path), 0).is_none() {
                    return Err(WriteError::new(
                        WriteErrorKind::InvalidPath,
                        "所选文件已经不存在，请刷新改动列表。",
                    ));
                }
                index.remove_path(Path::new(path)).map_err(git_error)?;
            }
            Err(_) => {
                return Err(WriteError::new(
                    WriteErrorKind::InvalidPath,
                    "无法读取所选文件。",
                ));
            }
        }
    }
    Ok(())
}

fn checked_file(root: &Path, index: &Index, path: &str) -> Result<(), WriteError> {
    let mut candidate = root.to_owned();
    let components: Vec<_> = Path::new(path).components().collect();
    for (position, component) in components.iter().enumerate() {
        candidate.push(component.as_os_str());
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                if is_link(&metadata) {
                    return Err(WriteError::new(
                        WriteErrorKind::Unsupported,
                        "此路径包含符号链接或重解析点，请单独处理以防读取仓库外的文件。",
                    ));
                }
                // Windows 8.3 aliases can resolve to a protected name although
                // its spelling was harmless. Verify the actual component too.
                let canonical = fs::canonicalize(&candidate).map_err(|_| {
                    WriteError::new(WriteErrorKind::InvalidPath, "无法确定文件的实际路径。")
                })?;
                if canonical.strip_prefix(root).ok().is_none_or(|relative| {
                    relative.components().any(|part| {
                        part.as_os_str()
                            .to_str()
                            .is_none_or(|name| name.eq_ignore_ascii_case(".git"))
                    })
                }) {
                    return Err(WriteError::new(
                        WriteErrorKind::InvalidPath,
                        "实际文件路径越出当前仓库或进入 Git 元数据目录。",
                    ));
                }
                if position + 1 < components.len() && !metadata.is_dir() {
                    return Err(WriteError::new(
                        WriteErrorKind::InvalidPath,
                        "文件路径的父目录无效。",
                    ));
                }
                if position + 1 < components.len() && candidate.join(".git").exists() {
                    return Err(WriteError::new(
                        WriteErrorKind::InvalidPath,
                        "不能跨入子模块或嵌套仓库暂存文件，请先进入该仓库。",
                    ));
                }
                if position + 1 == components.len() && !metadata.is_file() {
                    // A registered gitlink is an atomic parent-repository
                    // entry; staging it records only the child HEAD.
                    if !metadata.is_dir()
                        || index
                            .get_path(Path::new(path), 0)
                            .is_none_or(|entry| entry.mode != 0o160000)
                    {
                        return Err(WriteError::new(
                            WriteErrorKind::InvalidPath,
                            "请选择普通文件或已有子模块入口，不能递归添加目录或嵌套仓库。",
                        ));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => {
                return Err(WriteError::new(
                    WriteErrorKind::InvalidPath,
                    "无法安全读取文件路径。",
                ));
            }
        }
    }
    Ok(())
}

/// Status displays a staged rename as one destination row. Reset both sides
/// of that row, rather than leaving an unexpected staged deletion behind.
fn unstage_rename_paths(
    repository: &Repository,
    index: &Index,
    tree: Option<&Tree<'_>>,
    paths: &[String],
) -> Result<Vec<String>, WriteError> {
    let mut expanded: BTreeSet<_> = paths.iter().cloned().collect();
    let mut options = DiffOptions::new();
    options.include_typechange(true).update_index(false);
    let mut comparison = repository
        .diff_tree_to_index(tree, Some(index), Some(&mut options))
        .map_err(git_error)?;
    let additions = comparison
        .deltas()
        .any(|delta| matches!(delta.status(), git2::Delta::Added | git2::Delta::Typechange));
    let deletions = comparison.deltas().any(|delta| {
        matches!(
            delta.status(),
            git2::Delta::Deleted | git2::Delta::Typechange
        )
    });
    if !additions || !deletions {
        return Ok(expanded.into_iter().collect());
    }
    // Match the preview/status rename bound: an unrelated large candidate
    // keeps its honest A/D rows and cannot force loading a large blob here.
    let odb = repository.odb().map_err(git_error)?;
    for delta in comparison.deltas().filter(|delta| {
        matches!(
            delta.status(),
            git2::Delta::Added | git2::Delta::Deleted | git2::Delta::Typechange
        )
    }) {
        for file in [delta.old_file(), delta.new_file()] {
            let mode: u32 = file.mode().into();
            if file.id().is_zero() || !matches!(mode & 0o170000, 0o100000 | 0o120000) {
                continue;
            }
            if odb.read_header(file.id()).map_err(git_error)?.0 > 2 * 1024 * 1024 {
                return Ok(expanded.into_iter().collect());
            }
        }
    }
    comparison
        .find_similar(Some(
            git2::DiffFindOptions::new().renames(true).rename_limit(200),
        ))
        .map_err(git_error)?;
    for delta in comparison
        .deltas()
        .filter(|delta| delta.status() == git2::Delta::Renamed)
    {
        if delta
            .new_file()
            .path()
            .and_then(Path::to_str)
            .is_some_and(|path| expanded.contains(path))
        {
            let source = delta
                .old_file()
                .path()
                .and_then(Path::to_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    WriteError::new(
                        WriteErrorKind::InvalidPath,
                        "重命名来源路径不能表示为 UTF-8，请单独处理。",
                    )
                })?;
            validate_path(&source)?;
            expanded.insert(source);
        }
    }
    Ok(expanded.into_iter().collect())
}

fn unstage_paths(
    index: &mut Index,
    tree: Option<&Tree<'_>>,
    paths: &[String],
) -> Result<(), WriteError> {
    for path in paths {
        if let Some(entry) = tree.and_then(|tree| tree.get_path(Path::new(path)).ok()) {
            // Git tree entries have no mutable stat cache; copy the current
            // index entry when possible and invalidate its cached timestamps.
            if entry.kind() == Some(git2::ObjectType::Tree) {
                return Err(WriteError::new(
                    WriteErrorKind::InvalidPath,
                    "取消暂存需要选择文件，不能选择目录。",
                ));
            }
            let mut value =
                index
                    .get_path(Path::new(path), 0)
                    .unwrap_or_else(|| git2::IndexEntry {
                        ctime: git2::IndexTime::new(0, 0),
                        mtime: git2::IndexTime::new(0, 0),
                        dev: 0,
                        ino: 0,
                        mode: 0,
                        uid: 0,
                        gid: 0,
                        file_size: 0,
                        id: Oid::ZERO_SHA1,
                        flags: 0,
                        flags_extended: 0,
                        path: path.as_bytes().to_vec(),
                    });
            value.id = entry.id();
            value.mode = entry.filemode() as u32;
            value.ctime = git2::IndexTime::new(0, 0);
            value.mtime = git2::IndexTime::new(0, 0);
            value.file_size = 0;
            index.add(&value).map_err(git_error)?;
        } else if index.get_path(Path::new(path), 0).is_some() {
            index.remove_path(Path::new(path)).map_err(git_error)?;
        }
    }
    Ok(())
}

fn hunk_parts(patch: &str) -> Result<(&str, Vec<&str>), WriteError> {
    let mut starts: Vec<_> = patch
        .match_indices("\n@@ ")
        .map(|(offset, _)| offset + 1)
        .collect();
    if starts.is_empty() {
        return Err(stale_patch());
    }
    let header = &patch[..starts[0]];
    starts.push(patch.len());
    let hunks = starts
        .windows(2)
        .map(|range| &patch[range[0]..range[1]])
        .collect();
    Ok((header, hunks))
}

#[allow(clippy::too_many_arguments)]
fn apply_hunks(
    repository: &Repository,
    root: &Path,
    index: &mut Index,
    tree: Option<&Tree<'_>>,
    path: &str,
    supplied: &str,
    staged: bool,
) -> Result<(), WriteError> {
    checked_file(root, index, path)?;
    let mut options = DiffOptions::new();
    options
        .pathspec(path)
        .disable_pathspec_match(true)
        .context_lines(3)
        .update_index(false)
        .include_typechange(true)
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true);
    let comparison = if staged {
        repository.diff_tree_to_index(tree, Some(index), Some(&mut options))
    } else {
        repository.diff_index_to_workdir(Some(index), Some(&mut options))
    }
    .map_err(git_error)?;
    let canonical = checked_patch(&comparison, path)?;
    let (header, hunks) = hunk_parts(&canonical)?;
    let (selected_header, selected_hunks) = hunk_parts(supplied)?;
    if header != selected_header {
        return Err(stale_patch());
    }
    let mut selected = BTreeSet::new();
    let mut after = 0;
    for hunk in selected_hunks {
        let position = hunks
            .iter()
            .enumerate()
            .skip(after)
            .find(|(_, candidate)| **candidate == hunk)
            .map(|(position, _)| position)
            .ok_or_else(stale_patch)?;
        selected.insert(position);
        after = position + 1;
    }
    let reverse;
    let normalized;
    let applied = if staged {
        options.reverse(true);
        reverse = repository
            .diff_tree_to_index(tree, Some(index), Some(&mut options))
            .map_err(git_error)?;
        &reverse
    } else {
        // Workdir diffs label new files Untracked, but git_apply expects Added.
        // Parse only the freshly rendered server patch, never client bytes.
        normalized = Diff::from_buffer(canonical.as_bytes()).map_err(git_error)?;
        &normalized
    };
    let mut position = 0;
    let mut apply = ApplyOptions::new();
    apply.hunk_callback(|_| {
        let keep = selected.contains(&position);
        position += 1;
        keep
    });
    repository
        .apply(applied, ApplyLocation::Index, Some(&mut apply))
        .map_err(|error| {
            WriteError::new(
                WriteErrorKind::PatchStale,
                format!("代码块无法安全应用，请刷新差异后重试：{}", error.message()),
            )
        })?;
    Ok(())
}

fn checked_patch(comparison: &Diff<'_>, path: &str) -> Result<String, WriteError> {
    if comparison.deltas().len() != 1 {
        return Err(stale_patch());
    }
    let delta = comparison.get_delta(0).ok_or_else(stale_patch)?;
    if delta
        .old_file()
        .path()
        .is_some_and(|file| file != Path::new(path))
        || delta
            .new_file()
            .path()
            .is_some_and(|file| file != Path::new(path))
        || !matches!(
            delta.status(),
            git2::Delta::Added
                | git2::Delta::Untracked
                | git2::Delta::Deleted
                | git2::Delta::Modified
        )
        || [delta.old_file().mode(), delta.new_file().mode()]
            .iter()
            .any(|mode| {
                !matches!(
                    mode,
                    git2::FileMode::Unreadable
                        | git2::FileMode::Blob
                        | git2::FileMode::BlobExecutable
                )
            })
    {
        return Err(WriteError::new(
            WriteErrorKind::Unsupported,
            "此改动包含重命名、符号链接、子模块或类型变化，请按文件暂存。",
        ));
    }
    if delta.status() == git2::Delta::Modified && delta.old_file().mode() != delta.new_file().mode()
    {
        return Err(WriteError::new(
            WriteErrorKind::Unsupported,
            "权限变化请按文件暂存。",
        ));
    }
    let mut patch = Patch::from_diff(comparison, 0)
        .map_err(git_error)?
        .ok_or_else(|| {
            WriteError::new(WriteErrorKind::Unsupported, "二进制文件不支持代码块暂存。")
        })?;
    if patch.size(true, true, true) > 16 * 1024 * 1024 {
        return Err(WriteError::new(
            WriteErrorKind::Unsupported,
            "补丁超过 16 MiB，请按文件暂存。",
        ));
    }
    let buffer = patch.to_buf().map_err(git_error)?;
    String::from_utf8(buffer.to_vec()).map_err(|_| {
        WriteError::new(
            WriteErrorKind::Unsupported,
            "此文本不是 UTF-8，请按文件暂存。",
        )
    })
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

fn stale_patch() -> WriteError {
    WriteError::new(
        WriteErrorKind::PatchStale,
        "代码块已变化、补丁基线不一致或无法安全应用；请刷新差异后重新选择。",
    )
}
fn concurrent_change() -> WriteError {
    WriteError::new(
        WriteErrorKind::ConcurrentChange,
        "仓库或暂存区已被其他操作更新，请刷新后重试。",
    )
}
fn invalid_repository() -> WriteError {
    WriteError::new(
        WriteErrorKind::InvalidRepository,
        "无法打开所选工作树根目录。",
    )
}
fn index_error(error: CheckoutError) -> WriteError {
    WriteError::new(
        if error.kind == CheckoutErrorKind::ConcurrentChange {
            WriteErrorKind::ConcurrentChange
        } else {
            WriteErrorKind::WriteFailed
        },
        error.message,
    )
}
fn git_error(error: git2::Error) -> WriteError {
    let kind = if matches!(error.code(), ErrorCode::Locked | ErrorCode::Modified) {
        WriteErrorKind::ConcurrentChange
    } else {
        WriteErrorKind::WriteFailed
    };
    WriteError::new(kind, format!("Git 操作未完成：{}", error.message()))
}
