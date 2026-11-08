use super::*;
use gitviews_service::{
    ContentSearchIndex, ContentSearchMatch, FileSearchMatch, RepositorySearchIndex,
    ScopedContentSearchMatch, ScopedFileSearchMatch, SearchIndex, SearchOptions,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use std::{
    ops::Range,
    sync::{
        Weak,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(test)]
mod headless_tests;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum SearchMode {
    #[default]
    All,
    Paths,
    Contents,
}

#[derive(Clone)]
enum SearchHit {
    Path(FileSearchMatch),
    Content(ContentSearchMatch),
    ScopedPath(ScopedFileSearchMatch),
    ScopedContent(ScopedContentSearchMatch),
}

impl SearchHit {
    fn path(&self) -> &str {
        match self {
            Self::Path(hit) => &hit.path,
            Self::Content(hit) => &hit.path,
            Self::ScopedPath(hit) => &hit.hit.path,
            Self::ScopedContent(hit) => &hit.hit.path,
        }
    }
    fn line(&self) -> Option<usize> {
        match self {
            Self::Path(_) => None,
            Self::Content(hit) => Some(hit.line_number),
            Self::ScopedPath(_) => None,
            Self::ScopedContent(hit) => Some(hit.hit.line_number),
        }
    }
    fn display_path(&self) -> &str {
        match self {
            Self::ScopedPath(hit) => &hit.display_path,
            Self::ScopedContent(hit) => &hit.display_path,
            _ => self.path(),
        }
    }
    fn highlights(&self) -> Vec<Range<usize>> {
        match self {
            Self::Path(hit) => hit.highlights.clone(),
            Self::ScopedPath(hit) => hit.display_highlights.clone(),
            _ => Vec::new(),
        }
    }
    fn content(&self) -> Option<&ContentSearchMatch> {
        match self {
            Self::Content(hit) => Some(hit),
            Self::ScopedContent(hit) => Some(&hit.hit),
            _ => None,
        }
    }
}

struct CrossCache {
    root: PathBuf,
    tree: Weak<RepositoryTree>,
    index: Arc<RepositorySearchIndex>,
}

struct FilterInputs {
    fields: Vec<Entity<InputState>>,
    _subscriptions: Vec<Subscription>,
}

pub(super) struct PendingSearchOpen {
    root: PathBuf,
    path: String,
    line: Option<usize>,
    repository_id: u64,
    scope_id: u64,
}

struct CachedIndex {
    root: PathBuf,
    tree: Weak<RepositoryTree>,
    index: Option<Arc<SearchIndex>>,
    content: Option<Arc<ContentSearchIndex>>,
}

struct ResultScope {
    root: PathBuf,
    repository_id: u64,
    scope_id: u64,
    tree: Weak<RepositoryTree>,
}

impl ResultScope {
    fn matches(
        &self,
        root: &std::path::Path,
        repository_id: u64,
        scope_id: u64,
        tree: &Arc<RepositoryTree>,
    ) -> bool {
        paths_equal(&self.root, root)
            && self.repository_id == repository_id
            && self.scope_id == scope_id
            && self
                .tree
                .upgrade()
                .is_some_and(|previous| Arc::ptr_eq(tree, &previous))
    }
}

#[derive(Default)]
pub(super) struct FileSearch {
    pub open: bool,
    query: String,
    mode: SearchMode,
    options: SearchOptions,
    include_submodules: bool,
    filters: Option<FilterInputs>,
    cross_cache: Option<CrossCache>,
    pub(super) pending_open: Option<PendingSearchOpen>,
    generation: u64,
    selected: usize,
    loading: bool,
    status: String,
    results: Arc<Vec<SearchHit>>,
    result_scope: Option<ResultScope>,
    empty_message: Option<&'static str>,
    cache: Option<CachedIndex>,
    cancel: Arc<AtomicBool>,
    task: Option<Task<()>>,
    scroll: UniformListScrollHandle,
}

impl Drop for FileSearch {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

impl Workspace {
    pub(super) fn open_file_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.remote_blocks_navigation(cx) {
            return;
        }
        if self.preview || self.changes_repository().is_none() {
            self.notice = Some("请先打开本地仓库，再使用搜索。".into());
            cx.notify();
            return;
        }
        self.file_search.open = true;
        self.file_search.pending_open = None;
        if self.file_search.filters.is_none() {
            let mut subscriptions = Vec::new();
            let fields = [
                "目录范围，例如 src",
                "文件类型，例如 rs,java",
                "包含，例如 **/*.rs",
                "排除，例如 **/generated/**",
            ]
            .into_iter()
            .enumerate()
            .map(|(index, placeholder)| {
                let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
                subscriptions.push(cx.subscribe(&input, move |view, input, event, cx| {
                    if !matches!(event, InputEvent::Change) {
                        return;
                    }
                    let value = input.read(cx).value().to_string();
                    let items = || {
                        value
                            .split([',', ';', '；'])
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                            .map(str::to_owned)
                            .collect::<Vec<_>>()
                    };
                    match index {
                        0 => {
                            view.file_search.options.directory =
                                (!value.trim().is_empty()).then(|| value.trim().to_owned())
                        }
                        1 => view.file_search.options.extensions = items(),
                        2 => view.file_search.options.include_globs = items(),
                        _ => view.file_search.options.exclude_globs = items(),
                    }
                    view.query_files(view.file_search.query.clone(), cx);
                }));
                input
            })
            .collect();
            self.file_search.filters = Some(FilterInputs {
                fields,
                _subscriptions: subscriptions,
            });
        }
        self.active_input = None;
        self.menu = None;
        self.inputs[Field::Search.index()]
            .read(cx)
            .focus_handle(cx)
            .focus(window, cx);
        self.query_files(
            self.inputs[Field::Search.index()]
                .read(cx)
                .value()
                .to_string(),
            cx,
        );
        cx.notify();
    }

    pub(super) fn close_file_search(&mut self, cx: &mut Context<Self>) {
        self.file_search.open = false;
        self.file_search.generation = self.file_search.generation.wrapping_add(1);
        self.file_search.cancel.store(true, Ordering::Release);
        self.file_search.task = None;
        self.file_search.loading = false;
        self.file_search.results = Arc::default();
        self.file_search.result_scope = None;
        self.file_search.empty_message = None;
        cx.notify();
    }

    fn search_is_current(&self, generation: u64, scope: &ResultScope) -> bool {
        self.file_search.open
            && self.file_search.generation == generation
            && self
                .changes_repository()
                .zip(self.current_search_tree())
                .is_some_and(|(repo, tree)| {
                    scope.matches(&repo.root, self.request_id, self.submodule_request_id, tree)
                })
    }

    pub(super) fn query_files(&mut self, query: String, cx: &mut Context<Self>) {
        self.file_search.query = query.clone();
        self.file_search.cancel.store(true, Ordering::Release);
        self.file_search.task = None;
        self.file_search.generation = self.file_search.generation.wrapping_add(1);
        self.file_search.loading = false;
        self.file_search.results = Arc::default();
        self.file_search.result_scope = None;
        self.file_search.empty_message = None;
        if !self.file_search.open {
            return;
        }
        let Some(repo) = self.changes_repository() else {
            self.file_search.status = "请先打开本地仓库。".into();
            cx.notify();
            return;
        };
        let root = repo.root.clone();
        let name = repo.name.clone();
        let Some(tree) = self.current_search_tree().cloned() else {
            self.file_search.status = "文件目录尚未准备好，请刷新仓库后重试。".into();
            cx.notify();
            return;
        };
        if let Err(error) = self.file_search.options.validate(&query) {
            self.file_search.status = error.to_string();
            cx.notify();
            return;
        }
        if self.file_search.include_submodules {
            self.query_across_submodules(query, root, tree, name, cx);
            return;
        }
        let cached = self.file_search.cache.as_ref().filter(|cached| {
            paths_equal(&cached.root, &root)
                && cached
                    .tree
                    .upgrade()
                    .is_some_and(|previous| Arc::ptr_eq(&previous, &tree))
        });
        let cached_path = cached.and_then(|cached| cached.index.clone());
        let cached_content = cached.and_then(|cached| cached.content.clone());
        if cached.is_none() {
            self.file_search.cache = None;
        }
        let generation = self.file_search.generation;
        let repository_id = self.request_id;
        let scope_id = self.submodule_request_id;
        let tree_key = Arc::downgrade(&tree);
        let mode = self.file_search.mode;
        let options = self.file_search.options.clone();
        // Empty queries list paths only; never read all file contents on opening.
        let search_content = mode != SearchMode::Paths && !query.trim().is_empty();
        let cancel = Arc::new(AtomicBool::new(false));
        self.file_search.cancel = cancel.clone();
        self.file_search.loading = true;
        self.file_search.empty_message = Some("正在查询…");
        self.file_search.selected = 0;
        self.file_search.status = format!("{name} · 正在搜索…");
        self.file_search.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let worker_cancel = cancel.clone();
            let worker_tree = tree.clone();
            let worker_query = query.clone();
            let worker_options = options.clone();
            let result = tasks::spawn(
                cx.background_executor(),
                "files.search",
                generation,
                move || {
                    if worker_cancel.load(Ordering::Acquire) {
                        return Err(gitviews_service::SearchError::Cancelled);
                    }
                    if mode == SearchMode::Contents {
                        return Ok((cached_path, None));
                    }
                    let index = match cached_path {
                        Some(index) => index,
                        None => Arc::new(SearchIndex::with_limits(
                            &worker_tree,
                            Default::default(),
                            &worker_cancel,
                        )?),
                    };
                    let results = index.query_with_options(
                        &worker_query,
                        if mode == SearchMode::All { 100 } else { 200 },
                        &worker_cancel,
                        &worker_options,
                    )?;
                    Ok((Some(index), Some(results)))
                },
            )
            .await;
            let scope = ResultScope {
                root: root.clone(),
                repository_id,
                scope_id,
                tree: tree_key.clone(),
            };
            let path_status = this
                .update(cx, |view, cx| {
                    if !view.search_is_current(generation, &scope) {
                        return None;
                    }
                    let (index, results, path_error) = match result {
                        Ok((index, results)) => (index, results, None),
                        Err(error) => (None, None, Some(error.to_string())),
                    };
                    if !search_content && let Some(error) = &path_error {
                        view.file_search.loading = false;
                        view.file_search.empty_message = None;
                        view.file_search.status = error.clone();
                        cx.notify();
                        return None;
                    }
                    let extra = results.as_ref().map_or("", |results| {
                        if results.timed_out {
                            " · 路径查询达到时间预算"
                        } else if results.partial {
                            " · 部分路径不可读取"
                        } else if results.truncated {
                            " · 路径结果已截断，请继续输入"
                        } else {
                            ""
                        }
                    });
                    let count = results.as_ref().map_or(0, |results| results.matches.len());
                    view.file_search.results = Arc::new(
                        results
                            .into_iter()
                            .flat_map(|results| results.matches)
                            .map(SearchHit::Path)
                            .collect(),
                    );
                    let summary = match path_error {
                        Some(error) => format!("{name} · 路径搜索：{error}"),
                        None => format!("{name} · 路径 {count} 项{extra}"),
                    };
                    view.file_search.status = format!(
                        "{summary}{}",
                        if search_content {
                            " · 正在搜索内容…"
                        } else {
                            ""
                        }
                    );
                    view.file_search.result_scope = Some(ResultScope {
                        root: root.clone(),
                        repository_id,
                        scope_id,
                        tree: tree_key.clone(),
                    });
                    view.file_search.cache = Some(CachedIndex {
                        root: root.clone(),
                        tree: tree_key.clone(),
                        index,
                        content: cached_content.clone(),
                    });
                    view.file_search.loading = search_content;
                    view.file_search.empty_message = Some(if search_content {
                        "正在搜索文件内容…"
                    } else {
                        "没有匹配路径，请修改搜索词"
                    });
                    view.file_search
                        .scroll
                        .scroll_to_item(0, ScrollStrategy::Top);
                    cx.notify();
                    search_content.then_some(summary)
                })
                .ok()
                .flatten();
            let Some(path_status) = path_status else {
                return;
            };
            let worker_root = root.clone();
            let index_cancel = cancel.clone();
            let index = tasks::spawn(
                cx.background_executor(),
                "files.content-index",
                generation,
                move || match cached_content {
                    Some(index) => Ok(index),
                    None => ContentSearchIndex::with_limits(
                        &worker_root,
                        &tree,
                        Default::default(),
                        &index_cancel,
                    )
                    .map(Arc::new),
                },
            )
            .await;
            // Publish the shared cache before the scan: a superseding query can
            // reuse already-read files even when this query is cancelled.
            let index = this
                .update(cx, |view, cx| {
                    if !view.search_is_current(generation, &scope) {
                        return None;
                    }
                    match index {
                        Ok(index) => {
                            if let Some(cached) = &mut view.file_search.cache {
                                cached.content = Some(index.clone());
                            }
                            Some(index)
                        }
                        Err(error) => {
                            view.file_search.loading = false;
                            view.file_search.empty_message = None;
                            view.file_search.status = format!("{path_status} · 内容搜索：{error}");
                            cx.notify();
                            None
                        }
                    }
                })
                .ok()
                .flatten();
            let Some(index) = index else {
                return;
            };
            let result = tasks::spawn(
                cx.background_executor(),
                "files.search-content",
                generation,
                move || {
                    index.query_with_options(
                        &query,
                        if mode == SearchMode::All { 100 } else { 200 },
                        &cancel,
                        &options,
                    )
                },
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                        if !view.search_is_current(generation, &scope) { return; }
                        view.file_search.loading = false;
                        view.file_search.empty_message = Some("没有匹配结果，请修改搜索词；内容搜索跳过二进制和超大文件");
                        match result {
                            Ok(results) => {
                                let contents = results.matches.len();
                                let extra = if results.timed_out {" · 已达时间预算，覆盖不完整"}
                                    else if results.truncated {" · 结果或读取预算已达上限，请缩小搜索范围"} else {""};
                                view.file_search.status = format!("{path_status} / 内容 {contents} 行 · 扫描 {}，缓存 {}，跳过 {} · {}ms{extra}{}",
                                    results.scanned_files, results.cached_files, results.skipped_files, results.elapsed.as_millis(),
                                    if results.warning_count > 0 {" · 部分文件不可读取"} else {""});
                                let mut hits = (*view.file_search.results).clone();
                                hits.extend(results.matches.into_iter().map(SearchHit::Content));
                                view.file_search.results = Arc::new(hits);
                            }
                            Err(error) => view.file_search.status = format!("{path_status} · 内容搜索：{error}"),
                        }
                        cx.notify();
                    });
        }));
        cx.notify();
    }

    fn query_across_submodules(
        &mut self,
        query: String,
        root: PathBuf,
        tree: Arc<RepositoryTree>,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let cached = self
            .file_search
            .cross_cache
            .as_ref()
            .filter(|cache| {
                paths_equal(&cache.root, &root)
                    && cache.index.built_at.elapsed() < Duration::from_secs(10)
                    && cache
                        .tree
                        .upgrade()
                        .is_some_and(|previous| Arc::ptr_eq(&previous, &tree))
            })
            .map(|cache| cache.index.clone());
        if cached.is_none() {
            self.file_search.cross_cache = None;
        }
        let service = self.service.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.file_search.cancel = cancel.clone();
        let generation = self.file_search.generation;
        let repository_id = self.request_id;
        let scope_id = self.submodule_request_id;
        let tree_key = Arc::downgrade(&tree);
        let mode = self.file_search.mode;
        let options = self.file_search.options.clone();
        let search_content = mode != SearchMode::Paths && !query.trim().is_empty();
        self.file_search.loading = true;
        self.file_search.selected = 0;
        self.file_search.status = format!("{name} · 正在索引已登记子模块…");
        self.file_search.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let worker_root = root.clone();
            let stopped = cancel.clone();
            let built = tasks::spawn(
                cx.background_executor(),
                "search.scopes",
                generation,
                move || match cached {
                    Some(index) if !stopped.load(Ordering::Acquire) => Ok(index),
                    _ => RepositorySearchIndex::build(&service, &worker_root, tree, true, &stopped)
                        .map(Arc::new),
                },
            )
            .await;
            let scope = ResultScope {
                root: root.clone(),
                repository_id,
                scope_id,
                tree: tree_key.clone(),
            };
            let Some(index) = this
                .update(cx, |view, cx| {
                    if !view.search_is_current(generation, &scope) {
                        return None;
                    }
                    match built {
                        Ok(index) => {
                            view.file_search.cross_cache = Some(CrossCache {
                                root: root.clone(),
                                tree: tree_key,
                                index: index.clone(),
                            });
                            Some(index)
                        }
                        Err(error) => {
                            view.file_search.loading = false;
                            view.file_search.status = error.to_string();
                            cx.notify();
                            None
                        }
                    }
                })
                .ok()
                .flatten()
            else {
                return;
            };
            let worker = index.clone();
            let stopped = cancel.clone();
            let needle = query.clone();
            let filters = options.clone();
            let paths = tasks::spawn(
                cx.background_executor(),
                "search.scoped-paths",
                generation,
                move || {
                    if mode == SearchMode::Contents {
                        return Ok(None);
                    }
                    worker
                        .query_paths(
                            &needle,
                            if mode == SearchMode::All { 100 } else { 200 },
                            &stopped,
                            &filters,
                        )
                        .map(Some)
                },
            )
            .await;
            let Some(path_status) = this
                .update(cx, |view, cx| {
                    if !view.search_is_current(generation, &scope) {
                        return None;
                    }
                    let summary = match paths {
                        Ok(Some(results)) => {
                            let count = results.matches.len();
                            let limited = results.partial || results.stats.truncated;
                            view.file_search.results = Arc::new(
                                results
                                    .matches
                                    .into_iter()
                                    .map(SearchHit::ScopedPath)
                                    .collect(),
                            );
                            format!(
                                "{name} · {} 个仓库 · 路径 {count} 项{}",
                                results.scope_count,
                                if limited {
                                    " · 部分范围或结果已截断"
                                } else {
                                    ""
                                }
                            )
                        }
                        Ok(None) => format!("{name} · {} 个仓库", index.scope_count()),
                        Err(error) => format!("{name} · 路径搜索：{error}"),
                    };
                    view.file_search.result_scope = Some(ResultScope {
                        root: root.clone(),
                        repository_id,
                        scope_id,
                        tree: scope.tree.clone(),
                    });
                    view.file_search.status = format!(
                        "{summary}{}",
                        if search_content {
                            " · 正在搜索内容…"
                        } else {
                            ""
                        }
                    );
                    view.file_search.loading = search_content;
                    view.file_search.empty_message = Some(if search_content {
                        "正在搜索文件内容…"
                    } else {
                        "没有匹配路径"
                    });
                    view.file_search
                        .scroll
                        .scroll_to_item(0, ScrollStrategy::Top);
                    cx.notify();
                    Some(summary)
                })
                .ok()
                .flatten()
            else {
                return;
            };
            if !search_content {
                return;
            }
            let contents = tasks::spawn(
                cx.background_executor(),
                "search.scoped-contents",
                generation,
                move || {
                    index.query_contents(
                        &query,
                        if mode == SearchMode::All { 100 } else { 200 },
                        &cancel,
                        &options,
                    )
                },
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if !view.search_is_current(generation, &scope) {
                    return;
                }
                view.file_search.loading = false;
                view.file_search.empty_message = Some("没有匹配结果；二进制和超大文件会跳过");
                match contents {
                    Ok(results) => {
                        view.file_search.status = format!(
                            "{path_status} / 内容 {} 行 · 扫描 {}，缓存 {} · {}ms{}",
                            results.matches.len(),
                            results.stats.scanned_files,
                            results.stats.cached_files,
                            results.elapsed.as_millis(),
                            if results.partial {
                                " · 范围、时间或结果预算已截断"
                            } else {
                                ""
                            }
                        );
                        let mut hits = (*view.file_search.results).clone();
                        hits.extend(results.matches.into_iter().map(SearchHit::ScopedContent));
                        view.file_search.results = Arc::new(hits);
                    }
                    Err(error) => {
                        view.file_search.status = format!("{path_status} · 内容搜索：{error}")
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn move_file_search(&mut self, direction: isize, cx: &mut Context<Self>) {
        let count = self.file_search.results.len();
        if count > 0 {
            self.file_search.selected = (self.file_search.selected as isize + direction)
                .rem_euclid(count as isize) as usize;
            self.file_search
                .scroll
                .scroll_to_item(self.file_search.selected, ScrollStrategy::Center);
            cx.notify();
        }
    }

    pub(super) fn accept_file_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .file_search
            .result_scope
            .as_ref()
            .is_some_and(|scope| self.search_is_current(self.file_search.generation, scope))
        {
            self.refresh_file_search(cx);
            return;
        }
        if let Some(hit) = self
            .file_search
            .results
            .get(self.file_search.selected)
            .cloned()
        {
            let scoped = match &hit {
                SearchHit::ScopedPath(hit) => Some((hit.root.clone(), hit.parent_root.clone())),
                SearchHit::ScopedContent(hit) => Some((hit.root.clone(), hit.parent_root.clone())),
                _ => None,
            };
            if let Some((target, Some(parent))) = scoped
                && self
                    .changes_repository()
                    .is_none_or(|repo| !paths_equal(&repo.root, &target))
            {
                let path = hit.path().to_owned();
                let line = hit.line();
                self.close_file_search(cx);
                self.focus.focus(window, cx);
                self.read_submodule(parent, target.clone(), cx);
                self.file_search.pending_open = Some(PendingSearchOpen {
                    root: target,
                    path,
                    line,
                    repository_id: self.request_id,
                    scope_id: self.submodule_request_id,
                });
                return;
            }
            let root = self.changes_repository().map(|repo| repo.root.clone());
            self.close_file_search(cx);
            self.focus.focus(window, cx);
            self.open_preview_tab(hit.path().to_owned(), false, None, true, cx);
            if let Some(line) = hit.line().filter(|_| {
                self.active_preview_key().is_some_and(|key| {
                    key.path == hit.path()
                        && !key.staged
                        && key.revision.is_none()
                        && root
                            .as_ref()
                            .is_some_and(|root| paths_equal(&key.root, root))
                })
            }) {
                self.preview_pane
                    .update(cx, |pane, cx| pane.navigate_source_line(line, cx));
            }
        }
    }

    pub(super) fn complete_search_navigation(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.file_search.pending_open.take() else {
            return;
        };
        if pending.repository_id != self.request_id
            || pending.scope_id != self.submodule_request_id
            || self
                .changes_repository()
                .is_none_or(|repo| !paths_equal(&repo.root, &pending.root))
        {
            return;
        }
        self.open_preview_tab(pending.path, false, None, true, cx);
        if let Some(line) = pending.line {
            self.preview_pane
                .update(cx, |pane, cx| pane.navigate_source_line(line, cx));
        }
    }

    pub(super) fn refresh_file_search(&mut self, cx: &mut Context<Self>) {
        if self.file_search.open {
            self.query_files(self.file_search.query.clone(), cx);
        }
    }

    pub(super) fn content_search_open(&self) -> bool {
        self.file_search.open
            && self.file_search.mode != SearchMode::Paths
            && !self.file_search.query.trim().is_empty()
    }

    fn current_search_tree(&self) -> Option<&Arc<RepositoryTree>> {
        self.submodule_changes
            .as_ref()
            .map(|scope| &scope.tree)
            .or(self.tree.as_ref())
    }

    pub(super) fn file_search_popup(&self, light: bool, cx: &mut Context<Self>) -> Div {
        let muted = rgb(if light { 0x59636e } else { 0x9198a1 });
        let rows = self.file_search.results.clone();
        let selected = self.file_search.selected;
        let generation = self.file_search.generation;
        let entity = cx.entity();
        let list = uniform_list("quick-open-results", rows.len(), move |range, _, _| {
            range
                .map(|index| {
                    let hit = &rows[index];
                    let path = hit.display_path();
                    let content = hit.line().is_some();
                    let filename = path.rsplit('/').next().unwrap_or(path);
                    let title = match hit.line() {
                        Some(line) => format!("{filename}:{line}"),
                        None => filename.to_owned(),
                    };
                    let target = entity.clone();
                    let mut row = div()
                        .id(("quick-open-result", index))
                        .test_support()
                        .role(Role::Button)
                        .aria_label(match hit.line() {
                            Some(line) => format!("内容匹配 {path} 第{line}行"),
                            None => format!("路径匹配 {path}"),
                        })
                        .h(px(72.))
                        .px(px(14.))
                        .py(px(4.))
                        .cursor_pointer()
                        .overflow_hidden()
                        .when(selected == index, |this| {
                            this.bg(rgb(if light { 0xddf4ff } else { 0x1a3453 }))
                        })
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(rgb(if content { 0xb97800 } else { 0x2f81f7 }))
                                        .child(if content { "内容" } else { "路径" }),
                                )
                                .child(div().truncate().child(highlighted(
                                    &title,
                                    if content {
                                        Vec::new()
                                    } else {
                                        let offset = path.len().saturating_sub(filename.len());
                                        hit.highlights()
                                            .into_iter()
                                            .filter_map(|range| {
                                                (range.end > offset).then_some(
                                                    range.start.saturating_sub(offset)
                                                        ..range.end.saturating_sub(offset),
                                                )
                                            })
                                            .collect()
                                    },
                                    light,
                                ))),
                        )
                        .child(div().text_size(px(10.)).text_color(muted).truncate().child(
                            highlighted(
                                path,
                                if content {
                                    Vec::new()
                                } else {
                                    hit.highlights()
                                },
                                light,
                            ),
                        ));
                    if let Some(hit) = hit.content() {
                        row = row.child(div().text_size(px(11.)).truncate().child(highlighted(
                            &hit.snippet,
                            hit.highlights.clone(),
                            light,
                        )));
                    }
                    row.on_click(move |_, window, cx| {
                        target.update(cx, |view, cx| {
                            if view.file_search.generation != generation {
                                return;
                            }
                            view.file_search.selected = index;
                            view.accept_file_search(window, cx);
                        })
                    })
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.file_search.scroll)
        .h(px(360.))
        .w_full();
        let mut modes = div().flex().gap(px(6.)).py(px(7.));
        for (id, mode, label) in [
            ("search-all", SearchMode::All, "全部"),
            ("search-paths", SearchMode::Paths, "文件路径"),
            ("search-contents", SearchMode::Contents, "文件内容"),
        ] {
            modes = modes.child(
                Button::new(id)
                    .label(label)
                    .when(self.file_search.mode == mode, |button| button.primary())
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if view.file_search.mode != mode {
                            view.file_search.mode = mode;
                            view.query_files(view.file_search.query.clone(), cx);
                        }
                    })),
            );
        }
        let options = div()
            .flex()
            .gap(px(12.))
            .py(px(5.))
            .child(
                Checkbox::new("search-regex")
                    .label("正则")
                    .checked(self.file_search.options.regex)
                    .on_click(cx.listener(|view, checked, _, cx| {
                        view.file_search.options.regex = *checked;
                        view.query_files(view.file_search.query.clone(), cx);
                    })),
            )
            .child(
                Checkbox::new("search-case")
                    .label("区分大小写")
                    .checked(self.file_search.options.case_sensitive)
                    .on_click(cx.listener(|view, checked, _, cx| {
                        view.file_search.options.case_sensitive = *checked;
                        view.query_files(view.file_search.query.clone(), cx);
                    })),
            )
            .child(
                Checkbox::new("search-submodules")
                    .label("包含已初始化子模块")
                    .checked(self.file_search.include_submodules)
                    .on_click(cx.listener(|view, checked, _, cx| {
                        view.file_search.include_submodules = *checked;
                        view.file_search.cross_cache = None;
                        view.query_files(view.file_search.query.clone(), cx);
                    })),
            )
            .child(
                Button::new("search-rebuild")
                    .label("刷新范围")
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.file_search.cross_cache = None;
                        view.file_search.cache = None;
                        view.query_files(view.file_search.query.clone(), cx);
                    })),
            );
        let mut filters = div().flex().flex_col().gap(px(5.));
        if let Some(inputs) = &self.file_search.filters {
            for (row, ids) in [
                ["search-directory", "search-types"],
                ["search-include", "search-exclude"],
            ]
            .into_iter()
            .enumerate()
            {
                filters = filters.child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&inputs.fields[row * 2]).id(ids[0]).h(px(30.))),
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                Input::new(&inputs.fields[row * 2 + 1])
                                    .id(ids[1])
                                    .h(px(30.)),
                            ),
                        ),
                );
            }
        }
        let card = div()
            .id("quick-open-card")
            .test_support()
            .w(px(820.))
            .max_w(relative(0.94))
            .p(px(12.))
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(if light { 0xd0d7de } else { 0x3d444d }))
            .bg(rgb(if light { 0xffffff } else { 0x161b22 }))
            .shadow_lg()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "down" => this.move_file_search(1, cx),
                    "up" => this.move_file_search(-1, cx),
                    "escape" => {
                        this.close_file_search(cx);
                        this.focus.focus(window, cx);
                    }
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(
                div()
                    .pb(px(10.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("查找文件和内容 · Ctrl P"),
            )
            .child(Input::new(&self.inputs[Field::Search.index()]).h(px(34.)))
            .child(modes)
            .child(options)
            .child(filters)
            .child(
                div()
                    .py(px(8.))
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(self.file_search.status.clone()),
            )
            .child(list)
            .when(
                self.file_search.results.is_empty() && self.file_search.empty_message.is_some(),
                |this| {
                    this.child(
                        div()
                            .py(px(7.))
                            .text_color(muted)
                            .child(self.file_search.empty_message.unwrap_or_default()),
                    )
                },
            )
            .child(div().pt(px(9.)).text_size(px(11.)).text_color(muted).child(
                "↑↓ 选择 · Enter 打开定位 · Esc 返回 · 多个类型或 Glob 用逗号分隔；点击子模块结果会进入对应仓库",
            ));
        div()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .size_full()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(80.))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.close_file_search(cx);
                    this.focus.focus(window, cx);
                }),
            )
            .child(card)
    }
}

#[cfg(test)]
fn path_match_ranges(path: &str, query: &str) -> Vec<Range<usize>> {
    let query = query.trim().replace('\\', "/").to_ascii_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let folded = path.to_ascii_lowercase();
    let exact = folded
        .match_indices(&query)
        .map(|(start, _)| start..start + query.len())
        .collect::<Vec<_>>();
    if !exact.is_empty() {
        return exact;
    }
    // The path index also supports subsequences. Map those characters back to
    // original UTF-8 byte ranges without splitting multi-byte characters.
    let mut chars = folded.char_indices();
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for expected in query.chars() {
        let Some((start, value)) = chars.find(|(_, value)| *value == expected) else {
            return Vec::new();
        };
        let end = start + value.len_utf8();
        if let Some(previous) = ranges.last_mut().filter(|previous| previous.end == start) {
            previous.end = end;
        } else {
            ranges.push(start..end);
        }
    }
    ranges
}

fn highlighted(text: &str, ranges: Vec<Range<usize>>, light: bool) -> StyledText {
    let style = HighlightStyle {
        background_color: Some(rgb(if light { 0xffe69c } else { 0x75530c }).into()),
        color: Some(rgb(if light { 0x4d2d00 } else { 0xffec99 }).into()),
        font_weight: Some(FontWeight::SEMIBOLD),
        ..Default::default()
    };
    StyledText::new(text.to_owned()).with_highlights(ranges.into_iter().map(|range| (range, style)))
}

#[cfg(test)]
mod tests {
    use super::{RepositoryTree, ResultScope, path_match_ranges};
    use std::sync::Arc;

    #[test]
    fn search_result_is_rejected_after_reload_tree_replacement_or_scope_change() {
        let tree = Arc::new(RepositoryTree::default());
        let scope = ResultScope {
            root: "parent/child".into(),
            repository_id: 7,
            scope_id: 2,
            tree: Arc::downgrade(&tree),
        };
        let root = std::path::Path::new("parent/child");
        assert!(scope.matches(root, 7, 2, &tree.clone()));
        assert!(!scope.matches(root, 8, 2, &tree));
        assert!(!scope.matches(root, 7, 3, &tree));
        assert!(!scope.matches(std::path::Path::new("parent"), 7, 2, &tree));
        assert!(!scope.matches(root, 7, 2, &Arc::new(RepositoryTree::default())));
        drop(tree);
        assert!(!scope.matches(root, 7, 2, &Arc::new(RepositoryTree::default())));
    }

    #[test]
    fn path_highlights_handle_unicode_windows_separators_and_fuzzy_matches() {
        let path = "子模块/😀/FileFile.rs";
        let ranges = path_match_ranges(path, "file");
        assert_eq!(
            ranges.iter().map(|r| &path[r.clone()]).collect::<Vec<_>>(),
            ["File", "File"]
        );
        let ranges = path_match_ranges(path, "子模块\\😀");
        assert_eq!(&path[ranges[0].clone()], "子模块/😀");
        assert_eq!(
            path_match_ranges("src/search.rs", "ssr"),
            [0..1, 4..5, 7..8]
        );
        assert!(path_match_ranges(path, "不存在").is_empty());
        assert!(path_match_ranges(path, "").is_empty());
    }
}
