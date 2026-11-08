//! Native Git access, independent of the application and its UI toolkit.
//!
//! [`GitBackend`] is the service boundary. The default [`Git2Backend`] reads
//! repositories through libgit2 without launching processes. [`GitCli`] remains
//! available as a compatibility backend for the original overview APIs.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

mod branch_manage;
mod checkout;
mod compatibility;
mod conflicts;
mod details;
mod diff;
mod git_settings;
mod graph_history;
mod history;
mod history_tools;
mod native;
mod paths;
mod remote;
mod repository_setup;
mod search_tree;
mod source;
mod stash;
mod system_git;
mod tree;
mod worktrees;
mod write_ops;

pub use branch_manage::{BranchAction, BranchError, BranchErrorKind, BranchOutcome, BranchRequest};
pub use checkout::{CheckoutError, CheckoutErrorKind, CheckoutOutcome, CheckoutRequest};
pub use conflicts::{
    ConflictAction, ConflictError, ConflictErrorKind, ConflictFile, ConflictOperation,
    ConflictOutcome, ConflictRequest, ConflictResolution, ConflictState, ConflictVersions,
};
pub use details::{BranchInfo, RepositoryDetails, WorktreeInfo};
pub use diff::{DiffLine, DiffLineKind, FileDiff};
pub use git_settings::{GitRemoteSettings, GitSettings};
pub use graph_history::{
    GraphHistoryFilter, GraphHistoryPage, GraphHistoryRequest, GraphHistoryRow,
    GraphHistorySnapshot, GraphReference, GraphReferenceKind,
};
pub use history::{CommitDetails, HistoryPage};
pub use history_tools::{
    BlameLine, BlameView, ComparedFile, ComparisonView, GraphCommit, HistoryGraphPage,
    HistoryToolsError, HistoryToolsErrorKind, HistoryToolsRequest, HistoryToolsResult,
    RecoveryEntry, RecoveryPage,
};
pub use native::Git2Backend;
pub use paths::{display_path, native_path, path_relative_to, paths_equal};
pub use remote::{
    AuthChallenge, AuthProtocol, RemoteAction, RemoteAuth, RemoteControl, RemoteError,
    RemoteErrorKind, RemoteOutcome, RemotePhase, RemoteProgress, RemoteRequest, remote_protocol,
};
pub use repository_setup::{RepositorySetupKind, RepositorySetupOutcome, RepositorySetupRequest};
pub use source::{SourceFile, TextEncoding};
pub use stash::{StashAction, StashEntry, StashError, StashErrorKind, StashOutcome, StashRequest};
pub use system_git::SystemGit;
pub use tree::{FileTreeEntry, FileTreeKind, RepositoryTree, SubmoduleInfo, TreeWarning};
pub use worktrees::{
    WorktreeAction, WorktreeError, WorktreeErrorKind, WorktreeOutcome, WorktreeRequest,
};
pub use write_ops::{WriteAction, WriteError, WriteErrorKind, WriteOutcome, WriteRequest};

/// A consistent data shape for the repository overview.
///
/// Reads are not transactional, so a concurrent external Git operation can
/// change the repository between fields being read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositorySnapshot {
    /// Absolute root of the working tree.
    pub root: PathBuf,
    /// Last path component of the working tree root.
    pub name: String,
    /// Current branch, detached commit, or branch awaiting its first commit.
    pub head: Head,
    /// Staged, unstaged, and untracked changes from porcelain status.
    pub changes: Vec<FileChange>,
    /// Up to 20 recent ancestry commits: HEAD first, then newest available parents.
    pub recent_commits: Vec<CommitSummary>,
}

/// The three valid working-tree HEAD states.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Head {
    /// An existing local branch name.
    Branch(String),
    /// Full commit ID when HEAD is detached.
    Detached(String),
    /// Branch name before its first commit.
    Unborn(String),
}

/// One entry in Git's porcelain v1 status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileChange {
    /// Repository-relative path; for renames this is the destination path.
    pub path: String,
    /// Index status, such as `A`, `M`, `R`, `?`, or a space.
    pub index_status: char,
    /// Working-tree status, using the same porcelain status codes.
    pub worktree_status: char,
}

impl FileChange {
    /// All seven unmerged porcelain pairs, including add/add and delete/delete.
    pub fn is_conflicted(&self) -> bool {
        matches!(
            (self.index_status, self.worktree_status),
            ('D', 'D')
                | ('A', 'U')
                | ('U', 'D')
                | ('U', 'A')
                | ('D', 'U')
                | ('A', 'A')
                | ('U', 'U')
        )
    }
}

/// The small portion of commit metadata needed by the overview.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitSummary {
    /// Full commit ID.
    pub id: String,
    /// First line of the commit message.
    pub subject: String,
    /// Author's display name.
    pub author: String,
}

/// Repository access boundary; it can be replaced by a test or another backend.
pub trait GitBackend: Send + Sync {
    fn setup_repository(
        &self,
        _request: &RepositorySetupRequest,
        _control: &RemoteControl,
    ) -> Result<RepositorySetupOutcome, RemoteError> {
        Err(RemoteError::new(
            RemoteErrorKind::Unsupported,
            "此 Git 后端不支持克隆或新建仓库。",
        ))
    }
    /// Read a bounded, frozen graph across multiple local/remote branch tips.
    fn graph_history(
        &self,
        _root: &Path,
        _request: &GraphHistoryRequest,
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<GraphHistoryPage, HistoryToolsError> {
        Err(HistoryToolsError::new(
            HistoryToolsErrorKind::Unsupported,
            "此 Git 后端不支持多分支历史图。",
        ))
    }
    fn search_tree_with_cancel(
        &self,
        path: &Path,
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<RepositoryTree, GitError> {
        self.tree(path)
    }
    fn conflict_state(&self, _root: &Path) -> Result<ConflictState, ConflictError> {
        Err(ConflictError::new(
            ConflictErrorKind::Unsupported,
            "此 Git 后端不支持冲突处理。",
        ))
    }
    fn conflict_versions(
        &self,
        _root: &Path,
        _path: &str,
    ) -> Result<ConflictVersions, ConflictError> {
        Err(ConflictError::new(
            ConflictErrorKind::Unsupported,
            "此 Git 后端不支持冲突处理。",
        ))
    }
    fn conflict(
        &self,
        _root: &Path,
        _request: &ConflictRequest,
    ) -> Result<ConflictOutcome, ConflictError> {
        Err(ConflictError::new(
            ConflictErrorKind::Unsupported,
            "此 Git 后端不支持冲突处理。",
        ))
    }
    fn history_tools(
        &self,
        _root: &Path,
        _request: &HistoryToolsRequest,
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<HistoryToolsResult, HistoryToolsError> {
        Err(HistoryToolsError::new(
            HistoryToolsErrorKind::Unsupported,
            "此 Git 后端不支持历史分析。",
        ))
    }
    fn stashes(&self, _root: &Path) -> Result<Vec<StashEntry>, StashError> {
        Err(StashError::new(
            StashErrorKind::Unsupported,
            "此 Git 后端不支持储藏。",
        ))
    }

    fn stash(&self, _root: &Path, _request: &StashRequest) -> Result<StashOutcome, StashError> {
        Err(StashError::new(
            StashErrorKind::Unsupported,
            "此 Git 后端不支持储藏。",
        ))
    }
    fn write(&self, _root: &Path, _request: &WriteRequest) -> Result<WriteOutcome, WriteError> {
        Err(WriteError::new(
            WriteErrorKind::Unsupported,
            "此 Git 后端不支持暂存和提交。",
        ))
    }

    fn manage_branch(
        &self,
        _root: &Path,
        _request: &BranchRequest,
    ) -> Result<BranchOutcome, BranchError> {
        Err(BranchError::new(
            BranchErrorKind::Unsupported,
            "此 Git 后端不支持分支管理。",
        ))
    }
    /// Read repository identity, remote addresses and nonsecret authentication defaults.
    fn git_settings(&self, _root: &Path) -> Result<GitSettings, GitError> {
        Err(GitError::Unsupported {
            operation: "Git 配置",
        })
    }

    /// Save settings to this repository's local Git config; secrets are never stored.
    fn save_git_settings(&self, _root: &Path, _settings: &GitSettings) -> Result<(), GitError> {
        Err(GitError::Unsupported {
            operation: "保存 Git 配置",
        })
    }

    /// Manage a registered linked worktree without invoking an external Git executable.
    fn worktree(
        &self,
        _root: &Path,
        _request: &WorktreeRequest,
    ) -> Result<WorktreeOutcome, WorktreeError> {
        Err(WorktreeError::new(
            WorktreeErrorKind::Unsupported,
            "此 Git 后端不支持工作树管理。",
        ))
    }
    /// Switch the primary working tree to a local or remote-tracking branch.
    fn checkout(
        &self,
        _root: &Path,
        _request: &CheckoutRequest,
    ) -> Result<CheckoutOutcome, CheckoutError> {
        Err(CheckoutError::new(
            CheckoutErrorKind::InvalidRequest,
            "此 Git 后端不支持分支切换。",
        ))
    }

    /// Execute a native remote operation with cooperative cancellation/progress.
    fn remote(
        &self,
        _root: &Path,
        _request: &RemoteRequest,
        _control: &RemoteControl,
    ) -> Result<RemoteOutcome, RemoteError> {
        Err(RemoteError::new(
            RemoteErrorKind::Unsupported,
            "此 Git 后端不支持远端操作。",
        ))
    }

    /// Inspect a working tree from its root or any directory inside it.
    fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError>;

    /// Read identity and HEAD before status and commit history are loaded.
    /// `changes` and `recent_commits` are pending (empty), not clean/absent.
    /// Native implementations must not scan the index, history, or child worktrees.
    fn inspect_metadata(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
        let mut snapshot = self.inspect(path)?;
        snapshot.changes.clear();
        snapshot.recent_commits.clear();
        Ok(snapshot)
    }

    /// Resolve the owning working-tree root without reading status or history.
    /// Backends that support only overview reads retain compatible behavior.
    fn repository_root(&self, path: &Path) -> Result<PathBuf, GitError> {
        self.inspect(path).map(|snapshot| snapshot.root)
    }

    /// Look up one registered submodule without enumerating the parent's files.
    /// The native backend reads registration and gitlinks, never child status.
    fn registered_submodule(
        &self,
        parent: &Path,
        relative: &str,
    ) -> Result<Option<SubmoduleInfo>, GitError> {
        self.tree(parent).map(|tree| {
            tree.submodules
                .into_iter()
                .find(|module| module.path == relative)
        })
    }

    /// Read local/remote branches and registered worktrees.
    fn details(&self, _path: &Path) -> Result<RepositoryDetails, GitError> {
        Err(GitError::Unsupported {
            operation: "repository details",
        })
    }

    /// Compare HEAD to the index when staged, or the index to working files otherwise.
    /// `file` must be a repository-relative path using forward slashes.
    fn diff(&self, _path: &Path, _file: &str, _staged: bool) -> Result<FileDiff, GitError> {
        Err(GitError::Unsupported {
            operation: "file diff",
        })
    }

    /// Read tracked, unignored working files and submodules as a flat tree.
    fn tree(&self, _path: &Path) -> Result<RepositoryTree, GitError> {
        Err(GitError::Unsupported {
            operation: "repository file tree",
        })
    }

    /// Build a tree using status already read by this refresh's overview.
    fn tree_with_changes(
        &self,
        path: &Path,
        _changes: &[FileChange],
    ) -> Result<RepositoryTree, GitError> {
        self.tree(path)
    }

    /// Read complete text independently of diff. None selects working files.
    fn source(
        &self,
        _path: &Path,
        _file: &str,
        _revision: Option<&str>,
    ) -> Result<SourceFile, GitError> {
        Err(GitError::Unsupported {
            operation: "source file",
        })
    }

    /// Page through commits reachable from HEAD; page size must be 1..=200.
    fn history(&self, _path: &Path, _skip: usize, _limit: usize) -> Result<HistoryPage, GitError> {
        Err(GitError::Unsupported {
            operation: "commit history",
        })
    }

    /// Traverse from an immutable full commit ID so concurrent HEAD changes cannot shift pages.
    fn history_at(
        &self,
        _path: &Path,
        _anchor: &str,
        _skip: usize,
        _limit: usize,
    ) -> Result<HistoryPage, GitError> {
        Err(GitError::Unsupported {
            operation: "anchored commit history",
        })
    }

    /// Read commit metadata and changes relative to its first parent.
    fn commit(&self, _path: &Path, _id: &str) -> Result<CommitDetails, GitError> {
        Err(GitError::Unsupported {
            operation: "commit details",
        })
    }

    /// Compare a committed file to the first parent, or the empty tree at the root.
    fn commit_diff(&self, _path: &Path, _id: &str, _file: &str) -> Result<FileDiff, GitError> {
        Err(GitError::Unsupported {
            operation: "commit diff",
        })
    }
}

/// A read-only backend that executes the installed `git` command.
#[derive(Clone, Copy, Debug, Default)]
pub struct GitCli;

/// A failure to launch Git, locate a repository, or decode its output.
#[derive(Debug)]
pub enum GitError {
    /// An operation's argument is outside its documented range.
    InvalidRequest {
        operation: &'static str,
        reason: &'static str,
    },
    /// A single line exceeds the editor's safe layout limit.
    LineTooLong { path: String, limit_chars: usize },
    /// Too many lines to safely parse and render in an editor or diff.
    LineCountLimit { path: String, limit_lines: usize },
    /// A BOM-declared encoding contains invalid code units.
    InvalidEncoding {
        path: String,
        encoding: &'static str,
    },
    /// A native libgit2 operation failed.
    Native {
        /// Operation being attempted.
        operation: &'static str,
        /// Original libgit2 diagnostic.
        source: git2::Error,
    },
    /// The backend or selected file type does not support this operation.
    Unsupported { operation: &'static str },
    /// A diff path is not an ordinary file within the selected working tree.
    InvalidPath { path: String, reason: &'static str },
    /// Binary or non-UTF-8 content cannot be rendered by the text diff viewer.
    BinaryFile { path: String },
    /// A text preview exceeds the viewer's memory limit.
    FileTooLarge { path: String, limit_bytes: usize },
    /// Git is missing, could not be started, or its output could not be read.
    Io {
        /// Operation being attempted.
        operation: &'static str,
        /// Original operating-system error.
        source: io::Error,
    },
    /// The selected location is not a working-tree repository.
    NotRepository {
        /// Original selected path.
        path: PathBuf,
    },
    /// Git started successfully but rejected the command.
    CommandFailed {
        /// Operation being attempted.
        operation: &'static str,
        /// Process exit code, absent if terminated by a signal.
        exit_code: Option<i32>,
        /// Git's diagnostic text.
        stderr: String,
    },
    /// Git returned a successful response with an unexpected format.
    InvalidOutput {
        /// Operation whose response was invalid.
        operation: &'static str,
        /// Explanation of the malformed response.
        details: &'static str,
    },
}

impl fmt::Display for GitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LineCountLimit { path, limit_lines } => write!(
                formatter,
                "{path} 超过预览行数上限（{limit_lines} 行），请使用外部工具查看。"
            ),
            Self::InvalidRequest { operation, reason } => {
                write!(formatter, "{operation}: {reason}")
            }
            Self::LineTooLong { path, limit_chars } => write!(
                formatter,
                "{path} has a line exceeding the editor limit of {limit_chars} characters"
            ),
            Self::InvalidEncoding { path, encoding } => {
                write!(formatter, "{path} contains invalid {encoding} text")
            }
            Self::Native { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::Unsupported { operation } => write!(formatter, "{operation} is not supported"),
            Self::InvalidPath { path, reason } => {
                write!(formatter, "Cannot preview {path}: {reason}")
            }
            Self::BinaryFile { path } => write!(
                formatter,
                "{path} is binary or uses an unsupported text encoding"
            ),
            Self::FileTooLarge { path, limit_bytes } => write!(
                formatter,
                "{path} exceeds the text preview limit of {limit_bytes} bytes"
            ),
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::NotRepository { path } => {
                write!(
                    formatter,
                    "{} is not a Git working tree",
                    display_path(path)
                )
            }
            Self::CommandFailed {
                operation,
                exit_code,
                stderr,
            } => write!(
                formatter,
                "{operation} failed (exit {}): {}",
                exit_code.map_or_else(|| "unknown".to_owned(), |code| code.to_string()),
                stderr.trim()
            ),
            Self::InvalidOutput { operation, details } => {
                write!(formatter, "{operation}: invalid Git output ({details})")
            }
        }
    }
}

impl Error for GitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Native { source, .. } => Some(source),
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl GitBackend for GitCli {
    fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
        let root = repository_root(path)?;
        let name = root.file_name().map_or_else(
            || display_path(&root),
            |name| name.to_string_lossy().into_owned(),
        );

        let branch_output = run_git(
            &root,
            "read current branch",
            &["symbolic-ref", "--quiet", "HEAD"],
        )?;
        let branch = if branch_output.status.success() {
            let reference = output_line(&branch_output.stdout, "read current branch")?;
            Some(
                reference
                    .strip_prefix("refs/heads/")
                    .unwrap_or(&reference)
                    .to_owned(),
            )
        } else if branch_output.status.code() == Some(1) {
            None
        } else {
            return Err(command_error("read current branch", branch_output));
        };

        let commit_output = run_git(
            &root,
            "read HEAD commit",
            &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        )?;
        let (head, has_commits) = if commit_output.status.success() {
            let id = output_line(&commit_output.stdout, "read HEAD commit")?;
            (branch.map_or(Head::Detached(id), Head::Branch), true)
        } else if commit_output.status.code() == Some(1) {
            match branch {
                Some(branch) => {
                    // A missing branch ref is the normal first-commit state.
                    // An existing ref that cannot resolve is repository damage.
                    let branch_ref = format!("refs/heads/{branch}");
                    let ref_output = run_git(
                        &root,
                        "check unborn branch",
                        &["show-ref", "--verify", "--quiet", &branch_ref],
                    )?;
                    if ref_output.status.success() {
                        return Err(command_error("read HEAD commit", commit_output));
                    }
                    if ref_output.status.code() != Some(1) {
                        return Err(command_error("check unborn branch", ref_output));
                    }
                    (Head::Unborn(branch), false)
                }
                None => return Err(command_error("read HEAD commit", commit_output)),
            }
        } else {
            return Err(command_error("read HEAD commit", commit_output));
        };

        let status = checked_git(
            &root,
            "read working tree status",
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )?;
        let changes = parse_status(&status.stdout)?;
        let recent_commits = if has_commits {
            let log = checked_git(
                &root,
                "read recent commits",
                &[
                    "log",
                    "--no-show-signature",
                    "--max-count=20",
                    "--encoding=UTF-8",
                    "--format=%H%x00%s%x00%an",
                    "-z",
                    "HEAD",
                    "--",
                ],
            )?;
            parse_commits(&log.stdout)?
        } else {
            Vec::new()
        };

        Ok(RepositorySnapshot {
            root,
            name,
            head,
            changes,
            recent_commits,
        })
    }

    fn details(&self, path: &Path) -> Result<RepositoryDetails, GitError> {
        details::read(path)
    }

    fn diff(&self, path: &Path, file: &str, staged: bool) -> Result<FileDiff, GitError> {
        diff::read(path, file, staged)
    }
}

fn repository_root(path: &Path) -> Result<PathBuf, GitError> {
    let output = run_git(path, "locate repository", &["rev-parse", "--show-toplevel"])?;
    if output.status.success() {
        return Ok(PathBuf::from(output_line(
            &output.stdout,
            "locate repository",
        )?));
    }
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    if diagnostic.contains("not a git repository")
        || diagnostic.contains("must be run in a work tree")
    {
        return Err(GitError::NotRepository {
            path: path.to_owned(),
        });
    }
    Err(command_error("locate repository", output))
}

fn run_git(path: &Path, operation: &'static str, args: &[&str]) -> Result<Output, GitError> {
    let mut command = Command::new("git");
    command
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW prevents a console flashing in the desktop app.
        command.creation_flags(0x0800_0000);
    }
    command
        .output()
        .map_err(|source| GitError::Io { operation, source })
}

fn checked_git(path: &Path, operation: &'static str, args: &[&str]) -> Result<Output, GitError> {
    let output = run_git(path, operation, args)?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(command_error(operation, output))
    }
}

fn command_error(operation: &'static str, output: Output) -> GitError {
    GitError::CommandFailed {
        operation,
        exit_code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn output_line(bytes: &[u8], operation: &'static str) -> Result<String, GitError> {
    // Strip exactly Git's line ending, preserving whitespace in directory names.
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    if bytes.is_empty() {
        return Err(GitError::InvalidOutput {
            operation,
            details: "expected a nonempty line",
        });
    }
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn parse_status(bytes: &[u8]) -> Result<Vec<FileChange>, GitError> {
    let mut records = bytes.split(|byte| *byte == 0).peekable();
    let mut changes = Vec::new();
    while let Some(record) = records.next() {
        if record.is_empty() && records.peek().is_none() {
            break;
        }
        if record.len() < 4 || record[2] != b' ' {
            return Err(GitError::InvalidOutput {
                operation: "read working tree status",
                details: "malformed porcelain status record",
            });
        }
        changes.push(FileChange {
            path: String::from_utf8_lossy(&record[3..]).into_owned(),
            index_status: char::from(record[0]),
            worktree_status: char::from(record[1]),
        });
        if matches!(record[0], b'R' | b'C') || matches!(record[1], b'R' | b'C') {
            // With -z Git emits destination first, then a separate original path.
            if records.next().is_none_or(|original| original.is_empty()) {
                return Err(GitError::InvalidOutput {
                    operation: "read working tree status",
                    details: "rename or copy is missing its original path",
                });
            }
        }
    }
    Ok(changes)
}

fn parse_commits(bytes: &[u8]) -> Result<Vec<CommitSummary>, GitError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let fields: Vec<_> = bytes
        .strip_suffix(&[0])
        .unwrap_or(bytes)
        .split(|byte| *byte == 0)
        .collect();
    let (commits, remainder) = fields.as_chunks::<3>();
    if !remainder.is_empty() {
        return Err(GitError::InvalidOutput {
            operation: "read recent commits",
            details: "incomplete commit metadata",
        });
    }
    Ok(commits
        .iter()
        .map(|fields| CommitSummary {
            id: String::from_utf8_lossy(fields[0]).into_owned(),
            subject: String::from_utf8_lossy(fields[1]).into_owned(),
            author: String::from_utf8_lossy(fields[2]).into_owned(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_truncated_rename_records() {
        let error = parse_status(b"R  destination\0").unwrap_err();
        assert!(matches!(error, GitError::InvalidOutput { .. }));
    }

    #[test]
    fn rejects_incomplete_commit_metadata() {
        let error = parse_commits(b"hash\0subject\0").unwrap_err();
        assert!(matches!(error, GitError::InvalidOutput { .. }));
    }
}
