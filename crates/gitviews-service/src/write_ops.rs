use crate::{
    BranchError, BranchErrorKind, BranchOutcome, BranchRequest, RepositoryService, WriteError,
    WriteErrorKind, WriteOutcome, WriteRequest,
};
use gitviews_git::GitBackend;
use std::path::Path;

impl<B: GitBackend> RepositoryService<B> {
    pub fn stashes(&self, root: &Path) -> Result<Vec<crate::StashEntry>, crate::StashError> {
        let _guard = self.remote_gate.try_lock().map_err(|_| {
            crate::StashError::new(
                crate::StashErrorKind::InProgress,
                "已有 Git 操作正在执行，请等待完成。",
            )
        })?;
        if let Some(system) = self.configured_system_git() {
            system.stashes(root)
        } else {
            self.backend.stashes(root)
        }
    }

    pub fn stash(
        &self,
        root: &Path,
        request: &crate::StashRequest,
    ) -> Result<crate::StashOutcome, crate::StashError> {
        let _guard = self.remote_gate.try_lock().map_err(|_| {
            crate::StashError::new(
                crate::StashErrorKind::InProgress,
                "已有 Git 操作正在执行，请等待完成。",
            )
        })?;
        let result = if let Some(system) = self.configured_system_git() {
            system.stash(root, request)
        } else {
            self.backend.stash(root, request)
        };
        self.invalidate(root);
        result
    }
    pub fn write(&self, root: &Path, request: &WriteRequest) -> Result<WriteOutcome, WriteError> {
        let _guard = self.remote_gate.try_lock().map_err(|_| {
            WriteError::new(
                WriteErrorKind::InProgress,
                "已有 Git 操作正在执行，请等待完成。",
            )
        })?;
        let result = if let Some(system) = self.configured_system_git() {
            system.write(root, request)
        } else {
            self.backend.write(root, request)
        };
        self.invalidate(root);
        result
    }

    pub fn manage_branch(
        &self,
        root: &Path,
        request: &BranchRequest,
    ) -> Result<BranchOutcome, BranchError> {
        let _guard = self.remote_gate.try_lock().map_err(|_| {
            BranchError::new(
                BranchErrorKind::InProgress,
                "已有 Git 操作正在执行，请等待完成。",
            )
        })?;
        let result = if let Some(system) = self.configured_system_git() {
            system.manage_branch(root, request)
        } else {
            self.backend.manage_branch(root, request)
        };
        self.invalidate(root);
        result
    }
}
