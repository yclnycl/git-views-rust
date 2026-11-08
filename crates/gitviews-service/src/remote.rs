use std::{path::Path, sync::atomic::Ordering};

use gitviews_git::GitBackend;

use crate::{
    RemoteControl, RemoteError, RemoteErrorKind, RemoteOutcome, RemoteRequest, RepositoryService,
};

impl<B: GitBackend> RepositoryService<B> {
    /// All clones share the mutation gate. A second operation is rejected rather
    /// than queued with branch identity or credentials that may become stale.
    pub fn remote(
        &self,
        root: &Path,
        request: &RemoteRequest,
        control: &RemoteControl,
    ) -> Result<RemoteOutcome, RemoteError> {
        let _guard = self.remote_gate.try_lock().map_err(|_| {
            RemoteError::new(
                RemoteErrorKind::InProgress,
                "已有远端操作正在执行，请等待完成或取消。",
            )
        })?;
        if control.cancel.load(Ordering::Acquire) {
            return Err(RemoteError::new(RemoteErrorKind::Cancelled, "操作已取消。"));
        }
        let result = if let Some(system) = self.configured_system_git() {
            system.remote(root, request, control)
        } else {
            self.backend.remote(root, request, control)
        };
        // Fetch can update some remote refs even if a later stage fails.
        self.invalidate(root);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GitError, RemoteAction, RepositorySnapshot};
    use std::sync::{Arc, Barrier};

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

        fn remote(
            &self,
            _: &Path,
            request: &RemoteRequest,
            _: &RemoteControl,
        ) -> Result<RemoteOutcome, RemoteError> {
            self.entered.wait();
            self.released.wait();
            Ok(RemoteOutcome {
                action: request.action,
                remote: "fixture".into(),
                branch: None,
                head_before: None,
                head_after: None,
                summary: "done".into(),
            })
        }
    }

    #[test]
    fn concurrent_remote_operations_share_one_gate_across_service_handles() {
        let entered = Arc::new(Barrier::new(2));
        let released = Arc::new(Barrier::new(2));
        let service = Arc::new(RepositoryService::new(BlockingBackend {
            entered: entered.clone(),
            released: released.clone(),
        }));
        let request = RemoteRequest {
            action: RemoteAction::Fetch,
            expected_branch: None,
            expected_head: None,
        };
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                service.remote(Path::new("fixture"), &request, &RemoteControl::default())
            });
            entered.wait();
            let rejected =
                service.remote(Path::new("fixture"), &request, &RemoteControl::default());
            let checkout = service.checkout(
                Path::new("fixture"),
                &crate::CheckoutRequest {
                    branch: "main".into(),
                    remote: false,
                    expected_branch: None,
                    expected_head: None,
                },
            );
            released.wait();
            assert_eq!(rejected.unwrap_err().kind, RemoteErrorKind::InProgress);
            assert_eq!(
                checkout.unwrap_err().kind,
                crate::CheckoutErrorKind::InProgress
            );
            assert!(handle.join().unwrap().is_ok());
        });
    }
}
