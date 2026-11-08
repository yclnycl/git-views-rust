use std::path::Path;

use gitviews_git::GitBackend;

use crate::{
    CheckoutError, CheckoutErrorKind, CheckoutOutcome, CheckoutRequest, RepositoryService,
};

impl<B: GitBackend> RepositoryService<B> {
    /// Checkout and remote writes share one gate, including all service clones.
    pub fn checkout(
        &self,
        root: &Path,
        request: &CheckoutRequest,
    ) -> Result<CheckoutOutcome, CheckoutError> {
        let _guard = self.remote_gate.try_lock().map_err(|_| {
            CheckoutError::new(
                CheckoutErrorKind::InProgress,
                "已有 Git 操作正在执行，请等待完成。",
            )
        })?;
        let result = if let Some(system) = self.configured_system_git() {
            system.checkout(root, request)
        } else {
            self.backend.checkout(root, request)
        };
        self.invalidate(root);
        result
    }
}
