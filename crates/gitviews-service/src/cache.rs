use std::{
    collections::VecDeque,
    fs,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use crate::{FileDiff, SourceFile, native_path, path_relative_to, paths_equal};

#[derive(Clone, Copy, Debug)]
pub struct PreviewCacheLimits {
    pub max_entries: usize,
    pub max_bytes: usize,
}
impl Default for PreviewCacheLimits {
    fn default() -> Self {
        Self {
            max_entries: 48,
            max_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PreviewCacheStats {
    pub entries: usize,
    pub bytes: usize,
    pub hits: u64,
    pub misses: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PreviewKind {
    Diff { staged: bool },
    Source { revision: Option<String> },
    CommitDiff { revision: String },
}

#[derive(Clone, Debug)]
pub(crate) enum PreviewValue {
    Diff(Arc<FileDiff>),
    Source(Arc<SourceFile>),
}
impl PreviewValue {
    fn bytes(&self) -> usize {
        match self {
            Self::Source(value) => {
                std::mem::size_of::<SourceFile>() + value.path.capacity() + value.text.capacity()
            }
            Self::Diff(value) => {
                std::mem::size_of::<FileDiff>()
                    + value.path.capacity()
                    + value.old_text.capacity()
                    + value.new_text.capacity()
                    + value.patch.capacity()
                    + value.lines.capacity() * std::mem::size_of::<crate::DiffLine>()
                    + value
                        .lines
                        .iter()
                        .map(|line| line.text.capacity())
                        .sum::<usize>()
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
    is_dir: bool,
}

/// Cheap metadata checks supplement explicit watcher/manual invalidation. HEAD,
/// index, selected refs and the selected working file participate in each key.
#[derive(Clone, Debug, Eq)]
pub(crate) struct PreviewSignature(Vec<(PathBuf, Option<FileStamp>)>);
impl PartialEq for PreviewSignature {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self
                .0
                .iter()
                .zip(&other.0)
                .all(|((left, stamp), (right, other_stamp))| {
                    paths_equal(left, right) && stamp == other_stamp
                })
    }
}
impl PreviewSignature {
    pub fn read(root: &Path, file: &str, kind: &PreviewKind) -> Self {
        let root = normalized(root);
        let (gitdir, common) = git_directories(&root);
        let mut paths = vec![
            root.join(".git"),
            gitdir.join("HEAD"),
            gitdir.join("index"),
            gitdir.join("commondir"),
            common.join("packed-refs"),
            common.join("config"),
            common.join("shallow"),
        ];
        if let Ok(head) = fs::read_to_string(gitdir.join("HEAD"))
            && let Some(reference) = head.trim().strip_prefix("ref: ")
            && safe_relative(reference)
        {
            paths.push(common.join(reference));
            paths.push(gitdir.join(reference));
        }
        let revision = match kind {
            PreviewKind::Source { revision } => revision.as_deref(),
            PreviewKind::CommitDiff { revision } => Some(revision.as_str()),
            _ => None,
        };
        if let Some(revision) = revision.filter(|revision| safe_relative(revision)) {
            paths.push(common.join("refs/heads").join(revision));
            paths.push(common.join("refs/tags").join(revision));
            if revision.starts_with("refs/") {
                paths.push(common.join(revision));
            }
        }
        if matches!(
            kind,
            PreviewKind::Diff { staged: false } | PreviewKind::Source { revision: None }
        ) && safe_relative(file)
        {
            paths.push(root.join(file));
        }
        Self(
            paths
                .into_iter()
                .map(|path| {
                    let stamp = fs::symlink_metadata(&path).ok().map(|metadata| FileStamp {
                        len: metadata.len(),
                        modified: metadata.modified().ok(),
                        is_dir: metadata.is_dir(),
                    });
                    (path, stamp)
                })
                .collect(),
        )
    }
}

fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

pub(crate) fn normalized(path: &Path) -> PathBuf {
    let path = native_path(path);
    fs::canonicalize(&path).unwrap_or(path)
}

/// Works for ordinary repositories and `.git` indirections used by linked
/// worktrees and submodules. Relative paths are resolved against their file.
pub(crate) fn git_directories(root: &Path) -> (PathBuf, PathBuf) {
    let dotgit = root.join(".git");
    let gitdir = if dotgit.is_file() {
        fs::read_to_string(&dotgit)
            .ok()
            .and_then(|text| {
                text.trim()
                    .strip_prefix("gitdir:")
                    .map(str::trim)
                    .map(|path| native_path(Path::new(path)))
            })
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                }
            })
            .unwrap_or_else(|| dotgit.clone())
    } else {
        dotgit
    };
    let common = fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .map(|text| native_path(Path::new(text.trim())))
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                gitdir.join(path)
            }
        })
        .unwrap_or_else(|| gitdir.clone());
    (normalized(&gitdir), normalized(&common))
}

#[derive(Clone, Debug, Eq)]
pub(crate) struct PreviewKey {
    root: PathBuf,
    file: String,
    pub kind: PreviewKind,
    pub signature: PreviewSignature,
    generation: u64,
}

impl PartialEq for PreviewKey {
    fn eq(&self, other: &Self) -> bool {
        paths_equal(&self.root, &other.root)
            && self.file == other.file
            && self.kind == other.kind
            && self.signature == other.signature
            && self.generation == other.generation
    }
}

#[derive(Debug)]
struct Entry {
    key: PreviewKey,
    value: PreviewValue,
    bytes: usize,
}

#[derive(Debug)]
pub(crate) struct PreviewCache {
    limits: PreviewCacheLimits,
    entries: VecDeque<Entry>,
    stats: PreviewCacheStats,
    generation: u64,
}
impl PreviewCache {
    pub fn new(limits: PreviewCacheLimits) -> Self {
        Self {
            limits,
            entries: VecDeque::new(),
            stats: PreviewCacheStats::default(),
            generation: 0,
        }
    }
    /// Change the shared cache budget without replacing cached values or the
    /// service. Oldest entries are released immediately, including when a zero
    /// limit disables retention. Reads already in flight use these same limits
    /// when they later insert their result.
    pub fn set_limits(&mut self, limits: PreviewCacheLimits) {
        self.limits = limits;
        while self.entries.len() > self.limits.max_entries
            || self.stats.bytes > self.limits.max_bytes
        {
            let Some(entry) = self.entries.pop_front() else {
                break;
            };
            self.stats.bytes = self.stats.bytes.saturating_sub(entry.bytes);
        }
        self.recount();
    }
    pub fn key(
        &self,
        root: &Path,
        file: &str,
        kind: PreviewKind,
        signature: PreviewSignature,
    ) -> PreviewKey {
        PreviewKey {
            root: normalized(root),
            file: file.into(),
            kind,
            signature,
            generation: self.generation,
        }
    }
    pub fn get(&mut self, key: &PreviewKey) -> Option<PreviewValue> {
        if let Some(position) = self.entries.iter().position(|entry| entry.key == *key) {
            let entry = self.entries.remove(position)?;
            let value = entry.value.clone();
            self.entries.push_back(entry);
            self.stats.hits += 1;
            Some(value)
        } else {
            self.stats.misses += 1;
            None
        }
    }
    pub fn insert(&mut self, key: PreviewKey, value: PreviewValue) {
        if key.generation != self.generation {
            return;
        }
        let revision_bytes = match &key.kind {
            PreviewKind::Source {
                revision: Some(revision),
            }
            | PreviewKind::CommitDiff { revision } => revision.capacity(),
            _ => 0,
        };
        let bytes = std::mem::size_of::<Entry>()
            + value.bytes()
            + key.file.capacity()
            + key.root.as_os_str().len() * 2
            + revision_bytes
            + key.signature.0.capacity() * std::mem::size_of::<(PathBuf, Option<FileStamp>)>()
            + key
                .signature
                .0
                .iter()
                .map(|(path, _)| path.as_os_str().len() * 2)
                .sum::<usize>();
        if self.limits.max_entries == 0 || bytes > self.limits.max_bytes {
            return;
        }
        // A changed signature for the same file replaces its obsolete allocation.
        self.entries.retain(|entry| {
            !(paths_equal(&entry.key.root, &key.root)
                && entry.key.file == key.file
                && entry.key.kind == key.kind)
        });
        self.recount();
        while self.entries.len() >= self.limits.max_entries
            || self.stats.bytes.saturating_add(bytes) > self.limits.max_bytes
        {
            if let Some(entry) = self.entries.pop_front() {
                self.stats.bytes -= entry.bytes;
            } else {
                break;
            }
        }
        self.entries.push_back(Entry { key, value, bytes });
        self.recount();
    }
    pub fn invalidate(&mut self, root: &Path, paths: Option<&[PathBuf]>) {
        let root = normalized(root);
        let all = paths.is_none_or(|paths| {
            paths.is_empty()
                || paths.iter().any(|path| {
                    let relative = path_relative_to(path, &root).unwrap_or_else(|| path.to_owned());
                    relative.components().any(|component| {
                        component
                            .as_os_str()
                            .to_string_lossy()
                            .eq_ignore_ascii_case(".git")
                    }) || path_relative_to(path, &root).is_none() && path.is_absolute()
                })
        });
        self.entries.retain(|entry| {
            !paths_equal(&entry.key.root, &root)
                || !all
                    && !paths.unwrap_or_default().iter().any(|path| {
                        let absolute = if path.is_absolute() {
                            path.clone()
                        } else {
                            root.join(path)
                        };
                        let cached = root.join(&entry.key.file);
                        path_relative_to(&cached, &absolute).is_some()
                    })
        });
        self.generation = self.generation.wrapping_add(1);
        for entry in &mut self.entries {
            entry.key.generation = self.generation;
        }
        self.recount();
    }
    fn recount(&mut self) {
        self.stats.entries = self.entries.len();
        self.stats.bytes = self.entries.iter().map(|entry| entry.bytes).sum();
    }
    pub fn stats(&self) -> PreviewCacheStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GitBackend, GitError, Head, RepositoryService, RepositorySnapshot, TextEncoding};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone, Default)]
    struct Backend {
        reads: Arc<AtomicUsize>,
    }
    impl GitBackend for Backend {
        fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
            Ok(RepositorySnapshot {
                root: path.to_owned(),
                name: "test".into(),
                head: Head::Unborn("main".into()),
                changes: Vec::new(),
                recent_commits: Vec::new(),
            })
        }
        fn source(
            &self,
            _root: &Path,
            file: &str,
            _revision: Option<&str>,
        ) -> Result<SourceFile, GitError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            if file == "error" {
                return Err(GitError::Unsupported {
                    operation: "test error",
                });
            }
            Ok(SourceFile {
                path: file.into(),
                text: if file == "large" {
                    "x".repeat(1024 * 1024)
                } else {
                    format!("source {file}")
                },
                encoding: TextEncoding::Utf8,
                line_count: 1,
            })
        }
        fn diff(&self, _root: &Path, file: &str, staged: bool) -> Result<FileDiff, GitError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(FileDiff {
                path: file.into(),
                old_text: String::new(),
                new_text: staged.to_string(),
                patch: String::new(),
                lines: Vec::new(),
            })
        }
        fn commit_diff(&self, root: &Path, _id: &str, file: &str) -> Result<FileDiff, GitError> {
            self.diff(root, file, true)
        }
    }

    #[test]
    fn preview_hits_reuse_arc_and_keep_source_stage_and_revision_keys_distinct() {
        let backend = Backend::default();
        let reads = Arc::clone(&backend.reads);
        let service = RepositoryService::new(backend);
        let root = Path::new("fake-root");
        let first = service.cached_source(root, "a", None).unwrap();
        let second = service.cached_source(root, "a", None).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        service.cached_source(root, "a", Some(":index")).unwrap();
        service.cached_source(root, "a", Some("HEAD")).unwrap();
        service.cached_diff(root, "a", false).unwrap();
        service.cached_diff(root, "a", true).unwrap();
        service.cached_commit_diff(root, "commit1", "a").unwrap();
        service.cached_commit_diff(root, "commit2", "a").unwrap();
        assert_eq!(reads.load(Ordering::Relaxed), 7);
        assert_eq!(service.cache_stats().hits, 1);
    }

    #[test]
    fn least_recently_used_entries_are_evicted_with_both_limits_enforced() {
        let backend = Backend::default();
        let reads = Arc::clone(&backend.reads);
        let service = RepositoryService::with_cache_limits(
            backend,
            PreviewCacheLimits {
                max_entries: 2,
                max_bytes: 1024 * 1024,
            },
        );
        let root = Path::new("fake-root");
        for file in ["a", "b", "a", "c", "b"] {
            service.cached_source(root, file, None).unwrap();
        }
        assert_eq!(reads.load(Ordering::Relaxed), 4);
        assert_eq!(service.cache_stats().entries, 2);
        assert!(service.cache_stats().bytes <= 1024 * 1024);
        let small = RepositoryService::with_cache_limits(
            Backend::default(),
            PreviewCacheLimits {
                max_entries: 48,
                max_bytes: 4096,
            },
        );
        small.cached_source(root, "large", None).unwrap();
        small.cached_source(root, "large", None).unwrap();
        assert_eq!(small.backend.reads.load(Ordering::Relaxed), 2);
        assert_eq!(small.cache_stats().entries, 0);
    }

    #[test]
    fn shrinking_shared_cache_limits_immediately_evicts_lru_entries() {
        let backend = Backend::default();
        let reads = Arc::clone(&backend.reads);
        let service = RepositoryService::new(backend);
        // The worker owns a clone made before the settings change. Its cache
        // must see the new limits, rather than retaining a replaced old cache.
        let worker = service.clone();
        let root = Path::new("fake-root");
        for file in ["a", "b", "c", "a"] {
            service.cached_source(root, file, None).unwrap();
        }
        assert_eq!(reads.load(Ordering::Relaxed), 3);
        service.set_cache_limits(PreviewCacheLimits {
            max_entries: 1,
            max_bytes: 1024 * 1024,
        });
        assert_eq!(worker.cache_stats().entries, 1);
        worker.cached_source(root, "a", None).unwrap();
        assert_eq!(reads.load(Ordering::Relaxed), 3, "MRU value survives");
        worker.cached_source(root, "b", None).unwrap();
        assert_eq!(service.cache_stats().entries, 1);
        service.cached_source(root, "a", None).unwrap();
        assert_eq!(reads.load(Ordering::Relaxed), 5);

        // A byte budget below one entry releases even the newest allocation.
        service.set_cache_limits(PreviewCacheLimits {
            max_entries: 48,
            max_bytes: 1,
        });
        assert_eq!(worker.cache_stats().entries, 0);
        assert_eq!(worker.cache_stats().bytes, 0);
        worker.cached_source(root, "a", None).unwrap();
        worker.cached_source(root, "a", None).unwrap();
        assert_eq!(reads.load(Ordering::Relaxed), 7);
        assert_eq!(service.cache_stats().entries, 0);
    }

    #[test]
    fn shrinking_byte_limit_keeps_the_most_recent_value_that_fits() {
        let service = RepositoryService::new(Backend::default());
        let root = Path::new("fake-root");
        service.cached_source(root, "a", None).unwrap();
        let one_entry_bytes = service.cache_stats().bytes;
        service.cached_source(root, "b", None).unwrap();
        assert_eq!(service.cache_stats().entries, 2);
        service.set_cache_limits(PreviewCacheLimits {
            max_entries: 48,
            max_bytes: one_entry_bytes,
        });
        assert_eq!(service.cache_stats().entries, 1);
        assert_eq!(service.cache_stats().bytes, one_entry_bytes);
        service.cached_source(root, "b", None).unwrap();
        assert_eq!(service.backend.reads.load(Ordering::Relaxed), 2);
        service.set_cache_limits(PreviewCacheLimits {
            max_entries: 0,
            max_bytes: usize::MAX,
        });
        assert_eq!(service.cache_stats().entries, 0);
        service.cached_source(root, "b", None).unwrap();
        assert_eq!(service.cache_stats().entries, 0);
    }

    #[test]
    fn root_and_path_invalidation_and_errors_do_not_return_stale_entries() {
        let backend = Backend::default();
        let reads = Arc::clone(&backend.reads);
        let service = RepositoryService::new(backend);
        let root = Path::new("fake-root");
        service.cached_source(root, "a", None).unwrap();
        service.cached_source(root, "b", None).unwrap();
        service.invalidate_paths(root, &[PathBuf::from("a")]);
        service.cached_source(root, "b", None).unwrap();
        service.cached_source(root, "a", None).unwrap();
        assert_eq!(reads.load(Ordering::Relaxed), 3);
        service.invalidate(root);
        service.cached_source(root, "b", None).unwrap();
        assert_eq!(reads.load(Ordering::Relaxed), 4);
        assert!(service.cached_source(root, "error", None).is_err());
        assert!(service.cached_source(root, "error", None).is_err());
        assert_eq!(reads.load(Ordering::Relaxed), 6);
    }

    #[cfg(windows)]
    #[test]
    fn windows_alias_watcher_paths_invalidate_only_the_affected_cached_files() {
        let service = RepositoryService::new(Backend::default());
        let root = Path::new(r"\\?\Z:\gitviews-cache-alias\Repository");
        let alias = Path::new("z:/GITVIEWS-CACHE-ALIAS/repository");
        let first = service.cached_source(root, "src/a.txt", None).unwrap();
        let other = service.cached_source(root, "src/b.txt", None).unwrap();
        let hit = service.cached_source(alias, "src/a.txt", None).unwrap();
        assert!(Arc::ptr_eq(&first, &hit));
        service.invalidate_paths(
            alias,
            &[PathBuf::from(
                r"Z:\GITVIEWS-CACHE-ALIAS\Repository\SRC\A.TXT",
            )],
        );
        let other_hit = service.cached_source(root, "src/b.txt", None).unwrap();
        assert!(Arc::ptr_eq(&other, &other_hit));
        let refreshed = service.cached_source(root, "src/a.txt", None).unwrap();
        assert!(!Arc::ptr_eq(&first, &refreshed));
        assert_eq!(service.backend.reads.load(Ordering::Relaxed), 3);
    }

    #[cfg(windows)]
    #[test]
    fn windows_root_aliases_preserve_case_sensitive_git_object_file_keys() {
        let service = RepositoryService::new(Backend::default());
        let first = service
            .cached_source(
                Path::new(r"\\?\Z:\gitviews-cache-alias\Repository"),
                "Readme",
                Some("HEAD"),
            )
            .unwrap();
        let other = service
            .cached_source(
                Path::new("z:/GITVIEWS-CACHE-ALIAS/repository"),
                "README",
                Some("HEAD"),
            )
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &other));
        assert_eq!(service.backend.reads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn invalidation_during_a_backend_read_cannot_reinsert_the_old_generation() {
        let root = Path::new("fake-root");
        let mut cache = PreviewCache::new(PreviewCacheLimits::default());
        let kind = PreviewKind::Source { revision: None };
        let signature = PreviewSignature::read(root, "a", &kind);
        let key = cache.key(root, "a", kind, signature);
        cache.invalidate(root, None);
        cache.insert(
            key,
            PreviewValue::Source(Arc::new(SourceFile {
                path: "a".into(),
                text: "old generation".into(),
                encoding: TextEncoding::Utf8,
                line_count: 1,
            })),
        );
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn head_index_and_working_metadata_changes_bust_cached_previews() {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/service-cache-tests");
        fs::create_dir_all(&parent).unwrap();
        let root = parent.join(format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        fs::write(root.join(".git/index"), "index").unwrap();
        fs::write(root.join("a"), "a").unwrap();
        let backend = Backend::default();
        let reads = Arc::clone(&backend.reads);
        let service = RepositoryService::new(backend);
        service.cached_source(&root, "a", None).unwrap();
        service.cached_source(&root, "a", None).unwrap();
        fs::write(root.join("a"), "a changed").unwrap();
        service.cached_source(&root, "a", None).unwrap();
        fs::write(root.join(".git/index"), "index changed").unwrap();
        service.cached_source(&root, "a", None).unwrap();
        fs::write(root.join(".git/refs/heads/main"), "new head").unwrap();
        service.cached_source(&root, "a", None).unwrap();
        assert_eq!(reads.load(Ordering::Relaxed), 4);
        fs::remove_dir_all(root).unwrap();
    }
}
