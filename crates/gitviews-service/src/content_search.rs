//! Literal working-file search over an existing repository tree. No directory
//! walk or Git process is needed: the tree supplies the permitted candidates.
//! Searches use bounded parallel readers, linear-time matching, and a bounded
//! fingerprint/text cache. Changed files are re-read; unchanged trigram misses
//! need only metadata. Neither links nor another repository are traversed.

use std::{
    borrow::Cow,
    collections::HashMap,
    error::Error,
    fmt,
    fs::{self, File, Metadata},
    io::Read,
    ops::Range,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use crate::search_options::{CompiledSearchOptions, SearchOptions};
use gitviews_git::{FileTreeKind, RepositoryTree, native_path, paths_equal};

const MAX_QUERY_BYTES: usize = 1024;
const MAX_QUERY_CHARS: usize = 256;
const MAX_RESULTS: usize = 200;
const READ_CHUNK: usize = 64 * 1024;
const MAX_HIGHLIGHTS: usize = 32;
const BLOOM_WORDS: usize = 16;

#[derive(Clone, Copy, Debug)]
pub struct ContentSearchLimits {
    pub max_files: usize,
    pub max_index_bytes: usize,
    pub max_file_bytes: usize,
    pub max_read_bytes: u64,
    pub max_cache_bytes: usize,
    pub max_cached_file_bytes: usize,
    pub workers: usize,
    pub time_budget: Duration,
}

impl Default for ContentSearchLimits {
    fn default() -> Self {
        Self {
            max_files: 100_000,
            max_index_bytes: 32 * 1024 * 1024,
            max_file_bytes: 8 * 1024 * 1024,
            max_read_bytes: 128 * 1024 * 1024,
            max_cache_bytes: 32 * 1024 * 1024,
            max_cached_file_bytes: 256 * 1024,
            workers: 4,
            time_budget: Duration::from_secs(3),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentSearchError {
    Cancelled,
    QueryTooLong,
    InvalidQuery,
    InvalidLimit,
    InvalidRoot(String),
    IndexLimit,
    InvalidOptions(String),
}

impl fmt::Display for ContentSearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("内容搜索已取消。"),
            Self::QueryTooLong => formatter.write_str("内容查询不能超过 256 个字符或 1024 字节。"),
            Self::InvalidQuery => {
                formatter.write_str("内容搜索支持单行纯文本，请移除换行或空字符。")
            }
            Self::InvalidLimit => formatter.write_str("内容搜索预算和结果上限必须大于零。"),
            Self::InvalidRoot(message) => write!(formatter, "无法安全读取内容搜索目录：{message}"),
            Self::IndexLimit => formatter.write_str("内容搜索文件索引达到上限，请缩小仓库范围。"),
            Self::InvalidOptions(message) => formatter.write_str(message),
        }
    }
}

impl Error for ContentSearchError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentSearchMatch {
    pub path: Arc<str>,
    /// One-based source line number, including when earlier lines use CRLF.
    pub line_number: usize,
    /// A bounded, original-case UTF-8 excerpt. Ellipses mark omitted text.
    pub snippet: Arc<str>,
    /// UTF-8 byte ranges in `snippet`; safe for the UI's highlighted text runs.
    pub highlights: Vec<Range<usize>>,
}

#[derive(Clone, Debug, Default)]
pub struct ContentSearchResults {
    pub matches: Vec<ContentSearchMatch>,
    pub indexed_files: usize,
    /// Candidates checked, including unchanged files rejected by the cache.
    pub scanned_files: usize,
    /// Unchanged text or trigram summaries reused without reading file contents.
    pub cached_files: usize,
    pub skipped_files: usize,
    pub binary_files: usize,
    pub oversized_files: usize,
    pub read_bytes: u64,
    pub warning_count: usize,
    /// More results/candidates may exist; counts are lower bounds when set.
    pub truncated: bool,
    pub timed_out: bool,
    pub elapsed: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Fingerprint {
    len: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
}

impl Fingerprint {
    fn read(metadata: &Metadata) -> Option<Self> {
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok()?,
            created: metadata.created().ok(),
        })
    }
}

#[derive(Clone, Debug)]
struct CachedFile {
    fingerprint: Fingerprint,
    bloom: [u64; BLOOM_WORDS],
    text: Option<Arc<str>>,
    binary: bool,
}

#[derive(Debug)]
struct ContentCache {
    files: Vec<Option<CachedFile>>,
    text_bytes: usize,
    eviction_cursor: usize,
}

impl ContentCache {
    fn put(&mut self, index: usize, mut value: CachedFile, max_bytes: usize) {
        if let Some(old) = self.files[index].take() {
            self.text_bytes = self
                .text_bytes
                .saturating_sub(old.text.as_ref().map_or(0, |t| t.len()));
        }
        let cost = value.text.as_ref().map_or(0, |text| text.len());
        if cost > max_bytes {
            value.text = None;
        } else {
            // Evict only cached text. Tiny fingerprints/summaries stay bounded
            // by max_files and still avoid reads for definite trigram misses.
            for _ in 0..self.files.len() {
                if self.text_bytes.saturating_add(cost) <= max_bytes {
                    break;
                }
                let position = self.eviction_cursor;
                self.eviction_cursor = (position + 1) % self.files.len();
                if let Some(text) = self.files[position]
                    .as_mut()
                    .and_then(|entry| entry.text.take())
                {
                    self.text_bytes = self.text_bytes.saturating_sub(text.len());
                }
            }
            self.text_bytes = self.text_bytes.saturating_add(cost);
        }
        self.files[index] = Some(value);
    }
}

/// Keep one instance per repository-tree generation. A query is synchronous and
/// belongs on a UI worker; concurrent/cancelled queries cannot corrupt the cache.
#[derive(Debug)]
pub struct ContentSearchIndex {
    root: PathBuf,
    paths: Vec<Arc<str>>,
    limits: ContentSearchLimits,
    warning_count: usize,
    bytes: usize,
    cache: Mutex<ContentCache>,
}

impl ContentSearchIndex {
    pub fn from_tree(root: &Path, tree: &RepositoryTree) -> Result<Self, ContentSearchError> {
        Self::with_limits(
            root,
            tree,
            ContentSearchLimits::default(),
            &AtomicBool::new(false),
        )
    }

    pub fn with_limits(
        root: &Path,
        tree: &RepositoryTree,
        limits: ContentSearchLimits,
        cancel: &AtomicBool,
    ) -> Result<Self, ContentSearchError> {
        if limits.max_files == 0
            || limits.max_index_bytes == 0
            || limits.max_file_bytes == 0
            || limits.max_read_bytes == 0
            || limits.workers == 0
            || limits.time_budget.is_zero()
        {
            return Err(ContentSearchError::InvalidLimit);
        }
        check_cancel(cancel)?;
        let root = checked_root(root)?;
        let module_count = tree.submodules.len().saturating_add(
            tree.entries
                .iter()
                .filter(|entry| entry.kind == FileTreeKind::Submodule)
                .count(),
        );
        if module_count > limits.max_files
            || module_count.saturating_mul(std::mem::size_of::<Cow<'_, str>>())
                > limits.max_index_bytes
        {
            return Err(ContentSearchError::IndexLimit);
        }
        // Normalize boundary names only. No module is opened or traversed.
        let module_paths = tree
            .submodules
            .iter()
            .map(|module| module.path.as_str())
            .chain(
                tree.entries
                    .iter()
                    .filter(|entry| entry.kind == FileTreeKind::Submodule)
                    .map(|entry| entry.path.as_str()),
            );
        let mut module_bytes = module_count.saturating_mul(std::mem::size_of::<Cow<'_, str>>());
        for path in module_paths {
            check_cancel(cancel)?;
            module_bytes = module_bytes.saturating_add(path.len());
            if path.len() > 16 * 1024 || module_bytes > limits.max_index_bytes {
                return Err(ContentSearchError::IndexLimit);
            }
        }
        let mut modules: Vec<Cow<'_, str>> = tree
            .submodules
            .iter()
            .map(|module| module_key(&module.path))
            .collect();
        modules.extend(
            tree.entries
                .iter()
                .filter(|entry| entry.kind == FileTreeKind::Submodule)
                .map(|entry| module_key(&entry.path)),
        );
        modules.sort_unstable();
        modules.dedup();
        let mut paths = Vec::with_capacity(tree.entries.len().min(limits.max_files));
        let mut index_bytes = module_bytes.saturating_add(paths.capacity().saturating_mul(
            std::mem::size_of::<Arc<str>>() + std::mem::size_of::<Option<CachedFile>>(),
        ));
        if index_bytes > limits.max_index_bytes {
            return Err(ContentSearchError::IndexLimit);
        }
        let mut warning_count = tree.warnings.len();
        for entry in &tree.entries {
            check_cancel(cancel)?;
            if entry.kind != FileTreeKind::File
                || entry.worktree_status == 'D'
                || (entry.index_status == 'D' && entry.worktree_status == ' ')
            {
                continue;
            }
            let path = entry.path.as_str();
            if !usable_path(path) {
                warning_count = warning_count.saturating_add(1);
                continue;
            }
            if module_ancestor(path, &modules) {
                continue;
            }
            if paths.len() == limits.max_files {
                return Err(ContentSearchError::IndexLimit);
            }
            index_bytes = index_bytes.saturating_add(path.len() + 2 * std::mem::size_of::<usize>());
            if index_bytes > limits.max_index_bytes {
                return Err(ContentSearchError::IndexLimit);
            }
            paths.push(Arc::from(path));
        }
        paths.sort_unstable();
        paths.dedup();
        let files = vec![None; paths.len()];
        Ok(Self {
            root,
            paths,
            limits,
            warning_count,
            bytes: index_bytes,
            cache: Mutex::new(ContentCache {
                files,
                text_bytes: 0,
                eviction_cursor: 0,
            }),
        })
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
    pub fn estimated_bytes(&self) -> usize {
        self.bytes
    }

    /// Single-line literal search. ASCII ignores case, Unicode (including
    /// Chinese) preserves exact scalars. No regex or fuzzy content expansion.
    pub fn query(
        &self,
        query: &str,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<ContentSearchResults, ContentSearchError> {
        self.query_with_options(query, limit, cancel, &SearchOptions::default())
    }

    pub fn query_with_options(
        &self,
        query: &str,
        limit: usize,
        cancel: &AtomicBool,
        options: &SearchOptions,
    ) -> Result<ContentSearchResults, ContentSearchError> {
        self.query_with_budget(
            query,
            limit,
            cancel,
            options,
            Instant::now() + self.limits.time_budget,
            self.limits.max_read_bytes,
            "",
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn query_with_budget(
        &self,
        query: &str,
        limit: usize,
        cancel: &AtomicBool,
        options: &SearchOptions,
        deadline: Instant,
        max_read_bytes: u64,
        prefix: &str,
    ) -> Result<ContentSearchResults, ContentSearchError> {
        check_cancel(cancel)?;
        if limit == 0 || max_read_bytes == 0 {
            return Err(ContentSearchError::InvalidLimit);
        }
        if query.len() > MAX_QUERY_BYTES
            || query.chars().take(MAX_QUERY_CHARS + 1).count() > MAX_QUERY_CHARS
        {
            return Err(ContentSearchError::QueryTooLong);
        }
        if query.contains(['\0', '\r', '\n']) {
            return Err(ContentSearchError::InvalidQuery);
        }
        let filters = CompiledSearchOptions::scoped(options, query, prefix)
            .map_err(|error| ContentSearchError::InvalidOptions(error.0))?;
        let started = Instant::now();
        let mut result = ContentSearchResults {
            indexed_files: self.len(),
            warning_count: self.warning_count,
            ..Default::default()
        };
        if query.is_empty() || self.is_empty() {
            result.elapsed = started.elapsed();
            return Ok(result);
        }
        // Reject a replaced/junction repository root even when a prior tree is
        // still displayed. Never silently search its new target.
        let root = checked_root(&self.root)?;
        if !paths_equal(&root, &self.root) {
            return Err(ContentSearchError::InvalidRoot(
                "仓库根目录已改变，请刷新后重试。".into(),
            ));
        }
        let needle = if !options.case_sensitive && filters.regex.is_none() {
            Needle::new(query)
        } else {
            Needle::with_options(query, options.case_sensitive, filters.regex.clone())
        };
        let state = QueryState {
            next: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            read_budget: AtomicU64::new(0),
            match_count: AtomicUsize::new(0),
            cancel,
            deadline: deadline.min(started + self.limits.time_budget),
            limit: limit.min(MAX_RESULTS),
            max_read_bytes: max_read_bytes.min(self.limits.max_read_bytes),
        };
        let workers = self.limits.workers.min(4).min(self.len());
        let partials = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|_| scope.spawn(|| self.worker(&needle, &state, &filters)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("content search worker panicked"))
                .collect::<Vec<_>>()
        });
        check_cancel(cancel)?;
        for partial in partials {
            result.matches.extend(partial.matches);
            result.scanned_files += partial.scanned_files;
            result.cached_files += partial.cached_files;
            result.skipped_files += partial.skipped_files;
            result.binary_files += partial.binary_files;
            result.oversized_files += partial.oversized_files;
            result.read_bytes += partial.read_bytes;
        }
        result
            .matches
            .sort_unstable_by(|a, b| a.path.cmp(&b.path).then(a.line_number.cmp(&b.line_number)));
        result.matches.truncate(state.limit);
        result.timed_out = state.timed_out.load(Ordering::Relaxed);
        result.truncated = state.stop.load(Ordering::Relaxed);
        result.elapsed = started.elapsed();
        Ok(result)
    }

    fn worker(
        &self,
        needle: &Needle,
        state: &QueryState<'_>,
        filters: &CompiledSearchOptions,
    ) -> ContentSearchResults {
        let mut result = ContentSearchResults::default();
        let mut directories = DirectoryChecks::default();
        while !state.interrupted() {
            let position = state.next.fetch_add(1, Ordering::Relaxed);
            let Some(path) = self.paths.get(position) else {
                break;
            };
            if !filters.allows(path) {
                continue;
            }
            result.scanned_files += 1;
            let Some((absolute, metadata)) = checked_file(&self.root, path, &mut directories)
            else {
                result.skipped_files += 1;
                continue;
            };
            if metadata.len() > self.limits.max_file_bytes as u64 {
                result.oversized_files += 1;
                result.skipped_files += 1;
                continue;
            }
            let fingerprint = Fingerprint::read(&metadata);
            let cached = self
                .cache
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .files[position]
                .clone();
            let cached = cached.filter(|entry| Some(&entry.fingerprint) == fingerprint.as_ref());
            if let Some(cached) = cached {
                if cached.binary {
                    result.binary_files += 1;
                    result.skipped_files += 1;
                    result.cached_files += 1;
                    continue;
                }
                if !needle.possible_in(&cached.bloom) {
                    result.cached_files += 1;
                    continue;
                }
                if let Some(text) = cached.text {
                    result.cached_files += 1;
                    search_text(path, &text, needle, state, &mut result.matches);
                    continue;
                }
            }
            let reservation = metadata.len().saturating_add(1);
            if state
                .read_budget
                .fetch_add(reservation, Ordering::Relaxed)
                .saturating_add(reservation)
                > state.max_read_bytes
            {
                state.stop.store(true, Ordering::Relaxed);
                break;
            }
            let Some(bytes) = read_file(
                &absolute,
                &self.root,
                path,
                &metadata,
                self.limits.max_file_bytes,
                state,
                &mut result.read_bytes,
                &mut directories,
            ) else {
                result.skipped_files += 1;
                continue;
            };
            let text = decode_text(&bytes);
            let cached = fingerprint.map(|fingerprint| CachedFile {
                fingerprint,
                bloom: text
                    .as_ref()
                    .map_or([0; BLOOM_WORDS], |text| bloom(text.as_bytes())),
                text: text
                    .as_ref()
                    .filter(|text| text.len() <= self.limits.max_cached_file_bytes)
                    .map(|text| Arc::from(text.as_str())),
                binary: text.is_none(),
            });
            if let Some(cached) = cached {
                self.cache
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .put(position, cached, self.limits.max_cache_bytes);
            }
            if let Some(text) = text {
                search_text(path, &text, needle, state, &mut result.matches);
            } else {
                result.binary_files += 1;
                result.skipped_files += 1;
            }
        }
        result
    }
}

struct QueryState<'a> {
    next: AtomicUsize,
    stop: AtomicBool,
    timed_out: AtomicBool,
    read_budget: AtomicU64,
    match_count: AtomicUsize,
    cancel: &'a AtomicBool,
    deadline: Instant,
    limit: usize,
    max_read_bytes: u64,
}

impl QueryState<'_> {
    fn interrupted(&self) -> bool {
        if self.cancel.load(Ordering::Relaxed) || self.stop.load(Ordering::Relaxed) {
            return true;
        }
        if Instant::now() >= self.deadline {
            self.timed_out.store(true, Ordering::Relaxed);
            self.stop.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), ContentSearchError> {
    if cancel.load(Ordering::Relaxed) {
        Err(ContentSearchError::Cancelled)
    } else {
        Ok(())
    }
}

fn usable_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 16 * 1024
        && !path.contains(['\0', '\\', ':'])
        && path.split('/').all(|part| {
            !part.is_empty() && part != "." && part != ".." && !part.eq_ignore_ascii_case(".git")
        })
}

fn module_key(path: &str) -> Cow<'_, str> {
    let path = path.trim_end_matches('/');
    #[cfg(windows)]
    {
        Cow::Owned(path.to_ascii_lowercase())
    }
    #[cfg(not(windows))]
    {
        Cow::Borrowed(path)
    }
}

fn module_ancestor(path: &str, modules: &[Cow<'_, str>]) -> bool {
    let path = module_key(path);
    let contains = |path: &str| {
        modules
            .binary_search_by(|module| module.as_ref().cmp(path))
            .is_ok()
    };
    contains(&path)
        || path
            .match_indices('/')
            .any(|(position, _)| contains(&path[..position]))
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub(crate) fn checked_root(root: &Path) -> Result<PathBuf, ContentSearchError> {
    let root = native_path(root);
    if !root.is_absolute() {
        return Err(ContentSearchError::InvalidRoot(
            "仓库路径必须是绝对路径。".into(),
        ));
    }
    let mut cursor = PathBuf::new();
    for component in root.components() {
        if matches!(component, Component::ParentDir | Component::CurDir) {
            return Err(ContentSearchError::InvalidRoot(
                "仓库路径包含相对跳转。".into(),
            ));
        }
        cursor.push(component.as_os_str());
        // A Windows drive prefix alone is not a filesystem location yet.
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let metadata = fs::symlink_metadata(&cursor)
            .map_err(|error| ContentSearchError::InvalidRoot(error.to_string()))?;
        if is_link(&metadata) || !metadata.is_dir() {
            return Err(ContentSearchError::InvalidRoot(
                "仓库路径包含链接或不是普通目录。".into(),
            ));
        }
    }
    fs::canonicalize(root).map_err(|error| ContentSearchError::InvalidRoot(error.to_string()))
}

#[derive(Default)]
struct DirectoryChecks(HashMap<PathBuf, SystemTime>);

impl DirectoryChecks {
    fn plain(&mut self, path: &Path, metadata: &Metadata) -> bool {
        // Still inspect every directory for links on every file access. Only
        // repository-marker probes are reused, while its directory timestamp
        // is unchanged. Creating/renaming a .git marker changes that timestamp.
        let modified = metadata.modified().ok();
        if modified.is_some_and(|stamp| self.0.get(path) == Some(&stamp)) {
            return true;
        }
        if fs::symlink_metadata(path.join(".git")).is_ok()
            || (fs::symlink_metadata(path.join("HEAD")).is_ok()
                && fs::symlink_metadata(path.join("objects")).is_ok()
                && fs::symlink_metadata(path.join("refs")).is_ok())
        {
            self.0.remove(path);
            return false;
        }
        if let Some(modified) = modified {
            if self.0.len() >= 256 {
                self.0.clear();
            }
            self.0.insert(path.to_owned(), modified);
        }
        true
    }
}

fn checked_file(
    root: &Path,
    relative: &str,
    directories: &mut DirectoryChecks,
) -> Option<(PathBuf, Metadata)> {
    let mut path = root.to_owned();
    let parts: Vec<_> = relative.split('/').collect();
    for (position, part) in parts.iter().enumerate() {
        path.push(part);
        let metadata = fs::symlink_metadata(&path).ok()?;
        if is_link(&metadata) {
            return None;
        }
        if position + 1 == parts.len() {
            return metadata.is_file().then_some((path, metadata));
        }
        if !metadata.is_dir() {
            return None;
        }
        if !directories.plain(&path, &metadata) {
            return None;
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn read_file(
    path: &Path,
    root: &Path,
    relative: &str,
    before: &Metadata,
    max_bytes: usize,
    state: &QueryState<'_>,
    read_bytes: &mut u64,
    directories: &mut DirectoryChecks,
) -> Option<Vec<u8>> {
    let mut options = File::options();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Do not dereference a final component swapped after the metadata check.
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path).ok()?;
    let opened = file.metadata().ok()?;
    if is_link(&opened)
        || !opened.is_file()
        || opened.len() != before.len()
        || Fingerprint::read(&opened) != Fingerprint::read(before)
    {
        return None;
    }
    // A file that grows after metadata must not exceed its reserved IO budget.
    let mut file = file.take(opened.len().saturating_add(1));
    let mut bytes = Vec::with_capacity((opened.len() as usize).min(max_bytes));
    let mut chunk = [0; READ_CHUNK];
    loop {
        if state.interrupted() {
            return None;
        }
        let count = file.read(&mut chunk).ok()?;
        *read_bytes += count as u64;
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count) > max_bytes {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let (_, after) = checked_file(root, relative, directories)?;
    if Fingerprint::read(&opened) != Fingerprint::read(&after) || after.len() != bytes.len() as u64
    {
        return None;
    }
    Some(bytes)
}

fn decode_text(bytes: &[u8]) -> Option<String> {
    let text = if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let big_endian = bytes[0] == 0xfe;
        let (pairs, tail) = bytes[2..].as_chunks::<2>();
        if !tail.is_empty() {
            return None;
        }
        let units = pairs.iter().map(|pair| {
            if big_endian {
                u16::from_be_bytes(*pair)
            } else {
                u16::from_le_bytes(*pair)
            }
        });
        char::decode_utf16(units)
            .collect::<Result<String, _>>()
            .ok()?
    } else {
        std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
            .ok()?
            .to_owned()
    };
    (!text.chars().any(|character| {
        character.is_control() && !matches!(character, '\n' | '\r' | '\t' | '\u{c}')
    }))
    .then_some(text)
}

struct Needle {
    bytes: Vec<u8>,
    prefix: Vec<usize>,
    trigrams: Vec<(usize, u64)>,
    case_sensitive: bool,
    regex: Option<regex::Regex>,
}

impl Needle {
    fn new(query: &str) -> Self {
        Self::with_options(query, false, None)
    }

    fn with_options(query: &str, case_sensitive: bool, regex: Option<regex::Regex>) -> Self {
        let bytes: Vec<_> = query
            .bytes()
            .map(|byte| {
                if case_sensitive {
                    byte
                } else {
                    byte.to_ascii_lowercase()
                }
            })
            .collect();
        let mut prefix = vec![0; bytes.len()];
        for position in 1..bytes.len() {
            let mut matched = prefix[position - 1];
            while matched > 0 && bytes[matched] != bytes[position] {
                matched = prefix[matched - 1];
            }
            if bytes[matched] == bytes[position] {
                matched += 1;
            }
            prefix[position] = matched;
        }
        let trigrams = bytes.windows(3).map(trigram_bit).collect();
        Self {
            bytes,
            prefix,
            trigrams,
            case_sensitive,
            regex,
        }
    }

    fn possible_in(&self, bloom: &[u64; BLOOM_WORDS]) -> bool {
        if self.regex.is_some() {
            return true;
        }
        self.trigrams
            .iter()
            .all(|&(word, mask)| bloom[word] & mask == mask)
    }
}

fn trigram_bit(bytes: &[u8]) -> (usize, u64) {
    let value = u32::from(bytes[0].to_ascii_lowercase())
        | (u32::from(bytes[1].to_ascii_lowercase()) << 8)
        | (u32::from(bytes[2].to_ascii_lowercase()) << 16);
    let hash = value.wrapping_mul(2_654_435_761);
    let bit = (hash >> 22) as usize;
    (bit / 64, 1 << (bit % 64))
}

fn bloom(bytes: &[u8]) -> [u64; BLOOM_WORDS] {
    let mut bloom = [0; BLOOM_WORDS];
    for trigram in bytes.windows(3) {
        let (word, mask) = trigram_bit(trigram);
        bloom[word] |= mask;
    }
    bloom
}

fn search_text(
    path: &Arc<str>,
    text: &str,
    needle: &Needle,
    state: &QueryState<'_>,
    output: &mut Vec<ContentSearchMatch>,
) {
    if let Some(regex) = &needle.regex {
        return search_regex_text(path, text, regex, state, output);
    }
    let mut matched = 0;
    let mut line_number = 1;
    let mut line_start = 0;
    let mut line_ranges = Vec::new();
    for (position, &byte) in text.as_bytes().iter().enumerate() {
        if position % 4096 == 0 && state.interrupted() {
            return;
        }
        if byte == b'\n' {
            if !line_ranges.is_empty() {
                push_line(
                    path,
                    text,
                    line_start,
                    position,
                    line_number,
                    &line_ranges,
                    state,
                    output,
                );
                line_ranges.clear();
                if state.interrupted() {
                    return;
                }
            }
            line_number += 1;
            line_start = position + 1;
            matched = 0;
            continue;
        }
        let folded = if needle.case_sensitive {
            byte
        } else {
            byte.to_ascii_lowercase()
        };
        while matched > 0 && folded != needle.bytes[matched] {
            matched = needle.prefix[matched - 1];
        }
        if folded == needle.bytes[matched] {
            matched += 1;
        }
        if matched == needle.bytes.len() {
            if line_ranges.len() < MAX_HIGHLIGHTS {
                let range = position + 1 - matched - line_start..position + 1 - line_start;
                // Merge overlaps so GPUI text runs never overlap.
                if let Some(previous) = line_ranges
                    .last_mut()
                    .filter(|previous: &&mut Range<usize>| previous.end >= range.start)
                {
                    previous.end = range.end;
                } else {
                    line_ranges.push(range);
                }
            }
            matched = needle.prefix[matched - 1];
        }
    }
    if !line_ranges.is_empty() && !state.interrupted() {
        push_line(
            path,
            text,
            line_start,
            text.len(),
            line_number,
            &line_ranges,
            state,
            output,
        );
    }
}

fn search_regex_text(
    path: &Arc<str>,
    text: &str,
    regex: &regex::Regex,
    state: &QueryState<'_>,
    output: &mut Vec<ContentSearchMatch>,
) {
    let mut offset = 0;
    for (number, line) in text.split_terminator('\n').enumerate() {
        if state.interrupted() {
            return;
        }
        let line = line.trim_end_matches('\r');
        let ranges: Vec<_> = regex
            .find_iter(line)
            .take(MAX_HIGHLIGHTS)
            .map(|hit| hit.range())
            .collect();
        if !ranges.is_empty() {
            push_line(
                path,
                text,
                offset,
                offset + line.len(),
                number + 1,
                &ranges,
                state,
                output,
            );
        }
        // Account for the original CRLF length, independently of trimming.
        offset = text[offset..]
            .find('\n')
            .map_or(text.len(), |end| offset + end + 1);
    }
}

#[allow(clippy::too_many_arguments)]
fn push_line(
    path: &Arc<str>,
    text: &str,
    start: usize,
    end: usize,
    line_number: usize,
    ranges: &[Range<usize>],
    state: &QueryState<'_>,
    output: &mut Vec<ContentSearchMatch>,
) {
    let count = state.match_count.fetch_add(1, Ordering::Relaxed);
    if count >= state.limit {
        state.stop.store(true, Ordering::Relaxed);
        return;
    }
    let line = text[start..end].trim_end_matches('\r');
    let first = &ranges[0];
    let snippet_start = line[..first.start]
        .char_indices()
        .rev()
        .nth(80)
        .map_or(0, |(position, _)| position);
    let snippet_end = line[first.end..]
        .char_indices()
        .nth(160)
        .map_or(line.len(), |(position, _)| first.end + position);
    let prefix = if snippet_start > 0 { "…" } else { "" };
    let suffix = if snippet_end < line.len() { "…" } else { "" };
    let snippet = format!("{prefix}{}{suffix}", &line[snippet_start..snippet_end]);
    let highlights = ranges
        .iter()
        .filter_map(|range| {
            let start = range.start.max(snippet_start);
            let end = range.end.min(snippet_end);
            (start < end)
                .then_some(start - snippet_start + prefix.len()..end - snippet_start + prefix.len())
        })
        .collect();
    output.push(ContentSearchMatch {
        path: path.clone(),
        line_number,
        snippet: Arc::from(snippet),
        highlights,
    });
    if count + 1 >= state.limit {
        state.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitviews_git::{FileTreeEntry, SubmoduleInfo};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

    struct Fixture {
        root: PathBuf,
        tree: RepositoryTree,
    }

    impl Fixture {
        fn new() -> Self {
            let target =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/content-search-tests");
            fs::create_dir_all(&target).unwrap();
            let root = target.join(format!(
                "{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self {
                root: fs::canonicalize(root).unwrap(),
                tree: RepositoryTree::default(),
            }
        }

        fn file(&mut self, path: &str, bytes: &[u8]) {
            let absolute = self.root.join(path);
            fs::create_dir_all(absolute.parent().unwrap()).unwrap();
            fs::write(absolute, bytes).unwrap();
            self.tree.entries.push(FileTreeEntry {
                path: path.into(),
                kind: FileTreeKind::File,
                index_status: ' ',
                worktree_status: ' ',
            });
        }

        fn index(&self) -> ContentSearchIndex {
            ContentSearchIndex::from_tree(&self.root, &self.tree).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let target = fs::canonicalize(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/content-search-tests"),
            )
            .unwrap();
            assert!(self.root.starts_with(&target) && self.root != target);
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    fn query(index: &ContentSearchIndex, query: &str) -> ContentSearchResults {
        index.query(query, 200, &AtomicBool::new(false)).unwrap()
    }

    #[test]
    fn literal_search_preserves_chinese_case_crlf_lines_and_highlight_offsets() {
        let mut fixture = Fixture::new();
        fixture.file(
            "目录/中文.rs",
            "empty\r\n前缀Hello中文 HELLO中文\r\n[a-z]+ is literal\n".as_bytes(),
        );
        let index = fixture.index();
        let result = query(&index, "hello中文");
        assert_eq!(result.matches.len(), 1);
        let found = &result.matches[0];
        assert_eq!(found.path.as_ref(), "目录/中文.rs");
        assert_eq!(found.line_number, 2);
        assert_eq!(
            found
                .highlights
                .iter()
                .map(|range| &found.snippet[range.clone()])
                .collect::<Vec<_>>(),
            ["Hello中文", "HELLO中文"]
        );
        assert!(!found.snippet.ends_with('\r'));
        assert_eq!(query(&index, "[a-z]+").matches[0].line_number, 3);
        assert!(query(&index, "h.*中文").matches.is_empty());
    }

    #[test]
    fn long_lines_have_bounded_context_and_original_utf8_ranges() {
        let mut fixture = Fixture::new();
        fixture.file(
            "long.txt",
            format!("{}中文Target{}", "前".repeat(10_000), "后".repeat(10_000)).as_bytes(),
        );
        let result = query(&fixture.index(), "中文target");
        let found = &result.matches[0];
        assert!(found.snippet.starts_with('…') && found.snippet.ends_with('…'));
        assert!(found.snippet.chars().count() < 260);
        assert_eq!(&found.snippet[found.highlights[0].clone()], "中文Target");
    }

    #[test]
    fn decodes_editor_compatible_utf16_and_skips_binary_and_oversize_files() {
        let mut fixture = Fixture::new();
        let mut utf16 = vec![0xff, 0xfe];
        utf16.extend("中文 NeedLE\r\n".encode_utf16().flat_map(u16::to_le_bytes));
        fixture.file("utf16.txt", &utf16);
        fixture.file("binary.dat", b"needle\0hidden");
        fixture.file("oversize.txt", &[b'x'; 101]);
        let index = ContentSearchIndex::with_limits(
            &fixture.root,
            &fixture.tree,
            ContentSearchLimits {
                max_file_bytes: 100,
                ..Default::default()
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        let result = query(&index, "needle");
        assert_eq!(result.matches.len(), 1);
        assert_eq!(
            &result.matches[0].snippet[result.matches[0].highlights[0].clone()],
            "NeedLE"
        );
        assert_eq!(result.binary_files, 1);
        assert_eq!(result.oversized_files, 1);
        let warm = query(&index, "needle");
        assert_eq!(warm.read_bytes, 0);
        assert_eq!(warm.cached_files, 2);
    }

    #[test]
    fn searches_only_tree_candidates_without_entering_submodules_or_other_repositories() {
        let mut fixture = Fixture::new();
        fixture.file("allowed.txt", b"needle");
        fixture.file("nested/hidden.txt", b"needle");
        fs::write(fixture.root.join("nested/.git"), b"gitdir: anywhere").unwrap();
        fixture.file("module/hidden.txt", b"needle");
        fixture.file("deleted.txt", b"needle");
        fixture.tree.entries.last_mut().unwrap().worktree_status = 'D';
        fixture.file(".git/config", b"needle");
        fs::write(fixture.root.join("not-in-tree.txt"), b"needle").unwrap();
        fixture.tree.submodules.push(SubmoduleInfo {
            name: "module".into(),
            path: "module".into(),
            url: None,
            head_id: None,
            index_id: None,
            workdir_id: None,
            initialized: false,
            dirty: false,
        });
        let result = query(&fixture.index(), "needle");
        assert_eq!(
            result
                .matches
                .iter()
                .map(|item| item.path.as_ref())
                .collect::<Vec<_>>(),
            ["allowed.txt"]
        );
        assert_eq!(result.skipped_files, 1);
        assert_eq!(result.warning_count, 1);
    }

    #[test]
    fn directory_marker_cache_rechecks_new_nested_repositories() {
        let mut fixture = Fixture::new();
        fixture.file("nested/plain.txt", b"needle");
        let mut directories = DirectoryChecks::default();
        assert!(checked_file(&fixture.root, "nested/plain.txt", &mut directories).is_some());
        fs::write(
            fixture.root.join("nested/.git"),
            b"gitdir: another-repository",
        )
        .unwrap();
        assert!(checked_file(&fixture.root, "nested/plain.txt", &mut directories).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn never_reads_symlinked_files_or_parent_directories() {
        use std::os::unix::fs::symlink;
        let mut fixture = Fixture::new();
        fixture.file("safe/real.txt", b"private needle");
        symlink(
            fixture.root.join("safe/real.txt"),
            fixture.root.join("linked.txt"),
        )
        .unwrap();
        symlink(
            fixture.root.join("safe"),
            fixture.root.join("directory-link"),
        )
        .unwrap();
        fixture.tree.entries = ["linked.txt", "directory-link/real.txt"]
            .into_iter()
            .map(|path| FileTreeEntry {
                path: path.into(),
                kind: FileTreeKind::File,
                index_status: ' ',
                worktree_status: ' ',
            })
            .collect();
        let result = query(&fixture.index(), "needle");
        assert!(result.matches.is_empty());
        assert_eq!(result.read_bytes, 0);
        assert_eq!(result.skipped_files, 2);
    }

    #[test]
    fn refreshed_files_replace_cached_text_and_removed_files_do_not_leave_stale_matches() {
        let mut fixture = Fixture::new();
        fixture.file("changing.txt", b"old needle");
        let index = fixture.index();
        assert_eq!(query(&index, "needle").matches.len(), 1);
        let warm = query(&index, "needle");
        assert_eq!(warm.read_bytes, 0);
        assert_eq!(warm.cached_files, 1);
        fs::write(
            fixture.root.join("changing.txt"),
            b"new replacement without match",
        )
        .unwrap();
        assert!(query(&index, "needle").matches.is_empty());
        fs::remove_file(fixture.root.join("changing.txt")).unwrap();
        let removed = query(&index, "replacement");
        assert!(removed.matches.is_empty());
        assert_eq!(removed.skipped_files, 1);
    }

    #[test]
    fn trigram_prefilter_reuses_summary_after_text_cache_eviction_without_false_negatives() {
        let mut fixture = Fixture::new();
        fixture.file("a.txt", "中文 BeginNeedle 末尾".as_bytes());
        let index = ContentSearchIndex::with_limits(
            &fixture.root,
            &fixture.tree,
            ContentSearchLimits {
                max_cache_bytes: 0,
                ..Default::default()
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(query(&index, "beginneedle").matches.len(), 1);
        let absent = query(&index, "absolutely-unseen");
        assert_eq!(absent.read_bytes, 0);
        assert_eq!(absent.cached_files, 1);
        // Positive Bloom summaries only admit a candidate; the real matcher
        // must still read the uncached contents and validate the exact query.
        let present = query(&index, "中文");
        assert_eq!(present.matches.len(), 1);
        assert!(present.read_bytes > 0);
    }

    #[test]
    fn bounds_result_count_io_and_query_input_and_honors_cancellation() {
        let mut fixture = Fixture::new();
        fixture.file("many.txt", "needle\n".repeat(100).as_bytes());
        let index = fixture.index();
        let limited = index.query("needle", 3, &AtomicBool::new(false)).unwrap();
        assert_eq!(limited.matches.len(), 3);
        assert!(limited.truncated);
        assert!(matches!(
            index.query("needle", 3, &AtomicBool::new(true)),
            Err(ContentSearchError::Cancelled)
        ));
        assert!(matches!(
            index.query(&"中".repeat(257), 3, &AtomicBool::new(false)),
            Err(ContentSearchError::QueryTooLong)
        ));
        assert!(matches!(
            index.query("first\nsecond", 3, &AtomicBool::new(false)),
            Err(ContentSearchError::InvalidQuery)
        ));
        assert_eq!(query(&index, "").read_bytes, 0);
        let budget = ContentSearchIndex::with_limits(
            &fixture.root,
            &fixture.tree,
            ContentSearchLimits {
                max_read_bytes: 1,
                ..Default::default()
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        let result = query(&budget, "needle");
        assert!(result.truncated && result.matches.is_empty());
        assert_eq!(result.read_bytes, 0);
    }

    #[test]
    fn warm_large_fixture_performs_zero_content_io_and_metadata_memory_stays_bounded() {
        let mut fixture = Fixture::new();
        let text = "fn ordinary_function() { println!(\"中文 value\"); }\n".repeat(100);
        for number in 0..200 {
            fixture.file(&format!("src/file_{number:04}.rs"), text.as_bytes());
        }
        let index = fixture.index();
        let cold = query(&index, "absent-cold-query");
        assert_eq!(cold.scanned_files, 200);
        assert!(cold.read_bytes > 800_000 && !cold.truncated);
        let warm = query(&index, "different-absent-query");
        eprintln!(
            "content-search fixture: {} files, cold {:?} / {} content bytes, warm {:?} / {} content bytes / {} cache hits",
            cold.scanned_files,
            cold.elapsed,
            cold.read_bytes,
            warm.elapsed,
            warm.read_bytes,
            warm.cached_files
        );
        assert_eq!(warm.read_bytes, 0);
        assert_eq!(warm.cached_files, 200);
        assert!(warm.matches.is_empty() && !warm.truncated);
        assert!(index.cache.lock().unwrap().text_bytes <= index.limits.max_cache_bytes);
        assert!(matches!(
            ContentSearchIndex::with_limits(
                &fixture.root,
                &fixture.tree,
                ContentSearchLimits {
                    max_index_bytes: 100,
                    ..Default::default()
                },
                &AtomicBool::new(false)
            ),
            Err(ContentSearchError::IndexLimit)
        ));
    }
}
