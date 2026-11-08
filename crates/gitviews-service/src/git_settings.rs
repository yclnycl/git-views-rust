use std::path::Path;

use gitviews_git::GitBackend;

use crate::{GitError, GitSettings, RepositoryService, ServiceError};

impl<B: GitBackend> RepositoryService<B> {
    pub fn git_settings(&self, root: &Path) -> Result<GitSettings, ServiceError> {
        self.backend.git_settings(root).map_err(ServiceError::Git)
    }

    /// Settings and other Git mutations share the same gate across service clones.
    pub fn save_git_settings(
        &self,
        root: &Path,
        settings: &GitSettings,
    ) -> Result<(), ServiceError> {
        let _guard = self.remote_gate.try_lock().map_err(|_| {
            ServiceError::Git(GitError::InvalidRequest {
                operation: "保存 Git 配置",
                reason: "已有 Git 操作正在执行，请等待完成。",
            })
        })?;
        self.backend
            .save_git_settings(root, settings)
            .map_err(ServiceError::Git)?;
        self.invalidate(root);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RepositorySnapshot;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Clone)]
    struct Backend {
        saves: Arc<AtomicUsize>,
    }

    impl GitBackend for Backend {
        fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
            Err(GitError::NotRepository {
                path: path.to_owned(),
            })
        }

        fn git_settings(&self, _: &Path) -> Result<GitSettings, GitError> {
            Ok(GitSettings {
                user_name: "fixture".into(),
                ..Default::default()
            })
        }

        fn save_git_settings(&self, _: &Path, _: &GitSettings) -> Result<(), GitError> {
            self.saves.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn settings_save_uses_the_shared_git_mutation_gate() {
        let saves = Arc::new(AtomicUsize::new(0));
        let service = RepositoryService::new(Backend {
            saves: saves.clone(),
        });
        let cloned = service.clone();
        let guard = service.remote_gate.lock().unwrap();
        let root = Path::new("fixture");
        assert_eq!(cloned.git_settings(root).unwrap().user_name, "fixture");
        assert!(
            cloned
                .save_git_settings(root, &GitSettings::default())
                .is_err()
        );
        assert_eq!(saves.load(Ordering::SeqCst), 0);
        drop(guard);
        cloned
            .save_git_settings(root, &GitSettings::default())
            .unwrap();
        assert_eq!(saves.load(Ordering::SeqCst), 1);
    }
}
