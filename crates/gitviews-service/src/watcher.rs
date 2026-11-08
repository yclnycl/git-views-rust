//! Native notifications are collected here; scanning remains a worker-side job.
use crate::cache::git_directories;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::BTreeSet,
    error::Error,
    fmt,
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const DEBOUNCE: Duration = Duration::from_millis(300);
const MIN_DEBOUNCE: Duration = Duration::from_millis(50);
const MAX_DELAY: Duration = Duration::from_secs(2);
const MAX_PATHS: usize = 1024;
const MAX_ERRORS: usize = 8;

#[derive(Clone, Debug)]
pub struct WatchBatch {
    pub root: PathBuf,
    /// Absolute paths. Overflow or metadata changes warrant a complete scan.
    pub paths: Vec<PathBuf>,
    pub metadata_changed: bool,
    pub overflowed: bool,
    pub errors: Vec<String>,
}

/// Owns the native watcher. Dropping it releases platform resources and stops
/// callbacks. Call `poll` about every 100ms and run scans outside the UI thread.
pub struct RepositoryWatcher {
    root: PathBuf,
    _watcher: RecommendedWatcher,
    pending: Arc<Mutex<Pending>>,
    lost: Arc<AtomicBool>,
    debounce: Duration,
}

impl RepositoryWatcher {
    pub fn new(root: PathBuf) -> Result<Self, WatchError> {
        Self::new_with_debounce(root, DEBOUNCE)
    }

    /// Build a watcher with a bounded quiet period. Continuous writes still
    /// produce a batch within the fixed two-second maximum delay.
    pub fn new_with_debounce(root: PathBuf, debounce: Duration) -> Result<Self, WatchError> {
        let root = fs::canonicalize(crate::native_path(&root))
            .map_err(|source| WatchError::Io { path: root, source })?;
        let (gitdir, common) = git_directories(&root);
        if !gitdir.is_dir() {
            return Err(WatchError::InvalidRepository { root });
        }
        let mut metadata = vec![gitdir, common];
        // Linked submodules may keep metadata outside the superproject. Discover
        // configured paths once during setup; the callback never scans or reads.
        if let Ok(text) = fs::read_to_string(root.join(".gitmodules")) {
            for line in text.lines().take(4096) {
                let Some((key, value)) = line.trim().split_once('=') else {
                    continue;
                };
                let value = value.trim().trim_matches('"');
                if key.trim() == "path" && safe_relative(value) {
                    let (directory, common) = git_directories(&root.join(value));
                    if directory.is_dir() {
                        metadata.extend([directory, common]);
                    }
                }
            }
        }
        metadata.sort();
        metadata.dedup();
        metadata.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        let filter = Filter {
            root: root.clone(),
            metadata: metadata.clone(),
            ignored_build: ignored_build_directories(&root),
            build_filter_valid: AtomicBool::new(true),
        };
        let pending = Arc::new(Mutex::new(Pending::default()));
        let lost = Arc::new(AtomicBool::new(false));
        let callback_pending = Arc::clone(&pending);
        let callback_lost = Arc::clone(&lost);
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<Event>| match callback_pending
                .try_lock()
            {
                Ok(mut pending) => pending.push(event, &filter, Instant::now()),
                Err(TryLockError::Poisoned(error)) => {
                    error.into_inner().push(event, &filter, Instant::now())
                }
                Err(TryLockError::WouldBlock) => {
                    callback_lost.store(true, Ordering::Release);
                }
            })
            .map_err(|source| WatchError::Notify {
                path: root.clone(),
                source,
            })?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(|source| WatchError::Notify {
                path: root.clone(),
                source,
            })?;
        let mut watched = vec![root.clone()];
        // Shortest ancestors first prevents redundant recursive watches.
        metadata.sort_by_key(|path| path.components().count());
        for directory in metadata {
            if !directory.is_dir()
                || watched
                    .iter()
                    .any(|ancestor| crate::path_relative_to(&directory, ancestor).is_some())
            {
                continue;
            }
            watcher
                .watch(&directory, RecursiveMode::Recursive)
                .map_err(|source| WatchError::Notify {
                    path: directory.clone(),
                    source,
                })?;
            watched.push(directory);
        }
        Ok(Self {
            root,
            _watcher: watcher,
            pending,
            lost,
            debounce: bounded_debounce(debounce),
        })
    }

    /// Change the quiet period for this watcher, including already queued
    /// events. No watcher recreation or callback configuration is necessary.
    pub fn set_debounce(&mut self, debounce: Duration) {
        self.debounce = bounded_debounce(debounce);
    }

    /// Never waits for the notification callback's mutex. Bursts coalesce for
    /// the configured quiet period, capped at 2s for continuous writes.
    pub fn poll(&mut self) -> Option<WatchBatch> {
        let mut pending = match self.pending.try_lock() {
            Ok(pending) => pending,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        let now = Instant::now();
        if self.lost.swap(false, Ordering::AcqRel) {
            pending.overflowed = true;
            pending.touch(now);
        }
        pending.take_ready(&self.root, now, self.debounce)
    }
}

fn bounded_debounce(debounce: Duration) -> Duration {
    debounce.clamp(MIN_DEBOUNCE, MAX_DELAY)
}

#[derive(Default)]
struct Pending {
    paths: BTreeSet<PathBuf>,
    errors: Vec<String>,
    metadata_changed: bool,
    overflowed: bool,
    first: Option<Instant>,
    last: Option<Instant>,
}
impl Pending {
    fn touch(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }
    fn push(&mut self, event: notify::Result<Event>, filter: &Filter, now: Instant) {
        let mut relevant = false;
        match event {
            Ok(event) => {
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                if event.need_rescan() {
                    self.overflowed = true;
                    relevant = true;
                }
                if event.paths.len() > MAX_PATHS {
                    self.overflowed = true;
                    relevant = true;
                }
                for path in event.paths.into_iter().take(MAX_PATHS + 1) {
                    if let Some(metadata) = filter.relevant(&path) {
                        relevant = true;
                        self.metadata_changed |= metadata;
                        if self.paths.len() < MAX_PATHS {
                            self.paths.insert(path);
                        } else {
                            self.overflowed = true;
                        }
                    }
                }
            }
            Err(error) => {
                relevant = true;
                self.overflowed = true;
                if self.errors.len() < MAX_ERRORS {
                    self.errors
                        .push(error.to_string().chars().take(1024).collect());
                }
            }
        }
        if relevant {
            self.touch(now);
        }
    }
    fn take_ready(&mut self, root: &Path, now: Instant, debounce: Duration) -> Option<WatchBatch> {
        let first = self.first?;
        let last = self.last?;
        if now.saturating_duration_since(last) < debounce
            && now.saturating_duration_since(first) < MAX_DELAY
        {
            return None;
        }
        let batch = WatchBatch {
            root: root.to_owned(),
            paths: std::mem::take(&mut self.paths).into_iter().collect(),
            metadata_changed: self.metadata_changed,
            overflowed: self.overflowed,
            errors: std::mem::take(&mut self.errors),
        };
        self.first = None;
        self.last = None;
        self.metadata_changed = false;
        self.overflowed = false;
        Some(batch)
    }
}

struct Filter {
    root: PathBuf,
    metadata: Vec<PathBuf>,
    ignored_build: Vec<PathBuf>,
    build_filter_valid: AtomicBool,
}
impl Filter {
    fn relevant(&self, path: &Path) -> Option<bool> {
        for directory in &self.metadata {
            if let Some(relative) = crate::path_relative_to(path, directory) {
                let parts: Vec<_> = relative
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy())
                    .collect();
                if parts
                    .first()
                    .is_some_and(|part| matches!(part.as_ref(), "objects" | "logs"))
                    || parts.last().is_some_and(|part| part.ends_with(".lock"))
                {
                    return None;
                }
                // Index/HEAD changes can make an ignored build file tracked.
                // Disable the setup-time optimization until the watcher is
                // recreated rather than risk hiding newly tracked content.
                self.build_filter_valid.store(false, Ordering::Relaxed);
                return Some(true);
            }
        }
        let relative = crate::path_relative_to(path, &self.root)?;
        if self.build_filter_valid.load(Ordering::Relaxed)
            && self
                .ignored_build
                .iter()
                .any(|directory| crate::path_relative_to(&relative, directory).is_some())
        {
            return None;
        }
        if relative
            .file_name()
            .is_some_and(|name| name == ".git" || name == ".gitmodules" || name == ".gitignore")
        {
            self.build_filter_valid.store(false, Ordering::Relaxed);
            return Some(true);
        }
        Some(false)
    }
}

fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && Path::new(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

/// Filter conventional build output only when explicitly ignored and absent
/// from the index. Conservative byte matching protects tracked build/source
/// paths with uncompressed index names; newer/split indexes disable this
/// optimization because their paths may be compressed or stored elsewhere.
fn ignored_build_directories(root: &Path) -> Vec<PathBuf> {
    let Ok(ignore) = fs::read_to_string(root.join(".gitignore")) else {
        return Vec::new();
    };
    let (gitdir, _) = git_directories(root);
    let mut index = Vec::new();
    if let Ok(file) = File::open(gitdir.join("index"))
        && (file
            .take(32 * 1024 * 1024 + 1)
            .read_to_end(&mut index)
            .is_err()
            || index.len() > 32 * 1024 * 1024)
    {
        return Vec::new();
    }
    if !index.is_empty()
        && (index.len() < 12
            || &index[..4] != b"DIRC"
            || u32::from_be_bytes(index[4..8].try_into().unwrap_or_default()) > 3
            || index.windows(4).any(|bytes| bytes == b"link"))
    {
        return Vec::new();
    }
    ignore
        .lines()
        .filter_map(|line| {
            let directory = line.trim().trim_start_matches('/').trim_end_matches('/');
            if !matches!(
                directory,
                "target" | "node_modules" | "dist" | "build" | ".next"
            ) || ignore.lines().any(|line| line.trim().starts_with('!'))
            {
                return None;
            }
            let needle = format!("{directory}/");
            if index
                .windows(needle.len())
                .any(|window| window == needle.as_bytes())
            {
                None
            } else {
                Some(PathBuf::from(directory))
            }
        })
        .collect()
}

#[derive(Debug)]
pub enum WatchError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Notify {
        path: PathBuf,
        source: notify::Error,
    },
    InvalidRepository {
        root: PathBuf,
    },
}
impl fmt::Display for WatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "Cannot watch {}: {source}",
                    crate::display_path(path)
                )
            }
            Self::Notify { path, source } => write!(
                formatter,
                "Notification setup failed at {}: {source}",
                crate::display_path(path)
            ),
            Self::InvalidRepository { root } => {
                write!(
                    formatter,
                    "No Git metadata found for {}",
                    crate::display_path(root)
                )
            }
        }
    }
}
impl Error for WatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Notify { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn filter() -> Filter {
        Filter {
            root: PathBuf::from("repo"),
            metadata: vec![PathBuf::from("repo/.git"), PathBuf::from("outside/git")],
            ignored_build: vec![PathBuf::from("target")],
            build_filter_valid: AtomicBool::new(true),
        }
    }
    fn event(path: impl Into<PathBuf>) -> notify::Result<Event> {
        Ok(Event::new(EventKind::Any).add_path(path.into()))
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_alias_events_keep_workdir_metadata_and_build_filtering() {
        let filter = Filter {
            root: PathBuf::from(r"\\?\Z:\Repositories\Example"),
            metadata: vec![PathBuf::from(r"\\?\Z:\Repositories\Example\.git")],
            ignored_build: vec![PathBuf::from("node_modules")],
            build_filter_valid: AtomicBool::new(true),
        };
        assert_eq!(
            filter.relevant(Path::new("z:/repositories/EXAMPLE/src/main.rs")),
            Some(false)
        );
        assert_eq!(
            filter.relevant(Path::new(
                r"Z:\REPOSITORIES\example\NODE_MODULES\dependency.js"
            )),
            None
        );
        assert_eq!(
            filter.relevant(Path::new(r"Z:\Repositories\Example-sibling\src\main.rs")),
            None
        );
        assert_eq!(
            filter.relevant(Path::new("//?/z:/REPOSITORIES/example/.git/index")),
            Some(true)
        );
        assert!(!filter.build_filter_valid.load(Ordering::Relaxed));
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_alias_events_include_external_linked_metadata() {
        let filter = Filter {
            root: PathBuf::from(r"\\?\Z:\Repositories\Example"),
            metadata: vec![PathBuf::from(r"\\?\Z:\Metadata\Linked")],
            ignored_build: Vec::new(),
            build_filter_valid: AtomicBool::new(true),
        };
        assert_eq!(
            filter.relevant(Path::new("z:/METADATA/linked/HEAD")),
            Some(true)
        );
        assert_eq!(
            filter.relevant(Path::new("z:/METADATA/linked/objects/ab/cdef")),
            None
        );
        assert_eq!(
            filter.relevant(Path::new("z:/METADATA/linked/index.lock")),
            None
        );
        assert_eq!(
            filter.relevant(Path::new("z:/METADATA/linked-sibling/HEAD")),
            None
        );
    }
    #[test]
    fn bursts_are_debounced_and_drained_once() {
        let mut pending = Pending::default();
        let now = Instant::now();
        pending.push(event("repo/src/a.rs"), &filter(), now);
        pending.push(
            event("repo/src/a.rs"),
            &filter(),
            now + Duration::from_millis(200),
        );
        assert!(
            pending
                .take_ready(
                    Path::new("repo"),
                    now + Duration::from_millis(400),
                    DEBOUNCE
                )
                .is_none()
        );
        let batch = pending
            .take_ready(
                Path::new("repo"),
                now + Duration::from_millis(500),
                DEBOUNCE,
            )
            .unwrap();
        assert_eq!(batch.paths, [PathBuf::from("repo/src/a.rs")]);
        assert!(!batch.metadata_changed);
        assert!(
            pending
                .take_ready(Path::new("repo"), now + Duration::from_secs(4), DEBOUNCE)
                .is_none()
        );
    }
    #[test]
    fn continuous_writes_flush_after_two_seconds() {
        let mut pending = Pending::default();
        let now = Instant::now();
        for tick in 0..=20 {
            pending.push(
                event("repo/file"),
                &filter(),
                now + Duration::from_millis(tick * 100),
            );
        }
        assert!(
            pending
                .take_ready(Path::new("repo"), now + MAX_DELAY, DEBOUNCE)
                .is_some()
        );
    }

    #[test]
    fn updated_debounce_changes_when_an_already_queued_batch_is_ready() {
        let mut pending = Pending::default();
        let now = Instant::now();
        pending.push(event("repo/file"), &filter(), now);
        // Changing to a longer quiet period must defer a batch that the old
        // default would have released. The event timestamps stay untouched.
        assert!(
            pending
                .take_ready(
                    Path::new("repo"),
                    now + DEBOUNCE,
                    Duration::from_millis(1000),
                )
                .is_none()
        );
        // Shortening it applies to those same queued events immediately.
        assert!(
            pending
                .take_ready(Path::new("repo"), now + DEBOUNCE, MIN_DEBOUNCE)
                .is_some()
        );
        assert!(
            pending
                .take_ready(Path::new("repo"), now + MAX_DELAY, MIN_DEBOUNCE)
                .is_none()
        );
    }

    #[test]
    fn configurable_debounce_preserves_the_continuous_write_deadline() {
        let mut pending = Pending::default();
        let now = Instant::now();
        pending.push(event("repo/file"), &filter(), now);
        pending.push(
            event("repo/file"),
            &filter(),
            now + Duration::from_millis(1900),
        );
        assert!(
            pending
                .take_ready(
                    Path::new("repo"),
                    now + Duration::from_millis(1999),
                    MAX_DELAY,
                )
                .is_none()
        );
        assert!(
            pending
                .take_ready(Path::new("repo"), now + MAX_DELAY, MAX_DELAY)
                .is_some()
        );
    }

    #[test]
    fn debounce_bounds_prevent_zero_delay_and_unbounded_waiting() {
        for requested in [Duration::ZERO, Duration::from_millis(49), MIN_DEBOUNCE] {
            assert_eq!(bounded_debounce(requested), MIN_DEBOUNCE);
        }
        assert_eq!(bounded_debounce(DEBOUNCE), DEBOUNCE);
        for requested in [MAX_DELAY, Duration::from_secs(60), Duration::MAX] {
            assert_eq!(bounded_debounce(requested), MAX_DELAY);
        }
    }

    #[test]
    fn git_objects_logs_locks_and_confirmed_build_output_are_filtered() {
        let filter = filter();
        for path in [
            "repo/.git/objects/ab/hash",
            "outside/git/logs/HEAD",
            "outside/git/index.lock",
            "repo/target/debug/app.exe",
        ] {
            assert_eq!(filter.relevant(Path::new(path)), None);
        }
        for path in [
            "repo/.git/index",
            "outside/git/refs/heads/main",
            "repo/.gitmodules",
        ] {
            assert_eq!(filter.relevant(Path::new(path)), Some(true));
        }
        assert_eq!(filter.relevant(Path::new("repo/src/build.rs")), Some(false));
    }
    #[test]
    fn queue_and_error_buffers_are_bounded_and_request_rescan() {
        let mut pending = Pending::default();
        let now = Instant::now();
        for index in 0..MAX_PATHS + 100 {
            pending.push(event(format!("repo/file-{index}")), &filter(), now);
        }
        for _ in 0..20 {
            pending.push(
                Err(notify::Error::generic("watch overflow")),
                &filter(),
                now,
            );
        }
        let batch = pending
            .take_ready(Path::new("repo"), now + DEBOUNCE, DEBOUNCE)
            .unwrap();
        assert_eq!(batch.paths.len(), MAX_PATHS);
        assert_eq!(batch.errors.len(), MAX_ERRORS);
        assert!(batch.overflowed);
    }

    #[test]
    fn metadata_updates_disable_a_stale_build_ignore_optimization() {
        let filter = filter();
        assert_eq!(filter.relevant(Path::new("repo/target/source.rs")), None);
        assert_eq!(filter.relevant(Path::new("repo/.git/index")), Some(true));
        assert_eq!(
            filter.relevant(Path::new("repo/target/source.rs")),
            Some(false)
        );
        // A branch named objects/logs is metadata, not an object database.
        assert_eq!(
            filter.relevant(Path::new("repo/.git/refs/heads/objects")),
            Some(true)
        );
    }

    #[test]
    fn native_watcher_observes_worktree_and_external_git_metadata() {
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let parent =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/service-watcher-tests");
        fs::create_dir_all(&parent).unwrap();
        let directory = parent.join(format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let fixture = Fixture(directory);
        let worktree = fixture.0.join("worktree");
        let gitdir = fixture.0.join("external-gitdir");
        let common = fixture.0.join("external-common");
        for path in [&worktree, &gitdir, &common] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}", fs::canonicalize(&gitdir).unwrap().display()),
        )
        .unwrap();
        fs::write(
            gitdir.join("commondir"),
            fs::canonicalize(&common).unwrap().display().to_string(),
        )
        .unwrap();
        let mut watcher = RepositoryWatcher::new(worktree.clone()).unwrap();
        fs::write(worktree.join("observed.txt"), "working content").unwrap();
        fs::write(common.join("packed-refs"), "new metadata").unwrap();
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut saw_file = false;
        let mut saw_metadata = false;
        while Instant::now() < deadline {
            if let Some(batch) = watcher.poll() {
                assert!(
                    batch.errors.is_empty(),
                    "native watcher errors: {:?}",
                    batch.errors
                );
                saw_file |= batch.overflowed
                    || batch
                        .paths
                        .iter()
                        .any(|path| path.ends_with("observed.txt"));
                saw_metadata |= batch.overflowed || batch.metadata_changed;
                if saw_file && saw_metadata {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            saw_file && saw_metadata,
            "missing native working-file or linked Git metadata event"
        );
        drop(watcher);
    }
}
