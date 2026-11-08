//! UI-independent repository registration and durable local JSON storage.

use std::{
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{display_path, native_path, paths_equal};

/// One registered working-tree root. The UI obtains the root from the Git service.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryEntry {
    pub root: PathBuf,
    pub name: String,
}

/// An ordered repository list and its current selection, independent of GPUI.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryCatalog {
    pub repositories: Vec<RepositoryEntry>,
    pub active: Option<PathBuf>,
}

impl RepositoryCatalog {
    /// Register a resolved working-tree root, returning whether it was newly added.
    ///
    /// Canonicalization collapses symlink and spelling aliases. This is a directory
    /// registry, so callers first use `RepositoryService::open` to turn a selected
    /// subdirectory into its Git root. Registration does not change the selection.
    pub fn register(&mut self, root: PathBuf, name: String) -> Result<bool, CatalogError> {
        let canonical =
            fs::canonicalize(native_path(&root)).map_err(|source| CatalogError::Io {
                operation: "resolve repository root",
                path: root.clone(),
                source,
            })?;
        if !canonical.is_dir() {
            return Err(CatalogError::InvalidData {
                message: format!(
                    "Repository root is not a directory: {}",
                    display_path(&root)
                ),
            });
        }
        if name.trim().is_empty() {
            return Err(CatalogError::InvalidData {
                message: "Repository name must not be empty".into(),
            });
        }
        if self
            .repositories
            .iter()
            .any(|entry| paths_equal(&entry.root, &canonical))
        {
            return Ok(false);
        }
        self.repositories.push(RepositoryEntry {
            root: canonical,
            name,
        });
        Ok(true)
    }

    /// Remove a registered root. Removing the active entry selects the first one left.
    /// A missing on-disk repository can still be removed by its stored root.
    pub fn remove(&mut self, root: &Path) -> bool {
        let Some(index) = self.find(root) else {
            return false;
        };
        let removed = self.repositories.remove(index);
        if self
            .active
            .as_ref()
            .is_some_and(|active| paths_equal(active, &removed.root))
        {
            self.active = self.repositories.first().map(|entry| entry.root.clone());
        }
        true
    }

    /// Select an existing entry. Unknown roots leave the selection unchanged.
    pub fn set_active(&mut self, root: &Path) -> bool {
        let Some(index) = self.find(root) else {
            return false;
        };
        self.active = Some(self.repositories[index].root.clone());
        true
    }

    fn find(&self, root: &Path) -> Option<usize> {
        self.repositories
            .iter()
            .position(|entry| paths_equal(&entry.root, root))
            .or_else(|| {
                let canonical = fs::canonicalize(native_path(root)).ok()?;
                self.repositories
                    .iter()
                    .position(|entry| paths_equal(&entry.root, &canonical))
            })
    }

    fn validate(&self) -> Result<(), CatalogError> {
        for (index, entry) in self.repositories.iter().enumerate() {
            if !native_path(&entry.root).is_absolute() || entry.name.trim().is_empty() {
                return Err(CatalogError::InvalidData {
                    message: format!(
                        "Repository entry {index} requires an absolute root and a non-empty name"
                    ),
                });
            }
            if self.repositories[..index]
                .iter()
                .any(|previous| paths_equal(&previous.root, &entry.root))
            {
                return Err(CatalogError::InvalidData {
                    message: format!("Duplicate repository root: {}", display_path(&entry.root)),
                });
            }
        }
        if let Some(active) = &self.active
            && !self
                .repositories
                .iter()
                .any(|entry| paths_equal(&entry.root, active))
        {
            return Err(CatalogError::InvalidData {
                message: format!(
                    "Active repository is not registered: {}",
                    display_path(active)
                ),
            });
        }
        Ok(())
    }
}

/// Synchronous file storage. A desktop caller can run its operations on a worker.
///
/// The path is injectable; this type never chooses an application-data directory.
#[derive(Clone, Debug)]
pub struct RepositoryCatalogStore {
    path: PathBuf,
}

impl RepositoryCatalogStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Restore the catalog. Only a missing file means an empty catalog.
    /// Removed or unavailable repository directories remain registered.
    pub fn load(&self) -> Result<RepositoryCatalog, CatalogError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(RepositoryCatalog::default());
            }
            Err(source) => {
                return Err(CatalogError::Io {
                    operation: "read repository catalog",
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let catalog: RepositoryCatalog =
            serde_json::from_slice(&bytes).map_err(|source| CatalogError::Json {
                path: self.path.clone(),
                source,
            })?;
        catalog.validate()?;
        Ok(catalog)
    }

    /// Save through a synced temporary file in the same directory, then replace
    /// the destination with one rename. A failed write preserves the old file.
    ///
    /// An existing malformed catalog is never silently overwritten, including
    /// when a caller attempts to save without successfully restoring it first.
    /// Concurrent application instances are not merged; callers serialize saves.
    pub fn save(&self, catalog: &RepositoryCatalog) -> Result<(), CatalogError> {
        catalog.validate()?;
        // Validate an existing file before any replacement or directory creation.
        self.load()?;
        let parent = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| CatalogError::Io {
            operation: "create repository catalog directory",
            path: parent.to_owned(),
            source,
        })?;
        let bytes = serde_json::to_vec_pretty(catalog).map_err(|source| CatalogError::Json {
            path: self.path.clone(),
            source,
        })?;
        let (mut file, temporary) = create_temporary(parent)?;
        file.write_all(&bytes)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all())
            .map_err(|source| CatalogError::Io {
                operation: "write repository catalog",
                path: temporary.path.clone(),
                source,
            })?;
        // Windows cannot replace an open destination owned without delete-sharing.
        // Close our temporary handle before the standard-library replacement.
        drop(file);
        fs::rename(&temporary.path, &self.path).map_err(|source| CatalogError::Io {
            operation: "replace repository catalog",
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }
}

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

struct TemporaryCatalog {
    path: PathBuf,
}

impl Drop for TemporaryCatalog {
    fn drop(&mut self) {
        // Once renamed, the old temporary path simply no longer exists.
        let _ = fs::remove_file(&self.path);
    }
}

fn create_temporary(parent: &Path) -> Result<(File, TemporaryCatalog), CatalogError> {
    for _ in 0..32 {
        let suffix = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".gitviews-catalog-{}-{suffix}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, TemporaryCatalog { path })),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(CatalogError::Io {
                    operation: "create temporary repository catalog",
                    path,
                    source,
                });
            }
        }
    }
    Err(CatalogError::InvalidData {
        message: format!(
            "Unable to allocate a unique temporary catalog in {}",
            display_path(parent)
        ),
    })
}

/// A registry or persistence failure with the underlying diagnostic preserved.
#[derive(Debug)]
pub enum CatalogError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    InvalidData {
        message: String,
    },
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} at {}: {source}", display_path(path)),
            Self::Json { path, source } => write!(
                formatter,
                "Invalid repository catalog at {}: {source}",
                display_path(path)
            ),
            Self::InvalidData { message } => {
                write!(formatter, "Invalid repository catalog: {message}")
            }
        }
    }
}

impl Error for CatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json { source, .. } => Some(source),
            Self::InvalidData { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            // Windows AppContainer redirects OS temp into a location that
            // disallows rename. Keep disposable test data inside Cargo's
            // ignored build tree so replacement is exercised under the same
            // permissions as the workspace, without touching user app data.
            let parent =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/service-catalog-tests");
            fs::create_dir_all(&parent).unwrap();
            let path = parent.join(format!(
                "gitviews-service-catalog-{}-{nonce}-{}",
                std::process::id(),
                NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn directory(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(&path).unwrap();
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn registration_canonicalizes_roots_and_deduplicates_aliases() {
        let temporary = TestDirectory::new();
        let first = temporary.directory("first repository");
        let second = temporary.directory("second");
        let mut catalog = RepositoryCatalog::default();

        assert!(catalog.register(first.clone(), "First".into()).unwrap());
        assert!(catalog.register(second.clone(), "Second".into()).unwrap());
        assert!(
            !catalog
                .register(first.join("."), "Duplicate label".into())
                .unwrap()
        );
        assert_eq!(catalog.repositories.len(), 2);
        assert_eq!(
            catalog.repositories[0].root,
            fs::canonicalize(first).unwrap()
        );
        assert_eq!(catalog.repositories[0].name, "First");
        assert_eq!(catalog.active, None);
    }

    #[test]
    fn switching_and_removal_preserve_a_valid_selection() {
        let temporary = TestDirectory::new();
        let first = temporary.directory("first");
        let second = temporary.directory("second");
        let mut catalog = RepositoryCatalog::default();
        catalog.register(first.clone(), "First".into()).unwrap();
        catalog.register(second.clone(), "Second".into()).unwrap();

        assert!(catalog.set_active(&second));
        let second_root = fs::canonicalize(&second).unwrap();
        assert_eq!(catalog.active, Some(second_root));
        assert!(!catalog.set_active(&temporary.0.join("unknown")));
        assert!(!catalog.remove(&temporary.0.join("unknown")));
        assert!(catalog.remove(&second));
        assert_eq!(catalog.active, Some(fs::canonicalize(&first).unwrap()));
        assert!(catalog.remove(&first));
        assert_eq!(catalog, RepositoryCatalog::default());
    }

    #[test]
    fn rejected_registration_does_not_change_the_catalog() {
        let temporary = TestDirectory::new();
        let regular_file = temporary.0.join("file.txt");
        fs::write(&regular_file, b"not a directory").unwrap();
        let mut catalog = RepositoryCatalog::default();
        assert!(catalog.register(regular_file, "File".into()).is_err());
        assert!(
            catalog
                .register(temporary.0.join("missing"), "Missing".into())
                .is_err()
        );
        assert!(catalog.register(temporary.0.clone(), "  ".into()).is_err());
        assert_eq!(catalog, RepositoryCatalog::default());
    }

    #[test]
    fn missing_file_loads_empty_and_saved_catalog_restores_and_replaces() {
        let temporary = TestDirectory::new();
        let first = temporary.directory("first");
        let second = temporary.directory("second");
        let store = RepositoryCatalogStore::new(temporary.0.join("state/repositories.json"));
        assert_eq!(store.load().unwrap(), RepositoryCatalog::default());
        assert!(!store.path().exists());

        let mut catalog = RepositoryCatalog::default();
        catalog.register(first.clone(), "仓库一".into()).unwrap();
        catalog
            .register(second.clone(), "Repository two".into())
            .unwrap();
        catalog.set_active(&second);
        store.save(&catalog).unwrap();
        assert_eq!(store.load().unwrap(), catalog);

        catalog.remove(&second);
        store.save(&catalog).unwrap();
        assert_eq!(store.load().unwrap(), catalog);
        let files: Vec<_> = fs::read_dir(store.path().parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(files, ["repositories.json"]);
    }

    #[test]
    fn corrupted_json_is_reported_and_never_overwritten() {
        let temporary = TestDirectory::new();
        let path = temporary.0.join("repositories.json");
        let original = b"{ this catalog is damaged";
        fs::write(&path, original).unwrap();
        let store = RepositoryCatalogStore::new(&path);

        assert!(matches!(store.load(), Err(CatalogError::Json { .. })));
        assert!(store.save(&RepositoryCatalog::default()).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn invalid_schema_and_unregistered_active_roots_are_reported() {
        let temporary = TestDirectory::new();
        let path = temporary.0.join("repositories.json");
        let store = RepositoryCatalogStore::new(&path);
        fs::write(&path, b"{}").unwrap();
        assert!(matches!(store.load(), Err(CatalogError::Json { .. })));

        let invalid = RepositoryCatalog {
            repositories: Vec::new(),
            active: Some(temporary.0.clone()),
        };
        let original = serde_json::to_vec(&invalid).unwrap();
        fs::write(&path, &original).unwrap();
        assert!(matches!(
            store.load(),
            Err(CatalogError::InvalidData { .. })
        ));
        assert!(store.save(&RepositoryCatalog::default()).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn unavailable_repositories_restore_and_can_still_be_removed() {
        let temporary = TestDirectory::new();
        let repository = temporary.directory("later removed repository");
        let store = RepositoryCatalogStore::new(temporary.0.join("repositories.json"));
        let mut catalog = RepositoryCatalog::default();
        catalog
            .register(repository.clone(), "Missing later".into())
            .unwrap();
        catalog.set_active(&repository);
        let canonical = catalog.repositories[0].root.clone();
        store.save(&catalog).unwrap();
        fs::remove_dir(&repository).unwrap();

        let mut restored = store.load().unwrap();
        assert_eq!(restored, catalog);
        assert!(restored.set_active(&canonical));
        assert!(restored.remove(&canonical));
        store.save(&restored).unwrap();
        assert_eq!(store.load().unwrap(), RepositoryCatalog::default());
    }

    #[cfg(windows)]
    #[test]
    fn missing_repository_aliases_can_be_saved_selected_and_removed() {
        let temporary = TestDirectory::new();
        let repository = temporary.directory("Unavailable Repository");
        let canonical = fs::canonicalize(&repository).unwrap();
        let alias = PathBuf::from(display_path(&canonical).to_ascii_uppercase());
        fs::remove_dir(repository).unwrap();
        let store = RepositoryCatalogStore::new(temporary.0.join("repositories.json"));
        let catalog = RepositoryCatalog {
            repositories: vec![RepositoryEntry {
                root: canonical.clone(),
                name: "Missing".into(),
            }],
            active: Some(alias.clone()),
        };
        store.save(&catalog).unwrap();
        let mut restored = store.load().unwrap();
        assert!(restored.set_active(&alias));
        assert_eq!(restored.active, Some(canonical));
        assert!(restored.remove(&alias));
        store.save(&restored).unwrap();
        assert_eq!(store.load().unwrap(), RepositoryCatalog::default());
    }

    #[cfg(windows)]
    #[test]
    fn duplicate_stale_repository_aliases_are_rejected() {
        let catalog = RepositoryCatalog {
            repositories: vec![
                RepositoryEntry {
                    root: PathBuf::from(r"\\?\Z:\gitviews-stale\Repository"),
                    name: "First".into(),
                },
                RepositoryEntry {
                    root: PathBuf::from("z:/GITVIEWS-STALE/repository"),
                    name: "Alias".into(),
                },
            ],
            active: None,
        };
        let error = catalog.validate().unwrap_err();
        assert!(error.to_string().contains("Duplicate repository root"));
    }

    #[test]
    fn invalid_public_model_cannot_replace_a_valid_saved_catalog() {
        let temporary = TestDirectory::new();
        let repository = temporary.directory("repository");
        let store = RepositoryCatalogStore::new(temporary.0.join("repositories.json"));
        let mut catalog = RepositoryCatalog::default();
        catalog.register(repository, "Repository".into()).unwrap();
        store.save(&catalog).unwrap();
        let original = fs::read(store.path()).unwrap();

        catalog.repositories.push(catalog.repositories[0].clone());
        assert!(matches!(
            store.save(&catalog),
            Err(CatalogError::InvalidData { .. })
        ));
        assert_eq!(fs::read(store.path()).unwrap(), original);
    }
}
