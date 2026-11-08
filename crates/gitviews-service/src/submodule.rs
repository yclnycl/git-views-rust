//! Exact-root checks keep a missing child repository from discovering its parent.

use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

use gitviews_git::GitBackend;

use crate::{RepositoryService, RepositorySnapshot, RepositoryTree, ServiceError};

/// File-change data for one initialized child, independent of the parent's UI state.
#[derive(Clone, Debug)]
pub struct SubmoduleSnapshot {
    /// The child's overview; its root is the canonical child working directory.
    pub repository: RepositorySnapshot,
    /// Files relative to the child root, never to the superproject.
    pub tree: RepositoryTree,
}

impl<B: GitBackend> RepositoryService<B> {
    /// Verify that `root` still opens that exact working tree.
    ///
    /// Repository discovery intentionally accepts subdirectories for ordinary
    /// opening. A child scope must instead reject discovery of an ancestor after
    /// its `.git` file disappears. Call this before and after a child preview read
    /// and discard the preview if either check fails. This performs no tree scan.
    #[tracing::instrument(level = "debug", skip(self))]
    pub fn validate_root(&self, root: &Path) -> Result<(), ServiceError> {
        self.exact_root(root).map(|_| ())
    }

    /// Read an initialized submodule registered directly in the parent's current tree.
    ///
    /// Both paths must identify working-tree roots. `candidate` must be absolute,
    /// stay below the parent, and name a registered module rather than an ordinary
    /// nested directory or repository. Registration is read again on every call;
    /// an earlier UI `initialized` flag is insufficient after external deinitialization.
    /// Invoke this synchronous operation on a background worker.
    #[tracing::instrument(level = "info", skip(self))]
    pub fn submodule(
        &self,
        parent: &Path,
        candidate: &Path,
    ) -> Result<SubmoduleSnapshot, ServiceError> {
        self.submodule_with_cancel(parent, candidate, &AtomicBool::new(false))
    }

    /// Cancel a superseded read between registration, overview, and tree work.
    /// A synchronous libgit2 call already in progress runs to completion before
    /// the next check; cancellation does not preempt native I/O.
    #[tracing::instrument(level = "info", skip(self, cancel))]
    pub fn submodule_with_cancel(
        &self,
        parent: &Path,
        candidate: &Path,
        cancel: &AtomicBool,
    ) -> Result<SubmoduleSnapshot, ServiceError> {
        let started = Instant::now();
        let candidate_root = self.registered_child_root(parent, candidate, cancel)?;
        self.validate_root(&candidate_root)?;
        check_cancel(cancel, "before_overview")?;
        let inspect_started = Instant::now();
        let repository = self.exact_repository(&candidate_root)?;
        check_cancel(cancel, "after_overview")?;
        tracing::debug!(
            phase = "overview",
            elapsed_ms = inspect_started.elapsed().as_millis() as u64,
            changes = repository.changes.len(),
            "Submodule overview ready"
        );
        let tree_started = Instant::now();
        check_cancel(cancel, "before_file_tree")?;
        let tree = self
            .backend
            .tree_with_changes(&candidate_root, &repository.changes)
            .map_err(ServiceError::Git)?;
        check_cancel(cancel, "after_file_tree")?;
        tracing::debug!(
            phase = "file_tree",
            elapsed_ms = tree_started.elapsed().as_millis() as u64,
            entries = tree.entries.len(),
            "Submodule file tree ready"
        );
        // Every backend read opens independently. Recheck after the tree read so
        // a deinitialization during it cannot expose the ancestor's file tree.
        self.validate_root(&candidate_root)?;
        check_cancel(cancel, "before_publish")?;
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            changes = repository.changes.len(),
            entries = tree.entries.len(),
            "Submodule snapshot ready"
        );
        Ok(SubmoduleSnapshot { repository, tree })
    }

    /// Validate the selected child and show its identity before status/history/tree I/O.
    /// Empty changes/history are pending; callers request the full overview next.
    #[tracing::instrument(level = "info", skip(self, cancel))]
    pub fn submodule_metadata_with_cancel(
        &self,
        parent: &Path,
        candidate: &Path,
        cancel: &AtomicBool,
    ) -> Result<RepositorySnapshot, ServiceError> {
        let candidate_root = self.registered_child_root(parent, candidate, cancel)?;
        self.validate_root(&candidate_root)?;
        check_cancel(cancel, "before_metadata")?;
        let mut repository = self.open_metadata(&candidate_root)?;
        check_cancel(cancel, "after_metadata")?;
        if canonical(&repository.root)? != candidate_root {
            return Err(refused(
                candidate,
                "the path no longer identifies this repository root",
            ));
        }
        self.validate_root(&candidate_root)?;
        check_cancel(cancel, "before_publish")?;
        repository.root = candidate_root;
        Ok(repository)
    }

    fn registered_child_root(
        &self,
        parent: &Path,
        candidate: &Path,
        cancel: &AtomicBool,
    ) -> Result<PathBuf, ServiceError> {
        check_cancel(cancel, "before_root_validation")?;
        if !candidate.is_absolute()
            || candidate
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(refused(
                candidate,
                "use an absolute child root without parent traversal",
            ));
        }

        let parent_root = self.exact_root(parent)?;
        check_cancel(cancel, "after_parent_root_validation")?;
        let candidate_root = canonical(candidate)?;
        if candidate_root == parent_root || !candidate_root.starts_with(&parent_root) {
            return Err(refused(
                candidate,
                "the child root must remain inside the parent working tree",
            ));
        }

        let relative = candidate_root
            .strip_prefix(&parent_root)
            .ok()
            .and_then(Path::to_str)
            .map(|path| path.replace('\\', "/"))
            .filter(|path| safe_module_path(path))
            .ok_or_else(|| {
                refused(
                    candidate,
                    "the child registration must be a safe UTF-8 path",
                )
            })?;
        let registration_started = Instant::now();
        check_cancel(cancel, "before_registration")?;
        let module = self
            .backend
            .registered_submodule(&parent_root, &relative)
            .map_err(ServiceError::Git)?;
        check_cancel(cancel, "after_registration")?;
        let Some(module) = module else {
            return Err(refused(candidate, "the path is not a registered submodule"));
        };
        if module.path != relative || !safe_module_path(&module.path) {
            return Err(refused(
                candidate,
                "the returned submodule registration does not match",
            ));
        }
        if !module.initialized {
            return Err(refused(candidate, "the submodule is not initialized"));
        }
        tracing::debug!(
            phase = "registration",
            elapsed_ms = registration_started.elapsed().as_millis() as u64,
            "Submodule registration verified"
        );
        Ok(candidate_root)
    }

    fn exact_root(&self, root: &Path) -> Result<PathBuf, ServiceError> {
        let expected = canonical(root)?;
        let actual = self
            .backend
            .repository_root(&expected)
            .map_err(ServiceError::Git)?;
        if canonical(&actual)? != expected {
            return Err(refused(
                root,
                "the path no longer identifies this repository root",
            ));
        }
        Ok(expected)
    }

    fn exact_repository(&self, root: &Path) -> Result<RepositorySnapshot, ServiceError> {
        let expected = canonical(root)?;
        let mut repository = self.open(&expected)?;
        if canonical(&repository.root)? != expected {
            return Err(refused(
                root,
                "the path no longer identifies this repository root",
            ));
        }
        repository.root = expected;
        Ok(repository)
    }
}

fn check_cancel(cancel: &AtomicBool, phase: &'static str) -> Result<(), ServiceError> {
    if cancel.load(Ordering::Acquire) {
        tracing::debug!(
            phase,
            "Stopped superseded submodule read between backend calls"
        );
        Err(ServiceError::Cancelled {
            operation: "read submodule",
        })
    } else {
        Ok(())
    }
}

fn canonical(path: &Path) -> Result<PathBuf, ServiceError> {
    fs::canonicalize(path).map_err(|error| {
        refused(
            path,
            format!("cannot resolve the working directory: {error}"),
        )
    })
}

fn refused(path: &Path, reason: impl Into<String>) -> ServiceError {
    ServiceError::Submodule {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

fn safe_module_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\0', '\\', ':'])
        && Path::new(path).components().all(|part| {
            matches!(part, Component::Normal(name) if !name.to_string_lossy().eq_ignore_ascii_case(".git"))
        })
}

#[cfg(test)]
mod tests;
