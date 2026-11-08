//! Search metadata avoids branch/history and recursive submodule status work.
use crate::{FileChange, GitError, RepositoryTree, native};
use git2::{Status, StatusOptions};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

pub(super) fn read(path: &Path, cancel: &AtomicBool) -> Result<RepositoryTree, GitError> {
    let check = || {
        if cancel.load(Ordering::Relaxed) {
            Err(GitError::InvalidRequest {
                operation: "search file tree",
                reason: "cancelled",
            })
        } else {
            Ok(())
        }
    };
    check()?;
    let repository = native::open(path)?;
    let root = native::root(&repository)?;
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .exclude_submodules(true)
        .renames_head_to_index(false)
        .renames_index_to_workdir(false)
        .update_index(false)
        .no_refresh(true);
    let statuses = repository
        .statuses(Some(&mut options))
        .map_err(native::native("read search candidates"))?;
    check()?;
    let mut changes = Vec::new();
    for entry in statuses.iter() {
        check()?;
        let path = entry
            .path()
            .map_err(native::native("read search candidate path"))?;
        let status = entry.status();
        changes.push(FileChange {
            path: path.to_owned(),
            index_status: if status.contains(Status::INDEX_DELETED) {
                'D'
            } else {
                ' '
            },
            worktree_status: if status.contains(Status::WT_DELETED) {
                'D'
            } else if status.contains(Status::WT_NEW) {
                '?'
            } else {
                ' '
            },
        });
        if changes.len() > 100_000 {
            return Err(GitError::Unsupported {
                operation: "searching more than 100000 changed files",
            });
        }
    }
    check()?;
    let tree = native::read_tree(&repository, &root, Some(&changes))?;
    check()?;
    Ok(tree)
}
