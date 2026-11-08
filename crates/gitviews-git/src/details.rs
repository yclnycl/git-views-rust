use std::path::{Path, PathBuf};

use crate::{GitError, checked_git, repository_root};

/// Branches and worktrees used by the repository navigation panels.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepositoryDetails {
    /// Registered worktrees, with the main worktree first.
    pub worktrees: Vec<WorktreeInfo>,
    /// Local and remote-tracking branches; symbolic remote HEAD entries are omitted.
    pub branches: Vec<BranchInfo>,
}

/// A registered worktree. Missing/prunable worktrees remain visible.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorktreeInfo {
    /// Absolute registered working-tree path.
    pub path: PathBuf,
    /// Local branch name, absent when detached or bare.
    pub branch: Option<String>,
    /// Full HEAD commit ID, absent for an unborn or bare worktree.
    pub head: Option<String>,
    /// Whether this is the main worktree rather than a linked worktree.
    pub is_main: bool,
    /// Whether this registration refers to a bare repository.
    pub is_bare: bool,
    /// Whether HEAD is detached.
    pub is_detached: bool,
    /// Whether Git has locked this worktree registration.
    pub locked: bool,
    /// Whether Git considers this registration eligible for pruning.
    pub prunable: bool,
}

/// Display metadata for a local or remote-tracking branch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchInfo {
    /// Local branch name or remote/name, without the refs prefix.
    pub name: String,
    /// Full tip commit ID; empty for a branch awaiting its first commit.
    pub commit_id: String,
    /// Tip commit subject; empty for an unborn branch.
    pub subject: String,
    /// Whether HEAD in the selected worktree points at this branch.
    pub is_current: bool,
    /// Configured tracking branch when present.
    pub upstream: Option<String>,
    /// Whether this entry is a remote-tracking branch.
    pub is_remote: bool,
}

pub(super) fn read(path: &Path) -> Result<RepositoryDetails, GitError> {
    let root = repository_root(path)?;
    let output = checked_git(
        &root,
        "read worktrees",
        &["worktree", "list", "--porcelain", "-z"],
    )?;
    let mut worktrees = Vec::new();
    let mut current: Option<WorktreeInfo> = None;
    for field in output.stdout.split(|byte| *byte == 0) {
        let field = String::from_utf8_lossy(field);
        if let Some(path) = field.strip_prefix("worktree ") {
            if let Some(worktree) = current.take() {
                worktrees.push(worktree);
            }
            current = Some(WorktreeInfo {
                path: PathBuf::from(path),
                is_main: worktrees.is_empty(),
                ..WorktreeInfo::default()
            });
        } else if field.is_empty() {
            if let Some(worktree) = current.take() {
                worktrees.push(worktree);
            }
        } else if let Some(worktree) = current.as_mut() {
            if let Some(head) = field.strip_prefix("HEAD ") {
                worktree.head = (!head.bytes().all(|byte| byte == b'0')).then(|| head.to_owned());
            } else if let Some(branch) = field.strip_prefix("branch ") {
                worktree.branch = Some(
                    branch
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch)
                        .to_owned(),
                );
            } else if field == "bare" {
                worktree.is_bare = true;
            } else if field == "detached" {
                worktree.is_detached = true;
            } else if field == "locked" || field.starts_with("locked ") {
                worktree.locked = true;
            } else if field == "prunable" || field.starts_with("prunable ") {
                worktree.prunable = true;
            }
        }
    }
    if let Some(worktree) = current {
        worktrees.push(worktree);
    }

    let output = checked_git(
        &root,
        "read branches",
        &[
            "for-each-ref",
            "--sort=refname",
            "--format=%(refname)%00%(objectname)%00%(HEAD)%00%(upstream:short)%00%(subject)%00%(symref)%00",
            "refs/heads/",
            "refs/remotes/",
        ],
    )?;
    let mut fields = output.stdout.split(|byte| *byte == 0);
    let mut branches = Vec::new();
    while let Some(reference) = fields.next() {
        let reference = String::from_utf8_lossy(reference);
        let reference = reference.trim_start_matches('\n');
        if reference.is_empty() {
            break;
        }
        let mut next = || {
            fields.next().ok_or(GitError::InvalidOutput {
                operation: "read branches",
                details: "incomplete branch metadata",
            })
        };
        let commit_id = String::from_utf8_lossy(next()?).into_owned();
        let is_current = next()? == b"*";
        let upstream = String::from_utf8_lossy(next()?).into_owned();
        let subject = String::from_utf8_lossy(next()?).into_owned();
        let symbolic = next()?;
        if !symbolic.is_empty() {
            continue;
        }
        let is_remote = reference.starts_with("refs/remotes/");
        let name = reference
            .strip_prefix("refs/heads/")
            .or_else(|| reference.strip_prefix("refs/remotes/"))
            .unwrap_or(reference)
            .to_owned();
        branches.push(BranchInfo {
            name,
            commit_id,
            subject,
            is_current,
            upstream: (!upstream.is_empty()).then_some(upstream),
            is_remote,
        });
    }
    // An unborn current branch does not yet have a ref for for-each-ref to list.
    if let Some(worktree) = worktrees.iter().find(|worktree| worktree.path == root)
        && worktree.head.is_none()
        && let Some(name) = &worktree.branch
        && !branches
            .iter()
            .any(|branch| &branch.name == name && !branch.is_remote)
    {
        branches.insert(
            0,
            BranchInfo {
                name: name.clone(),
                commit_id: String::new(),
                subject: String::new(),
                is_current: true,
                upstream: None,
                is_remote: false,
            },
        );
    }
    Ok(RepositoryDetails {
        worktrees,
        branches,
    })
}
