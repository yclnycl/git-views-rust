use crate::{
    RemoteControl, RemoteError, RemoteErrorKind, RepositoryService, RepositorySetupOutcome,
    RepositorySetupRequest,
};
use gitviews_git::GitBackend;

impl<B: GitBackend> RepositoryService<B> {
    /// Clone/init participates in the same gate as all repository mutations.
    pub fn setup_repository(
        &self,
        request: &RepositorySetupRequest,
        control: &RemoteControl,
    ) -> Result<RepositorySetupOutcome, RemoteError> {
        let _guard = self.remote_gate.try_lock().map_err(|_| {
            RemoteError::new(
                RemoteErrorKind::InProgress,
                "已有 Git 操作正在执行，请等待完成或取消。",
            )
        })?;
        if let Some(system) = self.configured_system_git() {
            system.setup_repository(request, control)
        } else {
            self.backend.setup_repository(request, control)
        }
    }
}
