//! Application services between the UI and Git implementation.
//!
//! This crate deliberately has no dependency on GPUI. The UI supplies paths and
//! receives repository data; command execution stays behind [`GitBackend`].

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub use gitviews_git::GitError;
use gitviews_git::{Git2Backend, GitBackend};

mod cache;
mod catalog;
mod checkout;
mod conflicts;
mod content_search;
mod git_engine;
mod git_settings;
mod graph_history;
mod history_cache;
mod history_tools;
mod remote;
mod repository_search;
mod repository_setup;
mod search;
mod search_options;
mod settings;
mod ssh_keys;
mod submodule;
mod watcher;
mod worktrees;
mod write_ops;

pub use cache::{PreviewCacheLimits, PreviewCacheStats};
pub use catalog::{CatalogError, RepositoryCatalog, RepositoryCatalogStore, RepositoryEntry};
pub use content_search::{
    ContentSearchError, ContentSearchIndex, ContentSearchLimits, ContentSearchMatch,
    ContentSearchResults,
};
pub use git_engine::{
    GitEngineError, GitEngineKind, GitEnginePreferences, GitEngineStore, SystemGitInstallation,
    detect_system_git, validate_system_git,
};
pub use repository_search::{
    RepositoryContentSearchResults, RepositoryPathSearchResults, RepositorySearchError,
    RepositorySearchIndex, ScopedContentSearchMatch, ScopedFileSearchMatch,
};
pub use search::{FileSearchMatch, SearchError, SearchIndex, SearchIndexLimits, SearchResults};
pub use search_options::{SearchOptions, SearchOptionsError};
pub use settings::{
    AppSettings, AppearanceSettings, LogLevel, LoggingSettings, MAX_SETTINGS_BYTES,
    PreviewSettings, RefreshSettings, SETTINGS_VERSION, SettingsError, SettingsStore,
    ThemePreference,
};
pub use ssh_keys::discover_ssh_keys;
pub use submodule::SubmoduleSnapshot;
pub use watcher::{RepositoryWatcher, WatchBatch, WatchError};

pub use gitviews_git::{
    AuthChallenge, AuthProtocol, BlameLine, BlameView, BranchAction, BranchError, BranchErrorKind,
    BranchInfo, BranchOutcome, BranchRequest, CheckoutError, CheckoutErrorKind, CheckoutOutcome,
    CheckoutRequest, CommitDetails, CommitSummary, ComparedFile, ComparisonView, ConflictAction,
    ConflictError, ConflictErrorKind, ConflictFile, ConflictOperation, ConflictOutcome,
    ConflictRequest, ConflictResolution, ConflictState, ConflictVersions, DiffLine, DiffLineKind,
    FileChange, FileDiff, FileTreeEntry, FileTreeKind, GitRemoteSettings, GitSettings, GraphCommit,
    GraphHistoryFilter, GraphHistoryPage, GraphHistoryRequest, GraphHistoryRow,
    GraphHistorySnapshot, GraphReference, GraphReferenceKind, Head, HistoryGraphPage, HistoryPage,
    HistoryToolsError, HistoryToolsErrorKind, HistoryToolsRequest, HistoryToolsResult,
    RecoveryEntry, RecoveryPage, RemoteAction, RemoteAuth, RemoteControl, RemoteError,
    RemoteErrorKind, RemoteOutcome, RemotePhase, RemoteProgress, RemoteRequest, RepositoryDetails,
    RepositorySetupKind, RepositorySetupOutcome, RepositorySetupRequest, RepositorySnapshot,
    RepositoryTree, SourceFile, StashAction, StashEntry, StashError, StashErrorKind, StashOutcome,
    StashRequest, SubmoduleInfo, TextEncoding, TreeWarning, WorktreeAction, WorktreeError,
    WorktreeErrorKind, WorktreeInfo, WorktreeOutcome, WorktreeRequest, WriteAction, WriteError,
    WriteErrorKind, WriteOutcome, WriteRequest, display_path, native_path, path_relative_to,
    paths_equal, remote_protocol,
};

/// Coordinates repository reads through a replaceable backend.
#[derive(Clone, Debug)]
pub struct RepositoryService<B = Git2Backend> {
    backend: B,
    cache: Arc<Mutex<cache::PreviewCache>>,
    remote_gate: Arc<Mutex<()>>,
    history_cache: Arc<Mutex<history_cache::HistoryCache>>,
    git_engine: Arc<Mutex<GitEnginePreferences>>,
}

/// One worker-side refresh result. A UI coordinator can keep one scan running
/// and schedule another when watcher events mark that scan dirty.
#[derive(Clone, Debug)]
pub struct RepositoryScan {
    pub snapshot: RepositorySnapshot,
    pub details: RepositoryDetails,
    pub tree: RepositoryTree,
}

impl<B: GitBackend> RepositoryService<B> {
    /// Create a service with an explicit Git backend.
    pub fn new(backend: B) -> Self {
        Self::with_cache_limits(backend, PreviewCacheLimits::default())
    }

    pub fn with_cache_limits(backend: B, limits: PreviewCacheLimits) -> Self {
        Self {
            backend,
            cache: Arc::new(Mutex::new(cache::PreviewCache::new(limits))),
            remote_gate: Arc::new(Mutex::new(())),
            history_cache: Arc::new(Mutex::new(history_cache::HistoryCache::default())),
            git_engine: Arc::new(Mutex::new(GitEnginePreferences::default())),
        }
    }

    /// Open a repository root or subdirectory and read its overview.
    pub fn open(&self, path: &Path) -> Result<RepositorySnapshot, ServiceError> {
        self.backend.inspect(path).map_err(ServiceError::Git)
    }

    /// Show repository identity and HEAD before reading status and commit history.
    /// Empty changes/history lists are pending, rather than clean or absent data.
    pub fn open_metadata(&self, path: &Path) -> Result<RepositorySnapshot, ServiceError> {
        self.backend
            .inspect_metadata(path)
            .map_err(ServiceError::Git)
    }

    /// Synchronous by design: invoke on a background worker. Reads can observe
    /// concurrent external Git changes; a pending watcher batch should trigger
    /// another scan rather than allowing two competing scans in the UI.
    pub fn scan(&self, path: &Path) -> Result<RepositoryScan, ServiceError> {
        self.scan_with_cancel(path, &AtomicBool::new(false))
    }

    /// Stop a superseded refresh between independent native reads. An in-flight
    /// libgit2 call finishes normally; no further expensive phase is started.
    pub fn scan_with_cancel(
        &self,
        path: &Path,
        cancel: &AtomicBool,
    ) -> Result<RepositoryScan, ServiceError> {
        let check_cancel = || {
            if cancel.load(Ordering::Acquire) {
                Err(ServiceError::Cancelled {
                    operation: "scan repository",
                })
            } else {
                Ok(())
            }
        };
        check_cancel()?;
        let snapshot = self.open(path)?;
        check_cancel()?;
        let details = self.details(&snapshot.root)?;
        check_cancel()?;
        let tree = self.tree_with_changes(&snapshot.root, &snapshot.changes)?;
        check_cancel()?;
        Ok(RepositoryScan {
            snapshot,
            details,
            tree,
        })
    }

    /// Read registered worktrees and local/remote branch metadata.
    pub fn details(&self, path: &Path) -> Result<RepositoryDetails, ServiceError> {
        self.backend.details(path).map_err(ServiceError::Git)
    }

    /// Read tracked and untracked files, directory structure, and submodules.
    pub fn tree(&self, path: &Path) -> Result<RepositoryTree, ServiceError> {
        self.backend.tree(path).map_err(ServiceError::Git)
    }

    /// Build the tree with status already read from the same repository snapshot.
    pub fn tree_with_changes(
        &self,
        path: &Path,
        changes: &[FileChange],
    ) -> Result<RepositoryTree, ServiceError> {
        self.backend
            .tree_with_changes(path, changes)
            .map_err(ServiceError::Git)
    }

    /// Preview a repository-relative UTF-8 file, comparing HEAD/index or index/worktree.
    pub fn diff(&self, path: &Path, file: &str, staged: bool) -> Result<FileDiff, ServiceError> {
        self.cached_diff(path, file, staged)
            .map(|value| (*value).clone())
    }

    pub fn cached_diff(
        &self,
        root: &Path,
        file: &str,
        staged: bool,
    ) -> Result<Arc<FileDiff>, ServiceError> {
        match self.cached_preview(root, file, cache::PreviewKind::Diff { staged }, || {
            self.backend
                .diff(root, file, staged)
                .map(|value| cache::PreviewValue::Diff(Arc::new(value)))
        })? {
            cache::PreviewValue::Diff(value) => Ok(value),
            _ => unreachable!("typed diff cache key"),
        }
    }

    pub fn source(
        &self,
        root: &Path,
        file: &str,
        revision: Option<&str>,
    ) -> Result<SourceFile, ServiceError> {
        self.cached_source(root, file, revision)
            .map(|value| (*value).clone())
    }

    pub fn cached_source(
        &self,
        root: &Path,
        file: &str,
        revision: Option<&str>,
    ) -> Result<Arc<SourceFile>, ServiceError> {
        match self.cached_preview(
            root,
            file,
            cache::PreviewKind::Source {
                revision: revision.map(str::to_owned),
            },
            || {
                self.backend
                    .source(root, file, revision)
                    .map(|value| cache::PreviewValue::Source(Arc::new(value)))
            },
        )? {
            cache::PreviewValue::Source(value) => Ok(value),
            _ => unreachable!("typed source cache key"),
        }
    }

    pub fn history(
        &self,
        root: &Path,
        skip: usize,
        limit: usize,
    ) -> Result<HistoryPage, ServiceError> {
        self.backend
            .history(root, skip, limit)
            .map_err(ServiceError::Git)
    }

    pub fn commit(&self, root: &Path, id: &str) -> Result<CommitDetails, ServiceError> {
        self.backend.commit(root, id).map_err(ServiceError::Git)
    }

    pub fn commit_diff(&self, root: &Path, id: &str, file: &str) -> Result<FileDiff, ServiceError> {
        self.cached_commit_diff(root, id, file)
            .map(|value| (*value).clone())
    }

    pub fn cached_commit_diff(
        &self,
        root: &Path,
        id: &str,
        file: &str,
    ) -> Result<Arc<FileDiff>, ServiceError> {
        match self.cached_preview(
            root,
            file,
            cache::PreviewKind::CommitDiff {
                revision: id.to_owned(),
            },
            || {
                self.backend
                    .commit_diff(root, id, file)
                    .map(|value| cache::PreviewValue::Diff(Arc::new(value)))
            },
        )? {
            cache::PreviewValue::Diff(value) => Ok(value),
            _ => unreachable!("typed commit diff cache key"),
        }
    }

    /// Call before manual or watcher-driven scans. An in-flight read from an
    /// earlier generation cannot repopulate an invalidated cache.
    pub fn invalidate(&self, root: &Path) {
        self.cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate(root, None);
    }

    /// Paths can be absolute watcher paths or repository-relative file paths.
    /// Git metadata changes invalidate all previews for the root.
    pub fn invalidate_paths(&self, root: &Path, paths: &[std::path::PathBuf]) {
        self.cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invalidate(root, Some(paths));
    }

    pub fn cache_stats(&self) -> PreviewCacheStats {
        self.cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stats()
    }

    /// Apply a new budget to the existing shared preview cache and release any
    /// oldest excess entries immediately. Existing service clones and reads
    /// already in flight use the same budget.
    pub fn set_cache_limits(&self, limits: PreviewCacheLimits) {
        self.cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set_limits(limits);
    }

    #[tracing::instrument(level = "debug", skip(self, load))]
    fn cached_preview(
        &self,
        root: &Path,
        file: &str,
        kind: cache::PreviewKind,
        load: impl FnOnce() -> Result<cache::PreviewValue, GitError>,
    ) -> Result<cache::PreviewValue, ServiceError> {
        let started = std::time::Instant::now();
        let signature = cache::PreviewSignature::read(root, file, &kind);
        let key;
        {
            let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
            key = cache.key(root, file, kind, signature);
            if let Some(value) = cache.get(&key) {
                tracing::debug!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "Preview cache hit"
                );
                return Ok(value);
            }
        }
        // Never hold the cache mutex across Git or filesystem scanning.
        tracing::debug!("Preview cache miss");
        let value = load().map_err(ServiceError::Git)?;
        // Do not retain a result if metadata changed while the backend was reading.
        if cache::PreviewSignature::read(root, file, &key.kind) == key.signature {
            self.cache
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(key, value.clone());
        }
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Preview read completed"
        );
        Ok(value)
    }
}

impl Default for RepositoryService<Git2Backend> {
    fn default() -> Self {
        Self::new(Git2Backend)
    }
}

/// An application-level repository operation failure.
#[derive(Debug)]
pub enum ServiceError {
    /// Reading the selected repository failed.
    Git(GitError),
    /// A superseded background read stopped between synchronous backend calls.
    Cancelled { operation: &'static str },
    /// A requested child path is not an initialized, registered submodule root.
    Submodule {
        path: std::path::PathBuf,
        reason: String,
    },
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git(error) => write!(formatter, "Repository operation failed: {error}"),
            Self::Cancelled { operation } => write!(formatter, "操作已取消：{operation}"),
            Self::Submodule { path, reason } => {
                write!(
                    formatter,
                    "Cannot read submodule {}: {reason}",
                    display_path(path)
                )
            }
        }
    }
}

impl Error for ServiceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Git(error) => Some(error),
            Self::Submodule { .. } | Self::Cancelled { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct StubBackend;

    impl GitBackend for StubBackend {
        fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
            Ok(RepositorySnapshot {
                root: path.to_owned(),
                name: "example".to_owned(),
                head: Head::Unborn("main".to_owned()),
                changes: Vec::new(),
                recent_commits: Vec::new(),
            })
        }
    }

    #[test]
    fn open_passes_the_selected_path_to_the_backend() {
        let service = RepositoryService::new(StubBackend);
        let snapshot = service.open(Path::new("selected repository")).unwrap();
        assert_eq!(snapshot.root, PathBuf::from("selected repository"));
        assert_eq!(snapshot.head, Head::Unborn("main".to_owned()));
    }

    struct FailingBackend;

    impl GitBackend for FailingBackend {
        fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
            Err(GitError::NotRepository {
                path: path.to_owned(),
            })
        }
    }

    #[test]
    fn preserves_the_underlying_error_for_the_ui() {
        let error = RepositoryService::new(FailingBackend)
            .open(Path::new("not a repository"))
            .unwrap_err();
        assert!(matches!(
            error,
            ServiceError::Git(GitError::NotRepository { .. })
        ));
        assert!(error.source().is_some());
        assert!(error.to_string().contains("not a repository"));
    }

    struct TreeBackend;

    impl GitBackend for TreeBackend {
        fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
            StubBackend.inspect(path)
        }

        fn tree(&self, path: &Path) -> Result<RepositoryTree, GitError> {
            Ok(RepositoryTree {
                entries: vec![FileTreeEntry {
                    path: path.to_string_lossy().into_owned(),
                    kind: FileTreeKind::Symlink,
                    index_status: ' ',
                    worktree_status: 'M',
                }],
                submodules: Vec::new(),
                warnings: Vec::new(),
            })
        }
    }

    #[test]
    fn tree_passes_the_root_and_preserves_file_kinds_and_statuses() {
        let tree = RepositoryService::new(TreeBackend)
            .tree(Path::new("selected root"))
            .unwrap();
        assert_eq!(tree.entries[0].path, "selected root");
        assert_eq!(tree.entries[0].kind, FileTreeKind::Symlink);
        assert_eq!(tree.entries[0].worktree_status, 'M');
    }

    #[test]
    fn tree_reports_an_unsupported_backend_instead_of_an_empty_tree() {
        let error = RepositoryService::new(StubBackend)
            .tree(Path::new("selected root"))
            .unwrap_err();
        assert!(matches!(
            error,
            ServiceError::Git(GitError::Unsupported { .. })
        ));
    }

    struct CancelScanBackend {
        cancel: Arc<AtomicBool>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        cancel_after: usize,
    }

    impl CancelScanBackend {
        fn completed(&self) {
            if self.calls.fetch_add(1, Ordering::Relaxed) + 1 == self.cancel_after {
                self.cancel.store(true, Ordering::Release);
            }
        }
    }

    impl GitBackend for CancelScanBackend {
        fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
            self.completed();
            StubBackend.inspect(path)
        }

        fn details(&self, _: &Path) -> Result<RepositoryDetails, GitError> {
            self.completed();
            Ok(RepositoryDetails::default())
        }

        fn tree_with_changes(
            &self,
            _: &Path,
            _: &[FileChange],
        ) -> Result<RepositoryTree, GitError> {
            self.completed();
            Ok(RepositoryTree::default())
        }
    }

    #[test]
    fn superseded_refresh_does_not_start_additional_backend_phases() {
        for cancel_after in 0..=3 {
            let cancel = Arc::new(AtomicBool::new(cancel_after == 0));
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let service = RepositoryService::new(CancelScanBackend {
                cancel: Arc::clone(&cancel),
                calls: Arc::clone(&calls),
                cancel_after,
            });
            assert!(matches!(
                service.scan_with_cancel(Path::new("root"), &cancel),
                Err(ServiceError::Cancelled { .. })
            ));
            assert_eq!(calls.load(Ordering::Relaxed), cancel_after);
        }
    }
}
