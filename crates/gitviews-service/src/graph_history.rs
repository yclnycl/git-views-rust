use crate::{GraphHistoryPage, GraphHistoryRequest, HistoryToolsError, RepositoryService};
use gitviews_git::GitBackend;
use std::{path::Path, sync::atomic::AtomicBool};

impl<B: GitBackend> RepositoryService<B> {
    /// Native read only. The snapshot pins all selected tips and labels across
    /// pages; this does not enter the write gate or invalidate preview caches.
    pub fn graph_history(
        &self,
        root: &Path,
        request: &GraphHistoryRequest,
        cancel: &AtomicBool,
    ) -> Result<GraphHistoryPage, HistoryToolsError> {
        self.backend.graph_history(root, request, cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GitError, HistoryToolsErrorKind, RepositorySnapshot};
    use std::sync::atomic::Ordering;

    struct Stub;
    impl GitBackend for Stub {
        fn inspect(&self, _path: &Path) -> Result<RepositorySnapshot, GitError> {
            unreachable!("graph reads must not inspect HEAD/status")
        }
        fn graph_history(
            &self,
            root: &Path,
            request: &GraphHistoryRequest,
            cancel: &AtomicBool,
        ) -> Result<GraphHistoryPage, HistoryToolsError> {
            assert_eq!(root, Path::new("exact child root"));
            assert_eq!(request.references, ["refs/heads/feature"]);
            assert_eq!(request.offset, 15);
            assert_eq!(request.filter.text, "needle");
            assert!(cancel.load(Ordering::Relaxed));
            Err(HistoryToolsError::new(
                HistoryToolsErrorKind::Cancelled,
                "cancel token forwarded",
            ))
        }
    }

    #[test]
    fn graph_history_forwards_exact_root_filters_and_cancellation_without_inspection_or_write_gate()
    {
        let service = RepositoryService::new(Stub);
        // A read-only graph remains available while another mutation holds the gate.
        let _guard = service.remote_gate.lock().unwrap();
        let error = service
            .graph_history(
                Path::new("exact child root"),
                &GraphHistoryRequest {
                    references: vec!["refs/heads/feature".into()],
                    offset: 15,
                    filter: crate::GraphHistoryFilter {
                        text: "needle".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                &AtomicBool::new(true),
            )
            .unwrap_err();
        assert_eq!(error.kind, HistoryToolsErrorKind::Cancelled);
        assert_eq!(error.message, "cancel token forwarded");
    }
}
