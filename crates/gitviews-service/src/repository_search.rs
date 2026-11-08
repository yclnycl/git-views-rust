//! Opt-in bounded search across registered initialized submodules. The UI owns
//! a cached immutable build and supplies cancellation for superseded queries.
use crate::{
    ContentSearchIndex, ContentSearchLimits, ContentSearchMatch, ContentSearchResults,
    FileSearchMatch, RepositoryService, SearchIndex, SearchIndexLimits, SearchOptions,
    SearchResults, content_search::checked_root, paths_equal, search_options::safe_relative,
};
use gitviews_git::{GitBackend, RepositoryTree};
use std::{
    error::Error,
    fmt, fs,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepositorySearchError {
    Cancelled,
    Invalid(String),
    Read(String),
}
impl fmt::Display for RepositorySearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("跨仓库搜索已取消。"),
            Self::Invalid(message) | Self::Read(message) => f.write_str(message),
        }
    }
}
impl Error for RepositorySearchError {}

#[derive(Clone, Debug)]
pub struct ScopedFileSearchMatch {
    pub root: PathBuf,
    pub parent_root: Option<PathBuf>,
    /// Path relative to the top search root, preserving the submodule prefix.
    pub display_path: Arc<str>,
    pub display_highlights: Vec<Range<usize>>,
    /// Root-relative path and its own UTF-8 highlights, safe to open in root.
    pub hit: FileSearchMatch,
}
#[derive(Clone, Debug)]
pub struct ScopedContentSearchMatch {
    pub root: PathBuf,
    pub parent_root: Option<PathBuf>,
    pub display_path: Arc<str>,
    pub hit: ContentSearchMatch,
}
#[derive(Clone, Debug)]
pub struct RepositoryPathSearchResults {
    pub matches: Vec<ScopedFileSearchMatch>,
    /// Aggregate counters; matches live in the scoped list above.
    pub stats: SearchResults,
    pub scope_count: usize,
    pub partial: bool,
    pub elapsed: Duration,
}
#[derive(Clone, Debug)]
pub struct RepositoryContentSearchResults {
    pub matches: Vec<ScopedContentSearchMatch>,
    pub stats: ContentSearchResults,
    pub scope_count: usize,
    pub partial: bool,
    pub elapsed: Duration,
}

#[derive(Debug)]
struct Scope {
    root: PathBuf,
    parent_root: Option<PathBuf>,
    prefix: String,
    paths: SearchIndex,
    contents: ContentSearchIndex,
    boundary: Option<GitBoundary>,
}

#[derive(Debug, Eq, PartialEq)]
struct GitBoundary {
    marker: Option<Vec<u8>>,
    index: Option<(u64, SystemTime)>,
    modules: Option<(u64, SystemTime)>,
}

fn plain(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        !metadata.file_type().is_symlink() && metadata.file_attributes() & 0x400 == 0
    }
    #[cfg(not(windows))]
    {
        !metadata.file_type().is_symlink()
    }
}

fn file_stamp(path: &Path) -> Result<Option<(u64, SystemTime)>, RepositorySearchError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if plain(&metadata) && metadata.is_file() => Ok(Some((
            metadata.len(),
            metadata
                .modified()
                .map_err(|error| RepositorySearchError::Read(error.to_string()))?,
        ))),
        Ok(_) => Err(RepositorySearchError::Read(
            "Git 搜索边界包含链接或异常文件，请刷新范围。".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(RepositorySearchError::Read(error.to_string())),
    }
}

fn git_boundary(root: &Path) -> Result<Option<GitBoundary>, RepositorySearchError> {
    let marker_path = root.join(".git");
    let metadata = match fs::symlink_metadata(&marker_path) {
        Ok(metadata) if plain(&metadata) => metadata,
        Ok(_) => {
            return Err(RepositorySearchError::Read(
                "Git 搜索根目录已变为链接，请重新打开仓库。".into(),
            ));
        }
        // Custom GitBackend implementations may provide virtual repository
        // metadata; native builds always have a .git directory or file.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(RepositorySearchError::Read(error.to_string())),
    };
    let (marker, git_dir) = if metadata.is_dir() {
        (None, marker_path)
    } else if metadata.is_file() && metadata.len() <= 16 * 1024 {
        let bytes = fs::read(&marker_path)
            .map_err(|error| RepositorySearchError::Read(error.to_string()))?;
        let path = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| text.strip_prefix("gitdir: "))
            .map(str::trim)
            .filter(|path| !path.is_empty() && !path.contains(['\0', '\n', '\r']))
            .ok_or_else(|| RepositorySearchError::Read("Git 目录指向无效，请刷新范围。".into()))?;
        let git_dir = fs::canonicalize(root.join(path))
            .map_err(|error| RepositorySearchError::Read(error.to_string()))?;
        (Some(bytes), git_dir)
    } else {
        return Err(RepositorySearchError::Read(
            "Git 搜索边界无效，请重新打开仓库。".into(),
        ));
    };
    Ok(Some(GitBoundary {
        marker,
        index: file_stamp(&git_dir.join("index"))?,
        modules: file_stamp(&root.join(".gitmodules"))?,
    }))
}

impl Scope {
    fn verify(&self) -> Result<(), RepositorySearchError> {
        let root = checked_root(&self.root)
            .map_err(|error| RepositorySearchError::Read(error.to_string()))?;
        if !paths_equal(&root, &self.root) || git_boundary(&root)? != self.boundary {
            return Err(RepositorySearchError::Read(
                "仓库或子模块登记已变化，请点击“刷新范围”后重试。".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Debug)]
pub struct RepositorySearchIndex {
    pub root: PathBuf,
    pub include_submodules: bool,
    /// Missing/unsafe modules and exhausted build budgets are visible to callers.
    pub warnings: Vec<String>,
    pub built_at: Instant,
    scopes: Vec<Scope>,
}
impl RepositorySearchIndex {
    pub fn build<B: GitBackend>(
        service: &RepositoryService<B>,
        root: &Path,
        tree: Arc<RepositoryTree>,
        include_submodules: bool,
        cancel: &AtomicBool,
    ) -> Result<Self, RepositorySearchError> {
        check(cancel)?;
        let root = checked_root(root)
            .map_err(|error| RepositorySearchError::Invalid(error.to_string()))?;
        service
            .validate_root(&root)
            .map_err(|error| RepositorySearchError::Invalid(error.to_string()))?;
        let built_at = Instant::now();
        let deadline = built_at + Duration::from_secs(3);
        let mut pending = vec![(root.clone(), String::new(), tree, 0, None)];
        let mut retained_entries = pending[0]
            .2
            .entries
            .len()
            .saturating_add(pending[0].2.submodules.len());
        let mut warnings = Vec::new();
        let mut cursor = 0;
        while include_submodules && cursor < pending.len() {
            check(cancel)?;
            if Instant::now() >= deadline {
                warnings.push("子模块索引达到时间预算，部分范围尚未读取。".into());
                break;
            }
            let (parent, prefix, tree, depth, _) = pending[cursor].clone();
            cursor += 1;
            let mut modules = tree
                .submodules
                .iter()
                .filter(|module| module.initialized)
                .collect::<Vec<_>>();
            modules.sort_unstable_by(|left, right| left.path.cmp(&right.path));
            for module in modules {
                check(cancel)?;
                if pending.len() >= 32 || depth >= 4 || Instant::now() >= deadline {
                    if warnings.len() < 64 {
                        warnings.push("子模块索引达到仓库数量、深度或时间预算。".into());
                    }
                    break;
                }
                if !safe_relative(&module.path) {
                    if warnings.len() < 64 {
                        warnings.push("跳过不安全的子模块登记路径。".into());
                    }
                    continue;
                }
                let candidate = parent.join(&module.path);
                let result: Result<(PathBuf, Arc<RepositoryTree>), String> = (|| {
                    let verified = checked_root(&candidate).map_err(|error| error.to_string())?;
                    if !verified.starts_with(&parent) || verified == parent {
                        return Err("子模块越出所属仓库。".into());
                    }
                    let metadata = service
                        .submodule_metadata_with_cancel(&parent, &candidate, cancel)
                        .map_err(|error| error.to_string())?;
                    if !paths_equal(&verified, &metadata.root) {
                        return Err("子模块路径已变化。".into());
                    }
                    check(cancel).map_err(|error| error.to_string())?;
                    let child_tree = service
                        .backend
                        .search_tree_with_cancel(&verified, cancel)
                        .map_err(|error| error.to_string())?;
                    check(cancel).map_err(|error| error.to_string())?;
                    service
                        .validate_root(&verified)
                        .map_err(|error| error.to_string())?;
                    Ok((verified, Arc::new(child_tree)))
                })();
                check(cancel)?;
                match result {
                    Ok((verified, child_tree)) => {
                        if pending
                            .iter()
                            .any(|(path, _, _, _, _)| paths_equal(path, &verified))
                        {
                            continue;
                        }
                        let cost = child_tree
                            .entries
                            .len()
                            .saturating_add(child_tree.submodules.len());
                        if retained_entries.saturating_add(cost) > 100_000 {
                            if warnings.len() < 64 {
                                warnings.push(
                                    "跨仓库目录达到 100000 条元数据预算，部分子模块未展开。".into(),
                                );
                            }
                            continue;
                        }
                        retained_entries += cost;
                        let prefix = joined(&prefix, &module.path);
                        pending.push((
                            verified,
                            prefix,
                            child_tree,
                            depth + 1,
                            Some(parent.clone()),
                        ));
                    }
                    Err(error) if warnings.len() < 64 => {
                        warnings.push(format!("跳过子模块 {}：{error}", module.path))
                    }
                    Err(_) => {}
                }
            }
        }
        let cache_bytes = (32 * 1024 * 1024) / pending.len();
        let mut scopes = Vec::new();
        let mut metadata_bytes: usize = 0;
        let mut files: usize = 0;
        for (scope_root, prefix, tree, _, parent_root) in pending {
            check(cancel)?;
            if !scopes.is_empty() && Instant::now() >= deadline {
                warnings.push("索引构建达到时间预算，部分范围尚未读取。".into());
                break;
            }
            let prefixed;
            let path_tree = if prefix.is_empty() {
                tree.as_ref()
            } else {
                prefixed = RepositoryTree {
                    entries: tree
                        .entries
                        .iter()
                        .map(|entry| {
                            let mut entry = entry.clone();
                            entry.path = joined(&prefix, &entry.path);
                            entry
                        })
                        .collect(),
                    submodules: tree
                        .submodules
                        .iter()
                        .map(|module| {
                            let mut module = module.clone();
                            module.path = joined(&prefix, &module.path);
                            module
                        })
                        .collect(),
                    warnings: tree.warnings.clone(),
                };
                &prefixed
            };
            let result = (|| {
                let paths = SearchIndex::with_limits(
                    path_tree,
                    SearchIndexLimits {
                        max_files: 100_000usize.saturating_sub(files).max(1),
                        max_bytes: (64 * 1024 * 1024usize)
                            .saturating_sub(metadata_bytes)
                            .max(1),
                        ..Default::default()
                    },
                    cancel,
                )
                .map_err(|error| error.to_string())?;
                let contents = ContentSearchIndex::with_limits(
                    &scope_root,
                    &tree,
                    ContentSearchLimits {
                        max_cache_bytes: cache_bytes,
                        max_files: 100_000usize.saturating_sub(files).max(1),
                        max_index_bytes: (64 * 1024 * 1024usize)
                            .saturating_sub(metadata_bytes + paths.estimated_bytes())
                            .max(1),
                        ..Default::default()
                    },
                    cancel,
                )
                .map_err(|error| error.to_string())?;
                if files + paths.len() > 100_000
                    || metadata_bytes + paths.estimated_bytes() + contents.estimated_bytes()
                        > 64 * 1024 * 1024
                {
                    return Err("索引达到 100000 文件或 64 MiB 元数据预算。".into());
                }
                Ok((paths, contents))
            })();
            check(cancel)?;
            match result {
                Ok((paths, contents)) => {
                    files += paths.len();
                    metadata_bytes += paths.estimated_bytes() + contents.estimated_bytes();
                    let boundary = git_boundary(&scope_root)?;
                    scopes.push(Scope {
                        root: scope_root,
                        parent_root,
                        prefix,
                        paths,
                        contents,
                        boundary,
                    });
                }
                Err(error) if scopes.is_empty() => return Err(RepositorySearchError::Read(error)),
                Err(error) if warnings.len() < 64 => warnings.push(error),
                Err(_) => {}
            }
        }
        check(cancel)?;
        Ok(Self {
            root,
            include_submodules,
            warnings,
            built_at,
            scopes,
        })
    }

    pub fn scope_count(&self) -> usize {
        self.scopes.len()
    }
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        self.scopes.iter().map(|scope| scope.root.as_path())
    }

    pub fn query_paths(
        &self,
        query: &str,
        limit: usize,
        cancel: &AtomicBool,
        options: &SearchOptions,
    ) -> Result<RepositoryPathSearchResults, RepositorySearchError> {
        validate(options, query, limit, cancel)?;
        let started = Instant::now();
        let deadline = started + Duration::from_millis(250);
        let mut stats = SearchResults::default();
        let mut matches = Vec::new();
        let mut partial = !self.warnings.is_empty();
        for scope in &self.scopes {
            check(cancel)?;
            scope.verify()?;
            if Instant::now() >= deadline {
                stats.timed_out = true;
                partial = true;
                break;
            }
            let result = scope
                .paths
                .query_until_options(query, limit.min(200), cancel, deadline, options)
                .map_err(|error| RepositorySearchError::Read(error.to_string()))?;
            stats.indexed_files += result.indexed_files;
            stats.total_matches += result.total_matches;
            stats.warning_count += result.warning_count;
            stats.partial |= result.partial;
            stats.timed_out |= result.timed_out;
            stats.truncated |= result.truncated;
            for mut hit in result.matches {
                let display_path = hit.path.clone();
                let display_highlights = hit.highlights.clone();
                if !scope.prefix.is_empty() {
                    let offset = scope.prefix.len() + 1;
                    let Some(relative) = hit.path.get(offset..) else {
                        continue;
                    };
                    hit.path = Arc::from(relative);
                    hit.highlights = display_highlights
                        .iter()
                        .filter_map(|range| {
                            (range.end > offset).then_some(
                                range.start.saturating_sub(offset)
                                    ..range.end.saturating_sub(offset),
                            )
                        })
                        .collect();
                }
                matches.push(ScopedFileSearchMatch {
                    root: scope.root.clone(),
                    parent_root: scope.parent_root.clone(),
                    display_path,
                    display_highlights,
                    hit,
                });
            }
        }
        matches.sort_unstable_by(|left, right| {
            right
                .hit
                .score
                .cmp(&left.hit.score)
                .then(
                    left.display_path
                        .chars()
                        .count()
                        .cmp(&right.display_path.chars().count()),
                )
                .then(left.display_path.cmp(&right.display_path))
        });
        stats.truncated |= matches.len() > limit.min(200);
        matches.truncate(limit.min(200));
        partial |= stats.partial || stats.timed_out;
        stats.partial |= partial;
        check(cancel)?;
        Ok(RepositoryPathSearchResults {
            matches,
            stats,
            scope_count: self.scopes.len(),
            partial,
            elapsed: started.elapsed(),
        })
    }

    pub fn query_contents(
        &self,
        query: &str,
        limit: usize,
        cancel: &AtomicBool,
        options: &SearchOptions,
    ) -> Result<RepositoryContentSearchResults, RepositorySearchError> {
        validate(options, query, limit, cancel)?;
        let started = Instant::now();
        let deadline = started + Duration::from_secs(3);
        let mut stats = ContentSearchResults::default();
        let mut matches = Vec::new();
        let mut partial = !self.warnings.is_empty();
        for scope in &self.scopes {
            check(cancel)?;
            scope.verify()?;
            let remaining = 128 * 1024 * 1024u64 - stats.read_bytes.min(128 * 1024 * 1024);
            if Instant::now() >= deadline || remaining == 0 || matches.len() >= limit.min(200) {
                stats.timed_out |= Instant::now() >= deadline;
                stats.truncated = true;
                partial = true;
                break;
            }
            let result = scope
                .contents
                .query_with_budget(
                    query,
                    limit.min(200) - matches.len(),
                    cancel,
                    options,
                    deadline,
                    remaining,
                    &scope.prefix,
                )
                .map_err(|error| RepositorySearchError::Read(error.to_string()))?;
            stats.indexed_files += result.indexed_files;
            stats.scanned_files += result.scanned_files;
            stats.cached_files += result.cached_files;
            stats.skipped_files += result.skipped_files;
            stats.binary_files += result.binary_files;
            stats.oversized_files += result.oversized_files;
            stats.read_bytes += result.read_bytes;
            stats.warning_count += result.warning_count;
            stats.timed_out |= result.timed_out;
            stats.truncated |= result.truncated;
            matches.extend(
                result
                    .matches
                    .into_iter()
                    .map(|hit| ScopedContentSearchMatch {
                        root: scope.root.clone(),
                        parent_root: scope.parent_root.clone(),
                        display_path: Arc::from(joined(&scope.prefix, &hit.path)),
                        hit,
                    }),
            );
        }
        matches.sort_unstable_by(|left, right| {
            left.display_path
                .cmp(&right.display_path)
                .then(left.hit.line_number.cmp(&right.hit.line_number))
        });
        partial |= stats.truncated || stats.timed_out;
        stats.elapsed = started.elapsed();
        check(cancel)?;
        Ok(RepositoryContentSearchResults {
            matches,
            stats,
            scope_count: self.scopes.len(),
            partial,
            elapsed: started.elapsed(),
        })
    }
}
fn joined(prefix: &str, path: &str) -> String {
    if prefix.is_empty() {
        path.to_owned()
    } else {
        format!("{prefix}/{path}")
    }
}
fn check(cancel: &AtomicBool) -> Result<(), RepositorySearchError> {
    if cancel.load(Ordering::Relaxed) {
        Err(RepositorySearchError::Cancelled)
    } else {
        Ok(())
    }
}
fn validate(
    options: &SearchOptions,
    query: &str,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<(), RepositorySearchError> {
    check(cancel)?;
    if limit == 0 {
        return Err(RepositorySearchError::Invalid(
            "搜索结果上限必须大于零。".into(),
        ));
    }
    options
        .validate(query)
        .map_err(|error| RepositorySearchError::Invalid(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitviews_git::{Git2Backend, GitError, RepositorySnapshot, SubmoduleInfo};
    use std::{fs, process::Command, sync::atomic::AtomicUsize};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    #[derive(Clone)]
    struct CountedBackend {
        full_reads: Arc<AtomicUsize>,
        search_trees: Arc<AtomicUsize>,
    }
    impl GitBackend for CountedBackend {
        fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
            self.full_reads.fetch_add(1, Ordering::Relaxed);
            Git2Backend.inspect(path)
        }
        fn inspect_metadata(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
            Git2Backend.inspect_metadata(path)
        }
        fn repository_root(&self, path: &Path) -> Result<PathBuf, GitError> {
            Git2Backend.repository_root(path)
        }
        fn registered_submodule(
            &self,
            path: &Path,
            relative: &str,
        ) -> Result<Option<SubmoduleInfo>, GitError> {
            Git2Backend.registered_submodule(path, relative)
        }
        fn tree(&self, path: &Path) -> Result<RepositoryTree, GitError> {
            Git2Backend.tree(path)
        }
        fn search_tree_with_cancel(
            &self,
            path: &Path,
            cancel: &AtomicBool,
        ) -> Result<RepositoryTree, GitError> {
            self.search_trees.fetch_add(1, Ordering::Relaxed);
            Git2Backend.search_tree_with_cancel(path, cancel)
        }
    }
    struct Fixture {
        base: PathBuf,
        root: PathBuf,
        child: PathBuf,
        grand: PathBuf,
        backend: CountedBackend,
    }
    impl Fixture {
        fn new() -> Self {
            let base =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/repository-search-tests");
            fs::create_dir_all(&base).unwrap();
            let base = fs::canonicalize(base).unwrap();
            let root = base.join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            init(&root);
            let child = root.join("modules/子模块");
            init(&child);
            let grand = child.join("nested/grand");
            init(&grand);
            fs::write(grand.join("same.txt"), "grand NeedLe 56\r\n").unwrap();
            commit(&grand);
            fs::write(child.join("same.txt"), "child NeedLe 34\n").unwrap();
            register(&child, "nested/grand", &grand);
            commit(&child);
            fs::write(root.join("same.txt"), "root NeedLe 12\n").unwrap();
            fs::write(root.join("sensitive.RS"), "Version 42\nneedle\n").unwrap();
            register(&root, "modules/子模块", &child);
            commit(&root);
            Self {
                base,
                root,
                child,
                grand,
                backend: CountedBackend {
                    full_reads: Arc::new(AtomicUsize::new(0)),
                    search_trees: Arc::new(AtomicUsize::new(0)),
                },
            }
        }
        fn build(&self, include: bool) -> RepositorySearchIndex {
            let service = RepositoryService::new(self.backend.clone());
            let tree = Arc::new(service.tree(&self.root).unwrap());
            RepositorySearchIndex::build(
                &service,
                &self.root,
                tree,
                include,
                &AtomicBool::new(false),
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if self.root.parent() == Some(&self.base) {
                // Git objects can be readonly on Windows; cleanup is best effort
                // inside the verified fixture directory only.
                let _ = fs::remove_dir_all(&self.root);
            }
        }
    }
    fn git(root: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        command
            .args(["-c", "core.hooksPath=", "-c", "init.templateDir=", "-C"])
            .arg(crate::display_path(root))
            .args(args)
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Search Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Search Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    fn init(root: &Path) {
        fs::create_dir_all(root).unwrap();
        git(root, &["init", "--quiet", "--initial-branch=main"]);
        git(root, &["config", "core.autocrlf", "false"]);
    }
    fn commit(root: &Path) {
        git(root, &["add", "--all"]);
        git(root, &["commit", "--quiet", "-m", "search fixture"]);
    }
    fn register(root: &Path, path: &str, child: &Path) {
        fs::write(
            root.join(".gitmodules"),
            format!(
                "[submodule \"child\"]\n\tpath = {path}\n\turl = https://example.invalid/child\n"
            ),
        )
        .unwrap();
        let oid = git(child, &["rev-parse", "HEAD"]);
        git(
            root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                oid.trim(),
                path,
            ],
        );
    }
    #[test]
    fn submodules_are_explicit_and_results_keep_exact_root_and_direct_parent() {
        let fixture = Fixture::new();
        let cancelled = AtomicBool::new(false);
        let options = SearchOptions::default();
        let local = fixture.build(false);
        let hits = local
            .query_contents("NeedLe", 200, &cancelled, &options)
            .unwrap();
        assert_eq!(hits.matches.len(), 2); // same.txt plus sensitive.RS needle.
        assert!(
            hits.matches
                .iter()
                .all(|hit| paths_equal(&hit.root, &fixture.root) && hit.parent_root.is_none())
        );
        assert_eq!(fixture.backend.search_trees.load(Ordering::Relaxed), 0);
        let all = fixture.build(true);
        assert_eq!(all.scope_count(), 3, "{:?}", all.warnings);
        let hits = all
            .query_contents("NeedLe", 200, &cancelled, &options)
            .unwrap();
        assert_eq!(hits.matches.len(), 4, "{:?}", all.warnings);
        let child = hits
            .matches
            .iter()
            .find(|hit| paths_equal(&hit.root, &fixture.child))
            .unwrap();
        assert!(paths_equal(
            child.parent_root.as_deref().unwrap(),
            &fixture.root
        ));
        assert_eq!(child.hit.path.as_ref(), "same.txt");
        assert_eq!(child.display_path.as_ref(), "modules/子模块/same.txt");
        let grand = hits
            .matches
            .iter()
            .find(|hit| paths_equal(&hit.root, &fixture.grand))
            .unwrap();
        assert!(paths_equal(
            grand.parent_root.as_deref().unwrap(),
            &fixture.child
        ));
        assert_eq!(grand.hit.path.as_ref(), "same.txt");
        assert_eq!(
            fixture.backend.full_reads.load(Ordering::Relaxed),
            0,
            "cross-search must not read overview/history"
        );
        assert_eq!(fixture.backend.search_trees.load(Ordering::Relaxed), 2);
        all.query_contents("NeedLe", 200, &cancelled, &options)
            .unwrap();
        assert_eq!(
            fixture.backend.search_trees.load(Ordering::Relaxed),
            2,
            "queries reuse the immutable build"
        );
    }
    #[test]
    fn scoped_path_filters_precede_ranking_truncation_and_keep_unicode_ranges() {
        let fixture = Fixture::new();
        let index = fixture.build(true);
        let options = SearchOptions {
            directory: Some("modules/子模块".into()),
            include_globs: vec!["*.txt".into()],
            exclude_globs: vec!["**/nested/**".into()],
            extensions: vec!["txt".into()],
            regex: true,
            ..Default::default()
        };
        let result = index
            .query_paths(r"same\.txt$", 1, &AtomicBool::new(false), &options)
            .unwrap();
        assert_eq!(result.matches.len(), 1);
        let hit = &result.matches[0];
        assert!(paths_equal(&hit.root, &fixture.child));
        assert_eq!(hit.hit.path.as_ref(), "same.txt");
        for range in &hit.hit.highlights {
            assert_eq!(&hit.hit.path[range.clone()], "same.txt");
        }
        for range in &hit.display_highlights {
            assert_eq!(&hit.display_path[range.clone()], "same.txt");
        }
        let by_module_name = index
            .query_paths(
                "子模块",
                10,
                &AtomicBool::new(false),
                &SearchOptions::default(),
            )
            .unwrap();
        assert!(
            by_module_name
                .matches
                .iter()
                .any(|hit| paths_equal(&hit.root, &fixture.child))
        );
        assert!(by_module_name.matches.iter().all(|hit| {
            hit.display_highlights.iter().all(|range| {
                hit.display_path.is_char_boundary(range.start)
                    && hit.display_path.is_char_boundary(range.end)
            })
        }));
    }
    #[test]
    fn content_regex_case_and_glob_filters_apply_before_io_even_after_literal_cache_miss() {
        let fixture = Fixture::new();
        let index = fixture.build(false);
        let cancel = AtomicBool::new(false);
        let options = SearchOptions {
            extensions: vec!["rs".into()],
            case_sensitive: true,
            ..Default::default()
        };
        let cold = index
            .query_contents("absent-literal", 10, &cancel, &options)
            .unwrap();
        assert_eq!(cold.stats.scanned_files, 1);
        assert!(cold.matches.is_empty());
        let regex = SearchOptions {
            regex: true,
            ..options.clone()
        };
        let warm = index
            .query_contents(r"Version \d+", 10, &cancel, &regex)
            .unwrap();
        assert_eq!(warm.matches.len(), 1);
        assert_eq!(warm.stats.read_bytes, 0);
        assert_eq!(warm.stats.cached_files, 1);
        assert_eq!(
            &warm.matches[0].hit.snippet[warm.matches[0].hit.highlights[0].clone()],
            "Version 42"
        );
        assert!(
            index
                .query_contents("version", 10, &cancel, &options)
                .unwrap()
                .matches
                .is_empty()
        );
        assert_eq!(
            index
                .query_contents("Version", 10, &cancel, &options)
                .unwrap()
                .matches
                .len(),
            1
        );
        assert!(
            index
                .query_contents(r"version \d+", 10, &cancel, &regex)
                .unwrap()
                .matches
                .is_empty()
        );
        let ignore_case = SearchOptions {
            case_sensitive: false,
            ..regex
        };
        assert_eq!(
            index
                .query_contents(r"version \d+", 10, &cancel, &ignore_case)
                .unwrap()
                .matches
                .len(),
            1
        );
    }
    #[test]
    fn invalid_patterns_and_cancellation_fail_before_reading() {
        let fixture = Fixture::new();
        let index = fixture.build(true);
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            index.query_paths("x", 10, &cancelled, &SearchOptions::default()),
            Err(RepositorySearchError::Cancelled)
        ));
        assert!(matches!(
            index.query_contents("x", 10, &cancelled, &SearchOptions::default()),
            Err(RepositorySearchError::Cancelled)
        ));
        let invalid = SearchOptions {
            regex: true,
            ..Default::default()
        };
        assert!(matches!(
            index.query_contents("(", 10, &AtomicBool::new(false), &invalid),
            Err(RepositorySearchError::Invalid(_))
        ));
        let service = RepositoryService::new(fixture.backend.clone());
        assert!(matches!(
            RepositorySearchIndex::build(
                &service,
                &fixture.root,
                Arc::new(RepositoryTree::default()),
                true,
                &cancelled
            ),
            Err(RepositorySearchError::Cancelled)
        ));
    }
    #[test]
    fn unregistered_nested_repository_and_deinitialized_child_are_never_traversed() {
        let fixture = Fixture::new();
        let rogue = fixture.root.join("rogue");
        init(&rogue);
        fs::write(rogue.join("private.txt"), "ROGUE_SECRET\n").unwrap();
        commit(&rogue);
        fs::rename(
            fixture.child.join(".git"),
            fixture.child.join("git-metadata-disabled"),
        )
        .unwrap();
        let index = fixture.build(true);
        assert_eq!(index.scope_count(), 1);
        assert!(
            index
                .query_contents(
                    "ROGUE_SECRET",
                    10,
                    &AtomicBool::new(false),
                    &SearchOptions::default()
                )
                .unwrap()
                .matches
                .is_empty()
        );
        assert!(
            index
                .query_paths(
                    "private",
                    10,
                    &AtomicBool::new(false),
                    &SearchOptions::default()
                )
                .unwrap()
                .matches
                .is_empty()
        );
    }

    #[test]
    fn cached_scope_is_rejected_after_git_marker_or_registration_changes() {
        let fixture = Fixture::new();
        let index = fixture.build(true);
        fs::rename(
            fixture.child.join(".git"),
            fixture.child.join("metadata-disabled"),
        )
        .unwrap();
        assert!(matches!(
            index.query_paths(
                "same",
                10,
                &AtomicBool::new(false),
                &SearchOptions::default()
            ),
            Err(RepositorySearchError::Read(_))
        ));
        assert!(matches!(
            index.query_contents(
                "NeedLe",
                10,
                &AtomicBool::new(false),
                &SearchOptions::default()
            ),
            Err(RepositorySearchError::Read(_))
        ));
        fs::rename(
            fixture.child.join("metadata-disabled"),
            fixture.child.join(".git"),
        )
        .unwrap();
        let index = fixture.build(true);
        fixture_git_remove_module(&fixture.root);
        assert!(matches!(
            index.query_paths(
                "same",
                10,
                &AtomicBool::new(false),
                &SearchOptions::default()
            ),
            Err(RepositorySearchError::Read(_))
        ));
    }

    fn fixture_git_remove_module(root: &Path) {
        git(root, &["update-index", "--force-remove", "modules/子模块"]);
    }
}
