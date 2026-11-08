//! Immutable, bounded quick-open metadata. Build on a worker when a tree changes;
//! reuse an `Arc<SearchIndex>` for queries and discard it when that tree expires.
//! This module never reads files, runs Git, or traverses a submodule.

use std::{
    cmp::Ordering,
    collections::BinaryHeap,
    error::Error,
    fmt,
    mem::size_of,
    ops::Range,
    sync::{Arc, atomic::AtomicBool, atomic::Ordering as AtomicOrdering},
    time::{Duration, Instant},
};

use crate::search_options::{CompiledSearchOptions, SearchOptions};
use gitviews_git::{FileTreeKind, RepositoryTree};

const MAX_QUERY_CHARS: usize = 256;
const MAX_RESULTS: usize = 200;
const QUERY_BUDGET: Duration = Duration::from_millis(250);

/// A per-index metadata budget, independent of the preview cache or total RSS.
#[derive(Clone, Copy, Debug)]
pub struct SearchIndexLimits {
    pub max_files: usize,
    pub max_bytes: usize,
    pub max_path_bytes: usize,
}

impl Default for SearchIndexLimits {
    fn default() -> Self {
        Self {
            max_files: 100_000,
            max_bytes: 32 * 1024 * 1024,
            max_path_bytes: 16 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchError {
    Cancelled,
    IndexLimit { reason: &'static str, limit: usize },
    QueryTooLong,
    InvalidLimit,
    InvalidOptions(String),
}

impl fmt::Display for SearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("文件搜索已取消。"),
            Self::IndexLimit { reason, limit } => {
                write!(
                    formatter,
                    "文件索引达到{reason}上限（{limit}）；请缩小范围后重试。"
                )
            }
            Self::QueryTooLong => formatter.write_str("文件查询不能超过 256 个字符或 1024 字节。"),
            Self::InvalidLimit => formatter.write_str("文件索引预算和搜索结果上限必须大于零。"),
            Self::InvalidOptions(message) => formatter.write_str(message),
        }
    }
}

impl Error for SearchError {}

/// A real repository-relative path, suitable for the normal guarded source API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileSearchMatch {
    pub path: Arc<str>,
    /// Higher is better. The returned list includes deterministic tie breakers.
    pub score: i64,
    /// Original-path UTF-8 byte ranges, including regex and fuzzy matches.
    pub highlights: Vec<Range<usize>>,
}

#[derive(Clone, Debug, Default)]
pub struct SearchResults {
    pub matches: Vec<FileSearchMatch>,
    /// Number matched among visited paths; a lower bound when `timed_out`.
    pub total_matches: usize,
    pub truncated: bool,
    pub indexed_files: usize,
    /// Tree warnings plus unusable metadata paths skipped while indexing.
    pub warning_count: usize,
    pub partial: bool,
    pub timed_out: bool,
}

#[derive(Debug)]
struct IndexedPath {
    path: Arc<str>,
    folded: Box<[char]>,
    basename: usize,
    basename_byte: usize,
    depth: usize,
    character_mask: u64,
}

/// Store one index for a tree generation. Construct a replacement before
/// publishing it; cancelled/failed builds cannot mutate the previous index.
#[derive(Debug)]
pub struct SearchIndex {
    entries: Vec<IndexedPath>,
    bytes: usize,
    warning_count: usize,
}

impl SearchIndex {
    pub fn from_tree(tree: &RepositoryTree) -> Result<Self, SearchError> {
        Self::with_limits(tree, SearchIndexLimits::default(), &AtomicBool::new(false))
    }

    pub fn with_limits(
        tree: &RepositoryTree,
        limits: SearchIndexLimits,
        cancel: &AtomicBool,
    ) -> Result<Self, SearchError> {
        if limits.max_files == 0 || limits.max_bytes == 0 || limits.max_path_bytes == 0 {
            return Err(SearchError::InvalidLimit);
        }
        check_cancel(cancel)?;
        // Borrow boundaries instead of cloning their paths. Bound this temporary
        // allocation too, including malicious/partial trees supplied by a backend.
        let module_count = tree.submodules.len().saturating_add(
            tree.entries
                .iter()
                .filter(|entry| entry.kind == FileTreeKind::Submodule)
                .count(),
        );
        let scratch = module_count.saturating_mul(size_of::<&str>());
        if scratch > limits.max_bytes {
            return Err(byte_limit(limits.max_bytes));
        }
        let mut modules = Vec::with_capacity(module_count);
        for module in &tree.submodules {
            check_cancel(cancel)?;
            modules.push(module.path.trim_end_matches('/'));
        }
        for entry in &tree.entries {
            check_cancel(cancel)?;
            if entry.kind == FileTreeKind::Submodule {
                modules.push(entry.path.trim_end_matches('/'));
            }
        }
        modules.sort_unstable();
        modules.dedup();
        let capacity = tree.entries.len().min(limits.max_files);
        let mut bytes =
            size_of::<Self>().saturating_add(capacity.saturating_mul(size_of::<IndexedPath>()));
        if bytes.saturating_add(scratch) > limits.max_bytes {
            return Err(byte_limit(limits.max_bytes));
        }
        let mut entries = Vec::with_capacity(capacity);
        let mut warning_count = tree.warnings.len();
        for entry in &tree.entries {
            check_cancel(cancel)?;
            if !matches!(entry.kind, FileTreeKind::File | FileTreeKind::Symlink)
                || entry.worktree_status == 'D'
                || (entry.index_status == 'D' && entry.worktree_status == ' ')
            {
                continue;
            }
            let path = entry.path.as_str();
            if path
                .split('/')
                .any(|part| part.eq_ignore_ascii_case(".git"))
                || module_ancestor(path, &modules)
            {
                continue;
            }
            if path.len() > limits.max_path_bytes {
                return Err(SearchError::IndexLimit {
                    reason: "单路径字节数",
                    limit: limits.max_path_bytes,
                });
            }
            if !usable_path(path) {
                warning_count = warning_count.saturating_add(1);
                continue;
            }
            if entries.len() == limits.max_files {
                return Err(SearchError::IndexLimit {
                    reason: "文件数量",
                    limit: limits.max_files,
                });
            }
            // Count before cloning/normalizing: never allocate an unbounded
            // temporary path and truncate it after the fact.
            let characters = path.chars().count();
            let cost = path
                .len()
                .saturating_add(2 * size_of::<usize>())
                .saturating_add(characters.saturating_mul(size_of::<char>()));
            bytes = bytes.saturating_add(cost);
            if bytes.saturating_add(scratch) > limits.max_bytes {
                return Err(byte_limit(limits.max_bytes));
            }
            let folded: Box<[char]> = path.chars().map(fold).collect();
            let basename = folded
                .iter()
                .rposition(|&character| character == '/')
                .map_or(0, |position| position + 1);
            let basename_byte = path.rfind('/').map_or(0, |position| position + 1);
            let character_mask = folded
                .iter()
                .fold(0, |mask, &character| mask | character_bit(character));
            let depth = path.bytes().filter(|&byte| byte == b'/').count();
            entries.push(IndexedPath {
                path: Arc::from(path),
                folded,
                basename,
                basename_byte,
                depth,
                character_mask,
            });
        }
        entries.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        // Native trees are unique; also handle a partially merged custom backend
        // deterministically without showing a file twice.
        entries.dedup_by(|left, right| left.path == right.path);
        check_cancel(cancel)?;
        Ok(Self {
            entries,
            bytes,
            warning_count,
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// Conservative retained metadata charge. Original tree storage is separate.
    pub fn estimated_bytes(&self) -> usize {
        self.bytes
    }

    /// Only scans cached metadata; call on a worker after the UI's debounce.
    /// ASCII case folds and Windows separators normalize without altering the
    /// returned original path. Chinese and other Unicode scalars stay intact.
    pub fn query(
        &self,
        query: &str,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<SearchResults, SearchError> {
        self.query_until(query, limit, cancel, Instant::now() + QUERY_BUDGET)
    }

    pub fn query_with_options(
        &self,
        query: &str,
        limit: usize,
        cancel: &AtomicBool,
        options: &SearchOptions,
    ) -> Result<SearchResults, SearchError> {
        self.query_until_options(query, limit, cancel, Instant::now() + QUERY_BUDGET, options)
    }

    fn query_until(
        &self,
        query: &str,
        limit: usize,
        cancel: &AtomicBool,
        deadline: Instant,
    ) -> Result<SearchResults, SearchError> {
        self.query_until_options(query, limit, cancel, deadline, &SearchOptions::default())
    }

    pub(crate) fn query_until_options(
        &self,
        query: &str,
        limit: usize,
        cancel: &AtomicBool,
        deadline: Instant,
        options: &SearchOptions,
    ) -> Result<SearchResults, SearchError> {
        check_cancel(cancel)?;
        if limit == 0 {
            return Err(SearchError::InvalidLimit);
        }
        if query.len() > 1024 || query.chars().take(MAX_QUERY_CHARS + 1).count() > MAX_QUERY_CHARS {
            return Err(SearchError::QueryTooLong);
        }
        let filters = CompiledSearchOptions::new(options, query)
            .map_err(|error| SearchError::InvalidOptions(error.0))?;
        let original_query = if options.regex {
            query.to_owned()
        } else {
            query.replace('\\', "/")
        };
        let query: Vec<_> = original_query
            .chars()
            .map(|value| {
                if options.case_sensitive {
                    value
                } else {
                    fold(value)
                }
            })
            .collect();
        let mask = query
            .iter()
            .fold(0, |mask, &character| mask | character_bit(character));
        let prefix = prefix_table(&query);
        let mut context = QueryContext {
            cancel,
            deadline,
            timed_out: false,
        };
        let mut result = SearchResults {
            indexed_files: self.len(),
            warning_count: self.warning_count,
            partial: self.warning_count > 0,
            ..Default::default()
        };
        let limit = limit.min(MAX_RESULTS);
        let mut best = BinaryHeap::<RankedMatch>::with_capacity(limit);
        for entry in &self.entries {
            if context.interrupted()? {
                break;
            }
            if !filters.allows(&entry.path) {
                continue;
            }
            let rank = if let Some(regex) = &filters.regex {
                regex.find(&entry.path).map(|hit| Rank {
                    class: if hit.start() >= entry.basename_byte {
                        3
                    } else {
                        4
                    },
                    gap: 0,
                    boundary: hit.start(),
                    depth: entry.depth,
                    length: entry.path.len(),
                })
            } else {
                if !options.case_sensitive && entry.character_mask & mask != mask {
                    continue;
                }
                match_rank(
                    entry,
                    &original_query,
                    &query,
                    &prefix,
                    &mut context,
                    options.case_sensitive,
                )?
            };
            if context.timed_out {
                break;
            }
            let Some(rank) = rank else {
                continue;
            };
            result.total_matches += 1;
            let candidate = RankedMatch {
                path: entry.path.clone(),
                rank,
            };
            if best.len() < limit {
                best.push(candidate);
            } else if best.peek().is_some_and(|worst| candidate < *worst) {
                best.pop();
                best.push(candidate);
            }
        }
        check_cancel(cancel)?;
        result.matches = best
            .into_sorted_vec()
            .into_iter()
            .map(|item| FileSearchMatch {
                highlights: filters.path_highlights(&item.path, &original_query),
                path: item.path,
                score: item.rank.score(),
            })
            .collect();
        result.timed_out = context.timed_out;
        result.partial |= context.timed_out;
        result.truncated = result.total_matches > result.matches.len() || context.timed_out;
        Ok(result)
    }
}

fn usable_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\0', '\\', ':'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn module_ancestor(path: &str, modules: &[&str]) -> bool {
    modules.binary_search(&path).is_ok()
        || path
            .match_indices('/')
            .any(|(position, _)| modules.binary_search(&&path[..position]).is_ok())
}

fn byte_limit(limit: usize) -> SearchError {
    SearchError::IndexLimit {
        reason: "元数据字节数",
        limit,
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), SearchError> {
    if cancel.load(AtomicOrdering::Relaxed) {
        Err(SearchError::Cancelled)
    } else {
        Ok(())
    }
}

fn fold(character: char) -> char {
    character.to_ascii_lowercase()
}
fn character_bit(character: char) -> u64 {
    1 << ((character as u32).wrapping_mul(2_654_435_761) >> 26)
}

struct QueryContext<'a> {
    cancel: &'a AtomicBool,
    deadline: Instant,
    timed_out: bool,
}
impl QueryContext<'_> {
    fn interrupted(&mut self) -> Result<bool, SearchError> {
        check_cancel(self.cancel)?;
        self.timed_out |= Instant::now() >= self.deadline;
        Ok(self.timed_out)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Rank {
    class: u8,
    gap: usize,
    boundary: usize,
    depth: usize,
    length: usize,
}
impl Rank {
    fn score(self) -> i64 {
        1_000_000_000
            - i64::from(self.class) * 100_000_000
            - self.gap.min(100_000) as i64 * 100
            - self.boundary.min(100) as i64 * 10
            - self.depth.min(100) as i64
    }
}

#[derive(Debug, Eq, PartialEq)]
struct RankedMatch {
    path: Arc<str>,
    rank: Rank,
}
impl Ord for RankedMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        self.rank
            .cmp(&other.rank)
            .then_with(|| self.path.cmp(&other.path))
    }
}
impl PartialOrd for RankedMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn match_rank(
    entry: &IndexedPath,
    original_query: &str,
    query: &[char],
    prefix: &[usize],
    context: &mut QueryContext<'_>,
    case_sensitive: bool,
) -> Result<Option<Rank>, SearchError> {
    if query.is_empty() {
        return Ok(Some(Rank {
            class: 0,
            gap: 0,
            boundary: 0,
            depth: 0,
            length: 0,
        }));
    }
    let original;
    let folded: &[char] = if case_sensitive {
        original = entry.path.chars().collect::<Vec<_>>();
        &original
    } else {
        &entry.folded
    };
    let filename = &folded[entry.basename..];
    let rank = |class, gap, boundary| {
        Some(Rank {
            class,
            gap,
            boundary,
            depth: entry.depth,
            length: folded.len(),
        })
    };
    if filename == query {
        let exact = &entry.path[entry.basename_byte..] == original_query;
        return Ok(rank(if exact { 0 } else { 1 }, 0, 0));
    }
    if filename.starts_with(query) {
        return Ok(rank(2, 0, 0));
    }
    if let Some(position) = contains(filename, query, prefix, context)? {
        return Ok(rank(3, 0, usize::from(!boundary(filename, position))));
    }
    if let Some(position) = contains(folded, query, prefix, context)? {
        return Ok(rank(4, 0, usize::from(!boundary(folded, position))));
    }
    if let Some((gap, penalty)) = subsequence(filename, query, context)? {
        return Ok(rank(5, gap, penalty));
    }
    Ok(subsequence(folded, query, context)?.and_then(|(gap, penalty)| rank(6, gap, penalty)))
}

fn boundary(text: &[char], position: usize) -> bool {
    position == 0 || !text[position - 1].is_alphanumeric()
}

fn prefix_table(query: &[char]) -> Vec<usize> {
    let mut table = vec![0; query.len()];
    for position in 1..query.len() {
        let mut matched = table[position - 1];
        while matched > 0 && query[matched] != query[position] {
            matched = table[matched - 1];
        }
        if query[matched] == query[position] {
            matched += 1;
        }
        table[position] = matched;
    }
    table
}

fn contains(
    text: &[char],
    query: &[char],
    prefix: &[usize],
    context: &mut QueryContext<'_>,
) -> Result<Option<usize>, SearchError> {
    let mut matched = 0;
    for (position, &character) in text.iter().enumerate() {
        if position % 64 == 0 && context.interrupted()? {
            return Ok(None);
        }
        while matched > 0 && character != query[matched] {
            matched = prefix[matched - 1];
        }
        if character == query[matched] {
            matched += 1;
        }
        if matched == query.len() {
            return Ok(Some(position + 1 - matched));
        }
    }
    Ok(None)
}

fn subsequence(
    text: &[char],
    query: &[char],
    context: &mut QueryContext<'_>,
) -> Result<Option<(usize, usize)>, SearchError> {
    let mut matched = 0;
    let mut first = 0;
    let mut penalty = 0;
    for (position, &character) in text.iter().enumerate() {
        if position % 64 == 0 && context.interrupted()? {
            return Ok(None);
        }
        if character != query[matched] {
            continue;
        }
        if matched == 0 {
            first = position;
        }
        penalty += usize::from(!boundary(text, position));
        matched += 1;
        if matched == query.len() {
            return Ok(Some((position + 1 - first - matched, penalty)));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitviews_git::{FileTreeEntry, TreeWarning};

    fn tree(paths: &[&str]) -> RepositoryTree {
        RepositoryTree {
            entries: paths
                .iter()
                .map(|&path| FileTreeEntry {
                    path: path.into(),
                    kind: FileTreeKind::File,
                    index_status: ' ',
                    worktree_status: ' ',
                })
                .collect(),
            ..Default::default()
        }
    }
    fn paths(index: &SearchIndex, query: &str) -> Vec<Arc<str>> {
        index
            .query(query, 200, &AtomicBool::new(false))
            .unwrap()
            .matches
            .into_iter()
            .map(|item| item.path)
            .collect()
    }

    #[test]
    fn matches_clean_files_chinese_spaces_case_and_fuzzy_paths_without_file_io() {
        let index = SearchIndex::from_tree(&tree(&[
            "src/中文 文件.rs",
            "src/MainWindow.rs",
            "README.md",
            "docs/使用说明.md",
        ]))
        .unwrap();
        assert_eq!(paths(&index, "中文文")[0].as_ref(), "src/中文 文件.rs");
        assert_eq!(paths(&index, "mainwindow")[0].as_ref(), "src/MainWindow.rs");
        assert_eq!(paths(&index, "src\\main")[0].as_ref(), "src/MainWindow.rs");
        assert_eq!(paths(&index, "sMW")[0].as_ref(), "src/MainWindow.rs");
        assert!(paths(&index, "never-present").is_empty());
    }

    #[test]
    fn exact_basename_prefix_substring_path_and_fuzzy_have_deterministic_order() {
        let index = SearchIndex::from_tree(&tree(&[
            "z/main.rs",
            "a/Main.rs",
            "b/main.rs.bak",
            "c/xmain.rs",
            "main.rs/readme",
            "m/a/i/n.rs",
        ]))
        .unwrap();
        assert_eq!(
            paths(&index, "main.rs")
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>(),
            [
                "z/main.rs",
                "a/Main.rs",
                "b/main.rs.bak",
                "c/xmain.rs",
                "main.rs/readme",
                "m/a/i/n.rs"
            ]
        );
        assert_eq!(paths(&index, "").first().unwrap().as_ref(), "a/Main.rs");
        assert_eq!(
            paths(&index, "Main.rs").first().unwrap().as_ref(),
            "a/Main.rs"
        );
    }

    #[test]
    fn excludes_metadata_directories_submodules_deleted_and_invalid_paths() {
        let mut source = tree(&[
            ".git/config",
            "a/.GIT/HEAD",
            "vendor/sub/file.rs",
            "vendor/submarine.rs",
            "keep.rs",
            "../escape",
            "deleted.rs",
        ]);
        source.entries.push(FileTreeEntry {
            path: "vendor/sub".into(),
            kind: FileTreeKind::Submodule,
            index_status: ' ',
            worktree_status: ' ',
        });
        source.entries.push(FileTreeEntry {
            path: "ordinary-directory".into(),
            kind: FileTreeKind::Directory,
            index_status: ' ',
            worktree_status: ' ',
        });
        source
            .entries
            .iter_mut()
            .find(|entry| entry.path == "deleted.rs")
            .unwrap()
            .worktree_status = 'D';
        source.warnings.push(TreeWarning {
            path: "unreadable".into(),
            message: "permission denied".into(),
        });
        let index = SearchIndex::from_tree(&source).unwrap();
        let result = index.query("", 200, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            result
                .matches
                .iter()
                .map(|item| item.path.as_ref())
                .collect::<Vec<_>>(),
            ["keep.rs", "vendor/submarine.rs"]
        );
        assert_eq!(result.warning_count, 2);
        assert!(result.partial);
    }

    #[test]
    fn enforces_limits_before_large_path_allocations_and_keeps_previous_index_usable() {
        let source = tree(&["a.rs", "b.rs"]);
        let previous = SearchIndex::from_tree(&source).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            SearchIndex::with_limits(
                &source,
                SearchIndexLimits {
                    max_files: 1,
                    ..Default::default()
                },
                &cancel
            ),
            Err(SearchError::IndexLimit { .. })
        ));
        assert!(matches!(
            SearchIndex::with_limits(
                &source,
                SearchIndexLimits {
                    max_bytes: 1,
                    ..Default::default()
                },
                &cancel
            ),
            Err(SearchError::IndexLimit { .. })
        ));
        assert!(matches!(
            SearchIndex::with_limits(
                &source,
                SearchIndexLimits {
                    max_path_bytes: 2,
                    ..Default::default()
                },
                &cancel
            ),
            Err(SearchError::IndexLimit { .. })
        ));
        assert_eq!(paths(&previous, "").len(), 2);
        assert!(previous.estimated_bytes() <= SearchIndexLimits::default().max_bytes);
    }

    #[test]
    fn bounds_results_and_distinguishes_warning_timeout_cancellation_and_zero_matches() {
        let source = tree(&["a.rs", "b.rs", "c.rs"]);
        let index = SearchIndex::from_tree(&source).unwrap();
        let cancel = AtomicBool::new(false);
        let result = index.query("rs", 2, &cancel).unwrap();
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.total_matches, 3);
        assert!(result.truncated);
        assert!(!result.partial);
        let result = index.query_until("rs", 2, &cancel, Instant::now()).unwrap();
        assert!(result.timed_out && result.partial && result.truncated);
        assert!(matches!(
            index.query(&"中".repeat(257), 200, &cancel),
            Err(SearchError::QueryTooLong)
        ));
        cancel.store(true, AtomicOrdering::Relaxed);
        assert_eq!(
            index.query("rs", 2, &cancel).unwrap_err(),
            SearchError::Cancelled
        );
        assert!(matches!(
            SearchIndex::with_limits(&source, SearchIndexLimits::default(), &cancel),
            Err(SearchError::Cancelled)
        ));
    }

    #[test]
    fn rebuilt_index_replaces_deleted_paths_and_immutable_old_queries_stay_consistent() {
        let old = SearchIndex::from_tree(&tree(&["before.rs"])).unwrap();
        let new = SearchIndex::from_tree(&tree(&["after.rs"])).unwrap();
        assert_eq!(paths(&old, "")[0].as_ref(), "before.rs");
        assert!(paths(&new, "before").is_empty());
        assert_eq!(paths(&new, "after")[0].as_ref(), "after.rs");
    }

    #[test]
    fn large_result_sets_keep_only_200_and_reuse_cached_original_paths() {
        let mut source = RepositoryTree::default();
        for number in (0..1_000).rev() {
            source.entries.push(FileTreeEntry {
                path: format!("src/file_{number:04}.rs"),
                kind: FileTreeKind::File,
                index_status: ' ',
                worktree_status: ' ',
            });
        }
        let index = SearchIndex::from_tree(&source).unwrap();
        let cancel = AtomicBool::new(false);
        let first = index.query("file", usize::MAX, &cancel).unwrap();
        let repeated = index.query("file", usize::MAX, &cancel).unwrap();
        assert_eq!(first.total_matches, 1_000);
        assert_eq!(first.matches.len(), 200);
        assert!(first.truncated && !first.partial);
        assert_eq!(first.matches[0].path.as_ref(), "src/file_0000.rs");
        assert_eq!(first.matches[199].path.as_ref(), "src/file_0199.rs");
        assert!(Arc::ptr_eq(
            &first.matches[0].path,
            &repeated.matches[0].path
        ));
    }
}
