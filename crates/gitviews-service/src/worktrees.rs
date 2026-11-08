use std::path::Path;

use gitviews_git::GitBackend;

use crate::{
    RepositoryService, WorktreeAction, WorktreeError, WorktreeErrorKind, WorktreeOutcome,
    WorktreeRequest,
};

impl<B: GitBackend> RepositoryService<B> {
    /// Worktree mutations share the remote/checkout write gate across clones.
    pub fn worktree(
        &self,
        root: &Path,
        request: &WorktreeRequest,
    ) -> Result<WorktreeOutcome, WorktreeError> {
        let _guard = self.remote_gate.try_lock().map_err(|_| {
            WorktreeError::new(
                WorktreeErrorKind::Busy,
                "已有 Git 写操作正在执行，请等待完成。",
            )
        })?;
        let result = if let Some(system) = self.configured_system_git() {
            system.worktree(root, request)
        } else {
            self.backend.worktree(root, request)
        };
        self.invalidate(root);
        let path = match &request.action {
            WorktreeAction::Create { path, .. }
            | WorktreeAction::Remove { path, .. }
            | WorktreeAction::Move { path, .. }
            | WorktreeAction::Lock { path, .. }
            | WorktreeAction::Unlock { path }
            | WorktreeAction::Prune { path }
            | WorktreeAction::Repair { path, .. } => path,
        };
        self.invalidate(path);
        if let WorktreeAction::Move { destination, .. }
        | WorktreeAction::Repair { destination, .. } = &request.action
        {
            self.invalidate(destination);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GitError, Head, RepositorySnapshot};
    use std::sync::{Arc, Barrier};

    #[derive(Clone)]
    struct BlockingBackend {
        entered: Arc<Barrier>,
        released: Arc<Barrier>,
    }
    impl GitBackend for BlockingBackend {
        fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
            Err(GitError::NotRepository {
                path: path.to_owned(),
            })
        }
        fn worktree(
            &self,
            _: &Path,
            _: &WorktreeRequest,
        ) -> Result<WorktreeOutcome, WorktreeError> {
            self.entered.wait();
            self.released.wait();
            Ok(WorktreeOutcome {
                message: "done".into(),
                previous_path: None,
                path: None,
            })
        }
    }

    #[test]
    fn worktree_operations_share_the_existing_git_write_gate_across_clones() {
        let entered = Arc::new(Barrier::new(2));
        let released = Arc::new(Barrier::new(2));
        let service = RepositoryService::new(BlockingBackend {
            entered: Arc::clone(&entered),
            released: Arc::clone(&released),
        });
        let concurrent = service.clone();
        let request = WorktreeRequest {
            expected_head: Head::Unborn("main".into()),
            expected_commit: None,
            action: WorktreeAction::Prune {
                path: "missing".into(),
            },
        };
        std::thread::scope(|scope| {
            let pending = scope.spawn(|| service.worktree(Path::new("fixture"), &request));
            entered.wait();
            assert!(concurrent.remote_gate.try_lock().is_err());
            let second = concurrent
                .worktree(Path::new("fixture"), &request)
                .unwrap_err();
            assert_eq!(second.kind, WorktreeErrorKind::Busy);
            released.wait();
            pending.join().unwrap().unwrap();
        });
        assert!(service.remote_gate.try_lock().is_ok());
    }
}
