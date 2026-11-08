use crate::{
    ConflictError, ConflictErrorKind, ConflictOutcome, ConflictRequest, ConflictState,
    ConflictVersions, RepositoryService,
};
use gitviews_git::GitBackend;
use std::path::Path;

impl<B: GitBackend> RepositoryService<B> {
    pub fn conflict_state(&self, root: &Path) -> Result<ConflictState, ConflictError> {
        if let Some(system) = self.configured_system_git() {
            system.conflict_state(root)
        } else {
            self.backend.conflict_state(root)
        }
    }
    pub fn conflict_versions(
        &self,
        root: &Path,
        path: &str,
    ) -> Result<ConflictVersions, ConflictError> {
        if let Some(system) = self.configured_system_git() {
            system.conflict_versions(root, path)
        } else {
            self.backend.conflict_versions(root, path)
        }
    }
    pub fn conflict(
        &self,
        root: &Path,
        request: &ConflictRequest,
    ) -> Result<ConflictOutcome, ConflictError> {
        let _guard = self.remote_gate.try_lock().map_err(|_| {
            ConflictError::new(
                ConflictErrorKind::InProgress,
                "已有 Git 操作正在执行，请等待完成。",
            )
        })?;
        let result = if let Some(system) = self.configured_system_git() {
            system.conflict(root, request)
        } else {
            self.backend.conflict(root, request)
        };
        self.invalidate(root);
        result
    }
}
