use crate::{
    HistoryToolsError, HistoryToolsErrorKind, HistoryToolsRequest, HistoryToolsResult,
    RepositoryService,
};
use gitviews_git::GitBackend;
use std::{path::Path, sync::atomic::AtomicBool};

impl<B: GitBackend> RepositoryService<B> {
    pub fn history_tools(
        &self,
        root: &Path,
        request: &HistoryToolsRequest,
        cancel: &AtomicBool,
    ) -> Result<HistoryToolsResult, HistoryToolsError> {
        if request.is_mutation() {
            let _guard = self.remote_gate.try_lock().map_err(|_| {
                HistoryToolsError::new(
                    HistoryToolsErrorKind::InProgress,
                    "已有 Git 操作正在执行，请等待完成。",
                )
            })?;
            let result = if let Some(system) = self.configured_system_git() {
                system.history_tools(root, request, cancel)
            } else {
                self.backend.history_tools(root, request, cancel)
            };
            self.invalidate(root);
            result
        } else {
            self.backend.history_tools(root, request, cancel)
        }
    }
}
