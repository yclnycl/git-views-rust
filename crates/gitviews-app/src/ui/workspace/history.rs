use super::*;
use crate::ui::copy_menu::{self, CopyItem, CopyMenuExt as _};
use gitviews_service::{GraphHistoryFilter, GraphHistoryRequest, GraphHistorySnapshot};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

mod controls;
mod graph;
mod graph_view;
#[cfg(test)]
mod headless_tests;
mod model;
use model::{display_text, format_utc, message_ranges, short_id, status_label};

const PAGE_SIZE: usize = 100;
const MAX_LOADED_COMMITS: usize = 5_000;
const MAX_LOADED_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default)]
pub(super) struct HistoryState {
    input: Option<Entity<InputState>>,
    subscriptions: Vec<Subscription>,
    author_input: Option<Entity<InputState>>,
    path_input: Option<Entity<InputState>>,
    since_input: Option<Entity<InputState>>,
    until_input: Option<Entity<InputState>>,
    filter: GraphHistoryFilter,
    filter_dirty: bool,
    snapshot: Option<GraphHistorySnapshot>,
    graph_rows: Vec<graph::GraphRow>,
    layout: graph::Layout,
    next_offset: usize,
    cancel: Arc<AtomicBool>,
    scroll: UniformListScrollHandle,
    references: Vec<String>,
    draft_refs: Vec<String>,
    ref_menu: bool,
    ref_menu_id: u64,
    details_collapsed: bool,
    indices: Arc<Vec<usize>>,
    root: Option<PathBuf>,
    bytes: usize,
    limited: bool,
    head_changed: bool,
    page_error: Option<String>,
    detail_error: Option<String>,
    selected_id: Option<String>,
    detail_loading: bool,
    message_lines: Arc<Vec<Range<usize>>>,
    preview_placeholder: Option<String>,
}

impl Drop for HistoryState {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

impl Workspace {
    pub(super) fn pause_history_graph(&mut self) {
        self.history_state.cancel.store(true, Ordering::Release);
        self.dismiss_history_reference_menu();
    }

    pub(super) fn dismiss_history_reference_menu(&mut self) {
        if self.history_state.ref_menu {
            self.history_state.ref_menu = false;
            self.history_state.ref_menu_id = self.history_state.ref_menu_id.wrapping_add(1);
        }
    }

    pub(super) fn init_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("文本或哈希 · Enter 筛选"));
        self.history_state.subscriptions.push(cx.subscribe(
            &input,
            |this, _, event, cx| match event {
                InputEvent::Change => {
                    this.history_state.filter_dirty = true;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } if this.history => {
                    this.apply_history_filter(cx);
                }
                _ => {}
            },
        ));
        self.history_state.input = Some(input);
        self.init_history_filters(window, cx);
    }

    /// Reset repository-bound state while preserving the visible search text.
    pub(super) fn reset_history(&mut self, cx: &mut Context<Self>) {
        self.history_request_id = self.history_request_id.wrapping_add(1);
        self.history_state.cancel.store(true, Ordering::Release);
        self.commit_request_id = self.commit_request_id.wrapping_add(1);
        self.history_task = None;
        self.commit_task = None;
        self.history_loading = false;
        self.history_more = false;
        self.history_commits.clear();
        self.selected_commit = None;
        self.commit_file = None;
        self.history_state.indices = Arc::default();
        self.history_state.root = None;
        self.history_state.snapshot = None;
        self.history_state.graph_rows.clear();
        self.history_state.layout = graph::Layout::default();
        self.history_state.next_offset = 0;
        self.history_state.references.clear();
        self.history_state.draft_refs.clear();
        self.history_state.ref_menu = false;
        self.history_state.bytes = 0;
        self.history_state.limited = false;
        self.history_state.head_changed = false;
        self.history_state.page_error = None;
        self.history_state.detail_error = None;
        self.history_state.selected_id = None;
        self.history_state.detail_loading = false;
        self.history_state.message_lines = Arc::default();
        self.history_state.preview_placeholder = None;
        cx.notify();
    }

    /// New commits do not move the frozen tips of an already opened graph.
    pub(super) fn history_head_changed(&mut self, cx: &mut Context<Self>) {
        self.history_state.head_changed = true;
        cx.notify();
    }

    pub(super) fn toggle_history(&mut self, cx: &mut Context<Self>) {
        self.history = !self.history;
        if self.history {
            // Workspace-tab activation may have cancelled a metadata worker
            // while leaving the remembered selection intact. A new opening
            // always owns a fresh request, independently of its page load.
            self.commit_request_id = self.commit_request_id.wrapping_add(1);
            self.commit_task = None;
            self.history_state.detail_loading = false;
            let same_root = self.changes_repository().map(|repo| &repo.root)
                == self.history_state.root.as_ref();
            let selection = same_root
                .then(|| self.history_state.selected_id.clone())
                .flatten()
                .or_else(|| same_root.then(|| self.first_history_match()).flatten());
            if !same_root {
                self.history_state.selected_id = None;
                self.selected_commit = None;
                self.commit_file = None;
                self.history_state.detail_error = None;
                self.history_state.preview_placeholder = Some("正在读取提交历史…".into());
            }
            if !same_root || self.history_commits.is_empty() {
                self.load_history(true, cx);
            }
            if let Some(id) = selection {
                self.select_commit(id, cx);
            }
        } else {
            // A completed worker from a closed history session must not publish
            // into a subsequent opening, even when the repository is identical.
            self.history_request_id = self.history_request_id.wrapping_add(1);
            self.history_state.cancel.store(true, Ordering::Release);
            self.history_state.ref_menu = false;
            self.commit_request_id = self.commit_request_id.wrapping_add(1);
            self.history_task = None;
            self.commit_task = None;
            self.history_loading = false;
            self.history_state.detail_loading = false;
            self.exit_history_preview(cx);
        }
        cx.notify();
    }

    pub(super) fn load_history(&mut self, reset: bool, cx: &mut Context<Self>) {
        if !self.history || (!reset && self.history_loading) {
            return;
        }
        let Some(root) = self.changes_repository().map(|repo| repo.root.clone()) else {
            return;
        };
        if self.history_state.root.as_ref() != Some(&root) {
            self.history_state.references.clear();
            self.history_state.draft_refs.clear();
            self.history_state.ref_menu = false;
        }
        if reset {
            self.history_state.cancel.store(true, Ordering::Release);
            self.history_task = None;
            self.history_loading = false;
            self.history_more = false;
            self.history_commits.clear();
            self.history_state.indices = Arc::default();
            self.history_state.root = Some(root.clone());
            self.history_state.snapshot = None;
            self.history_state.graph_rows.clear();
            self.history_state.layout = graph::Layout::default();
            self.history_state.next_offset = 0;
            self.history_state
                .scroll
                .scroll_to_item(0, ScrollStrategy::Top);
            self.history_state.bytes = 0;
            self.history_state.limited = false;
            self.history_state.head_changed = false;
            self.commit_request_id = self.commit_request_id.wrapping_add(1);
            self.commit_task = None;
            self.selected_commit = None;
            self.commit_file = None;
            self.history_state.selected_id = None;
            self.history_state.detail_error = None;
            self.history_state.detail_loading = false;
            self.history_state.preview_placeholder = Some("正在读取多分支提交图…".into());
        } else if self.history_state.root.as_ref() != Some(&root)
            || self.history_state.snapshot.is_none()
            || self.history_state.limited
            || !self.history_more
        {
            return;
        }
        let limit = PAGE_SIZE.min(MAX_LOADED_COMMITS.saturating_sub(self.history_commits.len()));
        if limit == 0 {
            self.history_state.limited = true;
            self.history_more = false;
            return;
        }
        let snapshot = self.history_state.snapshot.clone();
        let request = GraphHistoryRequest {
            snapshot: snapshot.clone(),
            references: self.history_state.references.clone(),
            offset: self.history_state.next_offset,
            limit,
            filter: self.history_state.filter.clone(),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.history_state.cancel.store(true, Ordering::Release);
        self.history_state.cancel = Arc::clone(&cancel);
        let service = Arc::clone(&self.service);
        let repository_id = self.request_id;
        self.history_request_id = self.history_request_id.wrapping_add(1);
        let request_id = self.history_request_id;
        self.history_loading = true;
        self.history_state.page_error = None;
        let background_root = root.clone();
        self.history_task = Some(cx.spawn(async move |this, cx| {
            let result = tasks::spawn(
                cx.background_executor(),
                "history.graph",
                request_id,
                move || service.graph_history(&background_root, &request, &cancel),
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.history_request_id != request_id
                    || view.request_id != repository_id
                    || !view.history
                    || view.changes_repository().map(|repo| &repo.root) != Some(&root)
                    || (!reset && view.history_state.snapshot != snapshot)
                {
                    return;
                }
                view.history_loading = false;
                match result {
                    Ok(page) => {
                        let bytes = page
                            .rows
                            .iter()
                            .map(|row| {
                                row.commit.id.len()
                                    + row.commit.author.len()
                                    + row.commit.subject.len()
                                    + row.commit.parents.iter().map(String::len).sum::<usize>()
                                    + 128
                            })
                            .sum::<usize>();
                        if view.history_commits.len() + page.rows.len() > MAX_LOADED_COMMITS
                            || view.history_state.bytes.saturating_add(bytes) > MAX_LOADED_BYTES
                        {
                            view.history_state.limited = true;
                            view.history_more = false;
                        } else {
                            view.history_state.snapshot = Some(page.snapshot);
                            view.history_state.next_offset = page.next_offset;
                            view.history_state.bytes += bytes;
                            for row in page.rows {
                                view.history_commits.push(CommitSummary {
                                    id: row.commit.id.clone(),
                                    subject: row.commit.subject.clone(),
                                    author: row.commit.author.clone(),
                                });
                                let graph_row = view
                                    .history_state
                                    .layout
                                    .push(row.commit, row.matches_filter);
                                view.history_state.graph_rows.push(graph_row);
                            }
                            view.history_more = page.has_more;
                            if (view.history_commits.len() == MAX_LOADED_COMMITS && page.has_more)
                                || (page.truncated && !page.has_more)
                            {
                                view.history_state.limited = true;
                                view.history_more = false;
                            }
                            view.reindex_history();
                            if view.history_state.selected_id.is_none()
                                && let Some(id) = view.first_history_match()
                            {
                                view.select_commit(id, cx);
                            } else if view.history_state.selected_id.is_none() {
                                view.history_state.preview_placeholder = Some(
                                    if view.history_commits.is_empty() {
                                        "当前分支范围尚无提交。"
                                    } else {
                                        "已加载记录中没有匹配提交；可调整筛选或继续加载。"
                                    }
                                    .into(),
                                );
                            }
                        }
                    }
                    Err(error) => {
                        if view.history_state.selected_id.is_none() {
                            view.history_state.preview_placeholder =
                                Some(format!("读取提交图失败：{error}"));
                        }
                        view.history_state.page_error = Some(error.to_string());
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn reindex_history(&mut self) {
        self.history_state.indices = Arc::new(
            self.history_state
                .graph_rows
                .iter()
                .enumerate()
                .filter(|(_, row)| row.matches)
                .map(|(index, _)| index)
                .collect(),
        );
    }
    fn first_history_match(&self) -> Option<String> {
        self.history_state
            .indices
            .first()
            .and_then(|index| self.history_commits.get(*index))
            .map(|commit| commit.id.clone())
    }

    fn select_commit(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.history {
            return;
        }
        self.request_history_commit(id, None, true, cx);
    }

    /// A tab already owns its preview request. Restore only the matching
    /// metadata here; do not open its first file or supersede that tab request.
    pub(super) fn activate_history_file(
        &mut self,
        path: String,
        id: String,
        cx: &mut Context<Self>,
    ) {
        self.history = true;
        self.history_state.preview_placeholder = None;
        if self.history_commits.is_empty() && !self.history_loading {
            self.load_history(true, cx);
        }
        if self
            .selected_commit
            .as_ref()
            .is_some_and(|commit| commit.id == id)
        {
            self.commit_request_id = self.commit_request_id.wrapping_add(1);
            self.commit_task = None;
            self.history_state.selected_id = Some(id);
            self.history_state.detail_loading = false;
            self.commit_file = Some(path);
            cx.notify();
        } else {
            self.request_history_commit(id, Some(path), false, cx);
        }
    }

    fn request_history_commit(
        &mut self,
        id: String,
        preferred_file: Option<String>,
        open_first_file: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.changes_repository() else {
            return;
        };
        let root = repo.root.clone();
        let service = Arc::clone(&self.service);
        self.commit_request_id = self.commit_request_id.wrapping_add(1);
        let request_id = self.commit_request_id;
        let repository_id = self.request_id;
        self.history_state.selected_id = Some(id.clone());
        self.history_state.detail_loading = true;
        self.history_state.detail_error = None;
        self.history_state.message_lines = Arc::default();
        self.selected_commit = None;
        self.commit_file = preferred_file;
        self.history_state.preview_placeholder =
            open_first_file.then(|| "正在读取提交详情…".to_owned());
        let background_root = root.clone();
        let background_id = id.clone();
        self.commit_task = Some(cx.spawn(async move |this, cx| {
            let result = tasks::spawn(
                cx.background_executor(),
                "history.commit",
                request_id,
                move || {
                    service.validate_root(&background_root)?;
                    let commit = service.commit(&background_root, &background_id)?;
                    service.validate_root(&background_root)?;
                    let lines = message_ranges(&commit.message);
                    Ok::<_, gitviews_service::ServiceError>((commit, lines))
                },
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.commit_request_id != request_id
                    || view.request_id != repository_id
                    || !view.history
                    || view.changes_repository().map(|repo| &repo.root) != Some(&root)
                    || view.history_state.selected_id.as_ref() != Some(&id)
                {
                    return;
                }
                view.history_state.detail_loading = false;
                match result {
                    Ok((commit, message_lines)) => {
                        let first = commit.changes.first().map(|change| change.path.clone());
                        view.history_state.message_lines = Arc::new(message_lines);
                        view.selected_commit = Some(commit);
                        if open_first_file {
                            if let Some(path) = first {
                                view.history_state.preview_placeholder = None;
                                view.select_commit_file(path, cx);
                            } else {
                                view.history_state.preview_placeholder =
                                    Some("此提交相对第一父提交或空树没有文件变化。".into());
                            }
                        }
                    }
                    Err(error) => {
                        view.history_state.detail_error = Some(error.to_string());
                        if open_first_file {
                            view.history_state.preview_placeholder = Some(error.to_string());
                        }
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn select_commit_file(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(commit) = &self.selected_commit else {
            return;
        };
        let id = commit.id.clone();
        self.commit_file = Some(path.clone());
        self.history_state.preview_placeholder = None;
        self.open_history_file(path.clone(), id.clone(), cx);
        if self
            .active_preview_key()
            .is_none_or(|key| key.path != path || key.revision.as_deref() != Some(id.as_str()))
        {
            // A full tab deck may refuse a new key. Keep the previous pane
            // retained without displaying it underneath unrelated metadata.
            self.history_state.preview_placeholder =
                Some("未能打开此文件的预览标签；请关闭一个标签后重试。".into());
        }
        cx.notify();
    }

    pub(super) fn render_history(&self, cx: &mut Context<Self>) -> AnyElement {
        let light = cx.theme().mode == ThemeMode::Light;
        let border = rgb(if light { 0xd0d7de } else { 0x30363d });
        let muted = rgb(if light { 0x59636e } else { 0x9198a1 });
        let selected_bg = rgb(if light { 0xddf4ff } else { 0x1a3453 });
        let preview = if let Some(message) = &self.history_state.preview_placeholder {
            div()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .p(px(20.))
                .text_color(muted)
                .child(message.clone())
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(self.preview_pane.clone())
                .into_any_element()
        };
        let mut right = div()
            .w(px(340.))
            .flex_shrink_0()
            .min_h_0()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(border);
        if let Some(commit) = &self.selected_commit {
            let mut parents = div()
                .flex()
                .items_center()
                .gap(px(9.))
                .overflow_hidden()
                .text_size(px(10.))
                .text_color(muted);
            if commit.parent_ids.is_empty() {
                parents = parents.child("初始提交 · 与空树比较");
            } else {
                parents = parents.child(if commit.parent_ids.len() > 1 {
                    "Merge · 相对第 1 父比较"
                } else {
                    "父提交"
                });
                for (index, id) in commit.parent_ids.iter().take(8).enumerate() {
                    let id = id.clone();
                    let copy_items = vec![CopyItem::new("复制提交 ID", &id)];
                    parents =
                        parents.child(
                            div()
                                .id(("history-parent", index))
                                .role(Role::Button)
                                .aria_label(format!("查看父提交 {id}"))
                                .cursor_pointer()
                                .child(short_id(&id).to_owned())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.select_commit(id.clone(), cx)
                                }))
                                .copy_menu(copy_items),
                        );
                }
                if commit.parent_ids.len() > 8 {
                    parents = parents.child(format!("共 {} 个父提交", commit.parent_ids.len()));
                }
            }
            let message_entity = cx.entity();
            let message_lines = Arc::clone(&self.history_state.message_lines);
            let message = uniform_list(
                "history-message",
                message_lines.len(),
                move |range, _, cx| {
                    let view = message_entity.read(cx);
                    range
                        .filter_map(|index| {
                            let commit = view.selected_commit.as_ref()?;
                            let text = commit.message.get(message_lines.get(index)?.clone())?;
                            let text = text.trim_end_matches(['\r', '\n']);
                            Some(
                                div()
                                    .h(px(18.))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_size(px(11.))
                                    .child(display_text(text, 16_000)),
                            )
                        })
                        .collect::<Vec<_>>()
                },
            )
            .size_full();
            let files_entity = cx.entity();
            let changes =
                uniform_list("commit-files", commit.changes.len(), move |range, _, cx| {
                    let view = files_entity.read(cx);
                    range
                        .filter_map(|index| {
                            let change = view.selected_commit.as_ref()?.changes.get(index)?;
                            let path = change.path.clone();
                            let target = files_entity.clone();
                            let copy_items = view
                                .changes_repository()
                                .map(|repo| copy_menu::file(&repo.root, &path))
                                .unwrap_or_default();
                            Some(
                                div()
                                    .id(("commit-file", index))
                                    .role(Role::Button)
                                    .aria_label(format!("提交文件 {path}"))
                                    .h(px(28.))
                                    .px(px(12.))
                                    .cursor_pointer()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .when(view.commit_file.as_ref() == Some(&path), |this| {
                                        this.bg(selected_bg)
                                    })
                                    .child(format!(
                                        "{}   {path}",
                                        status_label(change.index_status)
                                    ))
                                    .on_click(move |_, _, cx| {
                                        target.update(cx, |this, cx| {
                                            this.select_commit_file(path.clone(), cx)
                                        })
                                    })
                                    .copy_menu(copy_items),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .size_full();
            right = right
                .child(
                    div()
                        .px(px(14.))
                        .py(px(9.))
                        .border_b_1()
                        .border_color(border)
                        .child(
                            div()
                                .overflow_hidden()
                                .text_ellipsis()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(display_text(&commit.subject, 2_000)),
                        )
                        .child(
                            div()
                                .id("history-commit-id")
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_color(muted)
                                .text_size(px(10.))
                                .child(commit.id.clone())
                                .copy_menu(vec![CopyItem::new("复制提交 ID", &commit.id)]),
                        )
                        .child(div().text_color(muted).text_size(px(11.)).child(format!(
                            "{} · {}",
                            display_text(&commit.author, 512),
                            format_utc(commit.time)
                        )))
                        .child(parents),
                )
                .child(
                    div()
                        .h(px(82.))
                        .flex_shrink_0()
                        .px(px(14.))
                        .py(px(5.))
                        .overflow_hidden()
                        .border_b_1()
                        .border_color(border)
                        .child(message),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .border_b_1()
                        .border_color(border)
                        .child(changes),
                );
        } else {
            let message = self.history_state.detail_error.clone().unwrap_or_else(|| {
                if self.history_state.detail_loading {
                    "正在读取提交详情…"
                } else if self.history_loading {
                    "正在读取提交历史…"
                } else {
                    "选择一次提交查看元信息、文件与差异"
                }
                .to_owned()
            });
            right = right.child(div().p(px(20.)).text_color(muted).child(message));
        }

        let mut preview_tools = div()
            .h(px(34.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(18.))
            .px(px(12.))
            .border_b_1()
            .border_color(border)
            .text_size(px(11.));
        for (id, label, mode) in [
            ("history-source", "源码", 0),
            ("history-split", "双栏 Diff", 1),
            ("history-unified", "单栏 Diff", 2),
        ] {
            preview_tools = preview_tools.child(
                div()
                    .id(id)
                    .test_support()
                    .role(Role::Button)
                    .aria_label(label)
                    .cursor_pointer()
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.history_state.preview_placeholder.is_some() {
                            return;
                        }
                        this.source = mode == 0;
                        this.unified = mode == 2;
                        this.sync_preview(cx);
                        cx.notify();
                    })),
            );
        }
        let preview_right = div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(preview_tools)
            .child(self.render_preview_tabs(cx))
            .child(preview);
        div()
            .id("history-view")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && this.history_state.ref_menu {
                    this.dismiss_history_reference_menu();
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.render_history_controls(cx))
            .child(self.render_history_graph(cx))
            .when(!self.history_state.details_collapsed, |this| {
                this.child(
                    div()
                        .h(relative(0.48))
                        .min_h(px(190.))
                        .flex_shrink_0()
                        .min_w_0()
                        .flex()
                        .child(right)
                        .child(preview_right),
                )
            })
            .when_some(self.history_reference_menu(cx), |this, menu| {
                this.child(menu)
            })
            .into_any_element()
    }
}
