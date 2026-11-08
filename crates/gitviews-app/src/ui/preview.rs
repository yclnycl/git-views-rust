//! Persistent, readonly source and diff controls from GPUI Kit 0.8.1.
//!
//! Repository I/O belongs to the service. This view owns only retained editor,
//! diff, selection and search state. It never writes a source file.

use std::sync::Arc;

use gitviews_service::{DiffLineKind, FileDiff, SourceFile};
use gpui_kit::{
    component::{
        ActiveTheme,
        diff::{Diff, DiffFile, DiffLinePosition, DiffLineRange, DiffMode, DiffSide, DiffState},
        input::{Copy, Editor, EditorState, Input, InputEvent, InputState, Position, SelectAll},
        menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    },
    *,
};

use super::syntax::language_for_source;

gpui_kit::actions!(
    gitviews_preview,
    [
        FindInPreview,
        NextPreviewMatch,
        PreviousPreviewMatch,
        ClosePreviewSearch,
        FileHistoryInPreview
    ]
);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PreviewMode {
    Source,
    #[default]
    Split,
    Unified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PreviewPaneEvent {
    FileHistory,
}

struct PreviewBindings;
impl Global for PreviewBindings {}

#[derive(Clone, Copy)]
pub(crate) struct PreviewPreferences {
    pub font_size: f32,
}
impl Default for PreviewPreferences {
    fn default() -> Self {
        Self { font_size: 12.5 }
    }
}
impl Global for PreviewPreferences {}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    Empty,
    Loading,
    Ready,
    Error(String),
}

/// Retain one entity per cached tab. The source editor and diff keep independent
/// reading/selection/search state; changing modes never replaces their content.
pub struct PreviewPane {
    editor: Option<Entity<EditorState>>,
    diff: Entity<DiffState>,
    search: Option<Entity<InputState>>,
    source: Option<Arc<SourceFile>>,
    diff_content: Option<Arc<FileDiff>>,
    status: Status,
    diff_ready: bool,
    diff_error: Option<String>,
    mode: PreviewMode,
    ignore_whitespace: bool,
    editor_pending: bool,
    editor_initialized: bool,
    editor_reserved_bytes: usize,
    editor_resident_bytes: usize,
    editor_line_pending: Option<usize>,
    source_navigation: Option<usize>,
    source_navigation_ready: bool,
    search_open: bool,
    search_hits: Vec<DiffLinePosition>,
    search_index: usize,
    change_index: Option<usize>,
    generation: u64,
    parse_task: Option<Task<()>>,
    parse_requested: bool,
    parse_resident_bytes: usize,
    history_available: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PreviewPaneEvent> for PreviewPane {}

impl PreviewPane {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut pane = Self::new_deferred(cx);
        pane.ensure_controls(window, cx);
        pane
    }

    /// Creating a cached tab does not require a Window. Window-dependent input
    /// states are allocated only when this pane first renders or receives Find.
    pub fn new_deferred(cx: &mut Context<Self>) -> Self {
        let diff = cx.new(|cx| DiffState::new(Vec::new(), cx).with_mode(DiffMode::Split));
        let subscriptions = vec![cx.observe(&diff, |_, _, cx| cx.notify())];
        if !cx.has_global::<PreviewBindings>() {
            cx.set_global(PreviewBindings);
            cx.bind_keys([
                KeyBinding::new("ctrl-f", FindInPreview, Some("GitViewsPreview")),
                KeyBinding::new("f3", NextPreviewMatch, Some("GitViewsPreview")),
                KeyBinding::new("shift-f3", PreviousPreviewMatch, Some("GitViewsPreview")),
                KeyBinding::new("escape", ClosePreviewSearch, Some("GitViewsPreview")),
            ]);
        }
        Self {
            editor: None,
            diff,
            search: None,
            source: None,
            diff_content: None,
            status: Status::Empty,
            diff_ready: false,
            diff_error: None,
            mode: PreviewMode::Split,
            ignore_whitespace: false,
            editor_pending: false,
            editor_initialized: false,
            editor_reserved_bytes: 0,
            editor_resident_bytes: 0,
            editor_line_pending: None,
            source_navigation: None,
            source_navigation_ready: false,
            search_open: false,
            search_hits: Vec::new(),
            search_index: 0,
            change_index: None,
            generation: 0,
            parse_task: None,
            parse_requested: false,
            parse_resident_bytes: 0,
            history_available: false,
            _subscriptions: subscriptions,
        }
    }

    fn ensure_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.is_none() {
            self.editor = Some(cx.new(|cx| {
                let mut editor = EditorState::new(window, cx)
                    .soft_wrap(false)
                    .context_menu(false);
                editor.set_readonly(true, cx);
                editor
            }));
        }
        if self.search.is_none() {
            let search = cx.new(|cx| InputState::new(window, cx).placeholder("查找差异内容…"));
            self._subscriptions
                .push(
                    cx.subscribe(&search, |this, _, event: &InputEvent, cx| match event {
                        InputEvent::Change => this.update_search(cx),
                        InputEvent::PressEnter { shift, .. } => this.move_match(!shift, cx),
                        _ => {}
                    }),
                );
            self.search = Some(search);
        }
    }

    pub fn mode(&self) -> PreviewMode {
        self.mode
    }

    /// The workspace supplies this capability from the pane's exact tab key.
    /// A payload alone is not authority to guess a repository or file target.
    pub(crate) fn set_history_available(&mut self, available: bool, cx: &mut Context<Self>) {
        if self.history_available != available {
            self.history_available = available;
            cx.notify();
        }
    }

    fn can_show_file_history(&self) -> bool {
        self.history_available && self.has_content() && matches!(self.status, Status::Ready)
    }

    fn show_file_history(&mut self, cx: &mut Context<Self>) {
        // Recheck when the menu dispatches: a read could have failed or
        // this cached pane could have been invalidated while the menu was open.
        if self.can_show_file_history() {
            cx.emit(PreviewPaneEvent::FileHistory);
        }
    }

    pub fn ignore_whitespace(&self) -> bool {
        self.ignore_whitespace
    }

    pub fn source(&self) -> Option<Arc<SourceFile>> {
        self.source.clone()
    }

    pub fn file_diff(&self) -> Option<Arc<FileDiff>> {
        self.diff_content.clone()
    }

    /// Conservative retained-content estimate, including editor rope and parsed
    /// diff/search derivatives. This is a cache budget, not a process RSS cap.
    pub fn estimated_bytes(&self) -> usize {
        estimated_content_bytes(self.source.as_deref(), self.diff_content.as_deref())
            // A deferred replacement may still retain the previous editor
            // rope until this pane renders. Reserve the larger of the pending
            // and resident rope, so applying the first render cannot increase
            // the budget and an old large rope is still fully accounted for.
            .saturating_add(self.editor_reserved_bytes.max(self.editor_resident_bytes))
            .saturating_add(self.parse_resident_bytes)
    }

    /// Replace both read results for the same tab identity. A missing side must
    /// not leave stale source or diff data available after deletion or decoding
    /// failure. An unchanged, still-valid side keeps its exact control state.
    pub fn set_payloads(
        &mut self,
        source: Option<Arc<SourceFile>>,
        diff: Option<Arc<FileDiff>>,
        cx: &mut Context<Self>,
    ) {
        self.source_navigation_ready = source.is_some();
        if source.is_none() && diff.is_none() {
            self.clear(cx);
            return;
        }
        if source.is_none() && self.source.take().is_some() {
            self.editor = None;
            self.editor_initialized = false;
            self.editor_resident_bytes = 0;
            self.editor_pending = true;
            self.editor_line_pending = None;
        }
        if diff.is_none() && self.diff_content.take().is_some() {
            self.cancel_preparation();
            self.diff_ready = false;
            self.diff_error = None;
            self.search_hits.clear();
            self.search_index = 0;
            self.change_index = None;
            self.diff
                .update(cx, |state, cx| state.set_files(Vec::new(), cx));
        }
        if let Some(source) = source {
            self.set_source(source, cx);
        }
        if let Some(diff) = diff {
            self.set_diff(diff, cx);
        }
        self.refresh_content_status();
        cx.notify();
    }

    /// No Window is needed here: the editor applies this payload once during
    /// its next render, where GPUI makes the window available.
    pub fn set_source(&mut self, source: Arc<SourceFile>, cx: &mut Context<Self>) {
        if let Some(previous) = &self.source
            && (Arc::ptr_eq(previous, &source)
                || (previous.path == source.path && previous.text == source.text))
        {
            // Metadata such as encoding may change without changing the editor.
            self.source = Some(source);
            self.refresh_editor_reservation();
            self.refresh_content_status();
            cx.notify();
            return;
        }
        self.source = Some(source);
        self.refresh_editor_reservation();
        if self.is_source() {
            self.status = Status::Ready;
        }
        self.editor_pending = true;
        self.editor_line_pending = None;
        cx.notify();
    }

    /// Search navigation can arrive before asynchronous source loading. Keep
    /// it separate from editor refresh state until the latest read is applied;
    /// a retained older payload may have a different number of lines.
    pub fn navigate_source_line(&mut self, line: usize, cx: &mut Context<Self>) {
        self.set_mode(PreviewMode::Source, cx);
        self.source_navigation = Some(line.max(1));
        self.source_navigation_ready = false;
        cx.notify();
    }

    pub fn set_diff(&mut self, content: Arc<FileDiff>, cx: &mut Context<Self>) {
        if let Some(previous) = &self.diff_content
            && (Arc::ptr_eq(previous, &content) || previous.as_ref() == content.as_ref())
        {
            self.diff_content = Some(content);
            self.refresh_editor_reservation();
            self.refresh_content_status();
            cx.notify();
            return;
        }
        let editor_changed = self.source.is_none()
            && self.diff_content.as_ref().is_none_or(|previous| {
                previous.path != content.path || previous.new_text != content.new_text
            });
        self.diff_content = Some(content);
        self.refresh_editor_reservation();
        self.editor_pending |= editor_changed;
        self.editor_line_pending = None;
        self.change_index = None;
        self.prepare_diff(cx);
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.reset(Status::Empty, cx);
    }

    pub fn set_loading(&mut self, cx: &mut Context<Self>) {
        // A refresh of the same identity keeps the last successful preview and
        // its reading state. A newly allocated tab has no content to retain.
        self.status = Status::Loading;
        cx.notify();
    }

    pub fn set_error(&mut self, error: String, cx: &mut Context<Self>) {
        if self.has_content() {
            self.status = Status::Error(error);
            cx.notify();
        } else {
            self.reset(Status::Error(error), cx);
        }
    }

    pub fn set_mode(&mut self, mode: PreviewMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.diff.update(cx, |state, cx| {
            state.set_mode(
                if mode == PreviewMode::Unified {
                    DiffMode::Unified
                } else {
                    DiffMode::Split
                },
                cx,
            );
        });
        // A diff preparation error concerns the diff mode only. Read failures
        // from the service remain visible when switching retained controls.
        if !matches!(self.status, Status::Error(_)) || self.diff_error.is_some() {
            self.refresh_content_status();
        }
        cx.notify();
    }

    pub fn set_ignore_whitespace(&mut self, ignore: bool, cx: &mut Context<Self>) {
        if self.ignore_whitespace == ignore {
            return;
        }
        self.ignore_whitespace = ignore;
        if self.diff_content.is_some() {
            self.prepare_diff(cx);
        }
    }

    /// Opens the source editor's native find panel or the diff's line search.
    /// Both operate on decoded UTF-8 text, including Chinese queries.
    pub fn find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_controls(window, cx);
        if self.is_source() {
            self.apply_editor_content(window, cx);
            if let Some(editor) = &self.editor {
                editor.update(cx, |editor, cx| {
                    editor.focus(window, cx);
                    editor.open_search(false, cx);
                });
            }
        } else {
            self.search_open = true;
            if let Some(search) = &self.search {
                search.update(cx, |search, cx| search.focus(window, cx));
            }
            self.update_search(cx);
        }
        cx.notify();
    }

    pub fn next_change(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.is_source() {
            let Some(content) = &self.diff_content else {
                return;
            };
            let positions = content
                .lines
                .iter()
                .filter(|line| line.kind == DiffLineKind::Section && line.text.starts_with("@@"))
                .filter_map(|line| new_hunk_line(&line.text))
                .collect::<Vec<_>>();
            if positions.is_empty() {
                return;
            }
            let index = next_change_index(self.change_index, positions.len(), forward);
            self.change_index = Some(index);
            self.editor_line_pending = Some(positions[index]);
        } else {
            self.diff.update(cx, |state, cx| {
                if forward {
                    state.next_change(cx);
                } else {
                    state.previous_change(cx);
                }
            });
        }
        cx.notify();
    }

    fn reset(&mut self, status: Status, cx: &mut Context<Self>) {
        self.cancel_preparation();
        self.source = None;
        self.diff_content = None;
        self.status = status;
        self.diff_ready = false;
        self.diff_error = None;
        self.editor_pending = true;
        self.editor_initialized = false;
        self.editor_reserved_bytes = 0;
        self.editor_resident_bytes = 0;
        self.editor = None;
        self.search = None;
        self._subscriptions.truncate(1);
        self.editor_line_pending = None;
        self.search_open = false;
        self.source_navigation = None;
        self.source_navigation_ready = false;
        self.search_hits.clear();
        self.search_index = 0;
        self.change_index = None;
        self.diff
            .update(cx, |state, cx| state.set_files(Vec::new(), cx));
        cx.notify();
    }

    fn cancel_preparation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.parse_task = None;
        self.parse_requested = false;
        self.parse_resident_bytes = 0;
    }

    fn prepare_diff(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let Some(content) = &self.diff_content else {
            return;
        };
        self.diff_ready = false;
        self.diff_error = None;
        // Changed patches replace the diff document. Release its previous
        // parsed rows immediately instead of keeping an invisible old copy
        // throughout background parsing; identical payloads never reach here.
        self.diff
            .update(cx, |state, cx| state.set_files(Vec::new(), cx));
        if content.patch.is_empty() {
            self.parse_requested = false;
            self.status = Status::Ready;
            cx.notify();
            return;
        }
        self.refresh_content_status();
        // DiffFile::parse is synchronous once its worker starts. Canceling the
        // awaiting future cannot preempt it, so retain one worker and coalesce
        // repeated mode/content updates into a single latest pending request.
        self.parse_requested = true;
        if self.parse_task.is_none() {
            self.start_diff_preparation(cx);
        }
        cx.notify();
    }

    fn start_diff_preparation(&mut self, cx: &mut Context<Self>) {
        let Some(content) = self.diff_content.clone() else {
            return;
        };
        self.parse_requested = false;
        self.parse_resident_bytes = estimated_content_bytes(None, Some(content.as_ref()));
        let generation = self.generation;
        let ignore = self.ignore_whitespace;
        self.parse_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let started = std::time::Instant::now();
                    let language = language_for_source(&content.path, &content.new_text);
                    let filtered = ignore.then(|| ignore_whitespace_patch(&content.patch));
                    let result = DiffFile::parse(filtered.as_deref().unwrap_or(&content.patch))
                        .map(|files| {
                            files
                                .into_iter()
                                .map(|file| file.with_language(language))
                                .collect::<Vec<_>>()
                        })
                        .map_err(|error| error.to_string());
                    tracing::debug!(
                        target: "gitviews::preview",
                        generation,
                        language,
                        patch_bytes = content.patch.len(),
                        ignore_whitespace = ignore,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        success = result.is_ok(),
                        "diff parsing completed"
                    );
                    result
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.parse_task = None;
                this.parse_resident_bytes = 0;
                if this.generation != generation {
                    tracing::debug!(
                        target: "gitviews::preview",
                        generation,
                        current_generation = this.generation,
                        "discarding stale diff parse"
                    );
                    if this.parse_requested {
                        this.start_diff_preparation(cx);
                    }
                    cx.notify();
                    return;
                }
                match result {
                    Ok(files) => {
                        this.diff.update(cx, |state, cx| state.set_files(files, cx));
                        this.diff_ready = true;
                        this.update_search(cx);
                    }
                    Err(error) => this.diff_error = Some(format!("差异无法显示：{error}")),
                }
                this.refresh_content_status();
                cx.notify();
            });
        }));
    }

    fn is_source(&self) -> bool {
        self.mode == PreviewMode::Source
            || (self.diff_content.is_none() && self.source.is_some())
            || self
                .diff_content
                .as_ref()
                .is_some_and(|content| content.patch.is_empty())
    }

    fn has_content(&self) -> bool {
        self.source.is_some() || self.diff_content.is_some()
    }

    fn refresh_editor_reservation(&mut self) {
        self.editor_reserved_bytes = if let Some(source) = &self.source {
            source
                .text
                .capacity()
                .saturating_mul(2)
                .saturating_add(source.line_count.saturating_mul(16))
        } else if let Some(diff) = &self.diff_content {
            estimated_editor_bytes(&diff.new_text, diff.new_text.capacity())
        } else {
            0
        };
    }

    fn refresh_content_status(&mut self) {
        self.status = if (self.is_source() && self.has_content()) || self.diff_ready {
            Status::Ready
        } else if let Some(error) = &self.diff_error {
            Status::Error(error.clone())
        } else if self.has_content() {
            Status::Loading
        } else {
            Status::Empty
        };
    }

    fn apply_editor_content(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        if self.editor_pending && (self.is_source() || !self.has_content()) {
            let started = std::time::Instant::now();
            let (path, text) = if let Some(source) = &self.source {
                (source.path.as_str(), source.text.as_str())
            } else if let Some(diff) = &self.diff_content {
                (diff.path.as_str(), diff.new_text.as_str())
            } else {
                ("", "")
            };
            let language = language_for_source(path, text);
            let text_bytes = text.len();
            let resident_bytes = estimated_editor_bytes(text, text.len());
            let text = SharedString::from(text.to_owned());
            let preserve = self.editor_initialized;
            let navigating = self.editor_line_pending.is_some();
            editor.update(cx, |editor, cx| {
                let anchor = preserve.then(|| (editor.selected_range(), editor.scroll_offset()));
                if editor.language_name().as_ref() != language {
                    editor.set_highlighter(language, cx);
                }
                editor.set_value(text, window, cx);
                if let Some((selection, offset)) = anchor {
                    editor.set_selected_range(selection, cx);
                    if !navigating {
                        editor.set_scroll_offset(offset, cx);
                    }
                }
            });
            self.editor_pending = false;
            self.editor_initialized = true;
            self.editor_resident_bytes = resident_bytes;
            tracing::debug!(
                target: "gitviews::preview",
                language,
                text_bytes,
                retained_selection = preserve,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "source editor content applied"
            );
        }
        if self.is_source()
            && self.source_navigation_ready
            && let Some(source) = &self.source
            && let Some(line) = self.source_navigation.take()
        {
            self.editor_line_pending = Some(line.min(source.line_count.max(1)));
        }
        if let Some(line) = self.editor_line_pending.take() {
            editor.update(cx, |editor, cx| {
                editor.unfold_at(Position::new(line.saturating_sub(1) as u32, 0), cx);
                editor.set_cursor_position(
                    Position::new(line.saturating_sub(1) as u32, 0),
                    window,
                    cx,
                );
            });
        }
    }

    fn update_search(&mut self, cx: &mut Context<Self>) {
        let query = self
            .search
            .as_ref()
            .map(|search| search.read(cx).value())
            .unwrap_or_default();
        self.search_hits = self
            .diff_content
            .as_ref()
            .map_or_else(Vec::new, |content| find_diff_lines(content, query.as_ref()));
        self.search_index = 0;
        if self.search_open {
            self.reveal_match(cx);
        }
        cx.notify();
    }

    fn move_match(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.is_source() {
            if let Some(editor) = &self.editor {
                editor.update(cx, |editor, cx| {
                    if forward {
                        editor.next_search_match(cx);
                    } else {
                        editor.previous_search_match(cx);
                    }
                });
            }
        } else if !self.search_hits.is_empty() {
            self.search_index = step_index(self.search_index, self.search_hits.len(), forward);
            self.reveal_match(cx);
        }
        cx.notify();
    }

    fn reveal_match(&mut self, cx: &mut Context<Self>) {
        let Some(position) = self.search_hits.get(self.search_index).cloned() else {
            return;
        };
        self.diff.update(cx, |state, cx| {
            state.scroll_to_line(position.clone(), cx);
            state.set_selected_lines(
                Some(DiffLineRange::new(
                    position.path().clone(),
                    position.side(),
                    position.line(),
                    position.line(),
                )),
                cx,
            );
        });
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_open = false;
        if let Some(editor) = &self.editor {
            editor.update(cx, |editor, cx| editor.close_search(cx));
        }
        if self.is_source() {
            if let Some(editor) = &self.editor {
                editor.update(cx, |editor, cx| editor.focus(window, cx));
            }
        } else {
            self.diff.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }
}

impl Render for PreviewPane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let font_size = cx
            .try_global::<PreviewPreferences>()
            .copied()
            .unwrap_or_default()
            .font_size;
        let line_height = font_size * (22.0 / 12.5);
        self.ensure_controls(window, cx);
        self.apply_editor_content(window, cx);
        let mut view = div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .key_context("GitViewsPreview")
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, _: &FindInPreview, window, cx| this.find(window, cx)))
            .on_action(cx.listener(|this, _: &NextPreviewMatch, _, cx| this.move_match(true, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousPreviewMatch, _, cx| this.move_match(false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ClosePreviewSearch, window, cx| {
                    this.close_search(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &FileHistoryInPreview, _, cx| this.show_file_history(cx)),
            );
        if let Status::Error(error) = &self.status
            && ((self.is_source() && self.has_content()) || self.diff_ready)
        {
            view = view.child(
                div()
                    .flex_shrink_0()
                    .px(px(10.))
                    .py(px(6.))
                    .text_size(px(11.))
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("内容刷新失败，保留上次预览：{error}")),
            );
        }
        if self.search_open && !self.is_source() {
            let search = self
                .search
                .as_ref()
                .expect("controls initialized before render");
            view = view.child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .h(px(36.))
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .bg(cx.theme().muted)
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(search).h(px(28.))),
                    )
                    .child(div().text_size(px(11.)).child(format!(
                        "{} / {} 行",
                        if self.search_hits.is_empty() {
                            0
                        } else {
                            self.search_index + 1
                        },
                        self.search_hits.len()
                    )))
                    .child(
                        div()
                            .id("preview-search-previous")
                            .cursor_pointer()
                            .child(
                                svg()
                                    .path("icons/chevron-up.svg")
                                    .size(px(14.))
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.move_match(false, cx))),
                    )
                    .child(
                        div()
                            .id("preview-search-next")
                            .cursor_pointer()
                            .child(
                                svg()
                                    .path("icons/chevron-down.svg")
                                    .size(px(14.))
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.move_match(true, cx))),
                    )
                    .child(
                        div()
                            .id("preview-search-close")
                            .cursor_pointer()
                            .child(
                                svg()
                                    .path("icons/x.svg")
                                    .size(px(14.))
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_search(window, cx)),
                            ),
                    ),
            );
        }
        let body = if self.is_source() && self.has_content() {
            let editor = self
                .editor
                .as_ref()
                .expect("controls initialized before render");
            let menu_editor = editor.clone();
            let pane = cx.entity().downgrade();
            div()
                .id("preview-source-context")
                .test_support()
                .size_full()
                .child(
                    Editor::new(editor)
                        .readonly(true)
                        .appearance(false)
                        .bordered(false)
                        .font_family("JetBrains Mono")
                        .text_size(px(font_size))
                        .line_height(px(line_height))
                        .h(relative(1.))
                        .size_full(),
                )
                .context_menu(move |menu, _, cx| {
                    readonly_preview_menu(
                        menu.action_context(menu_editor.focus_handle(cx)),
                        !menu_editor.read(cx).selected_range().is_empty(),
                        pane.upgrade()
                            .is_some_and(|pane| pane.read(cx).can_show_file_history()),
                    )
                })
                .into_any_element()
        } else if self.diff_ready && self.diff_content.is_some() {
            let menu_diff = self.diff.clone();
            let pane = cx.entity().downgrade();
            div()
                .id("preview-diff-context")
                .test_support()
                .size_full()
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _, window, cx| {
                        // Diff rows own left-click text selection, but do not
                        // install a right-click menu. Keep that selection and
                        // focus the original DiffState so Copy/SelectAll use
                        // its side, range, and character-selection semantics.
                        this.diff.read(cx).focus_handle(cx).focus(window, cx);
                    }),
                )
                .child(
                    Diff::new(&self.diff)
                        .header_visible(false)
                        .line_number(true)
                        .syntax_highlight(true)
                        .font_family("JetBrains Mono")
                        .text_size(px(font_size))
                        .line_height(px(line_height))
                        .size_full(),
                )
                .context_menu(move |menu, _, cx| {
                    let state = menu_diff.clone();
                    let text = state.read(cx).selected_text(cx);
                    let selected_lines = state.read(cx).selected_lines();
                    let generation = pane.upgrade().map(|pane| pane.read(cx).generation);
                    let copy_pane = pane.clone();
                    // Base's window selection clears Diff's selection on the
                    // menu item's left mouse-down. Preserve the control's own
                    // exact selected text now, including character selections
                    // and the original/modified side, before that happens.
                    let copy = PopupMenuItem::new("复制")
                        .action(Box::new(Copy))
                        .disabled(text.is_empty())
                        .on_click(move |_, window, cx| {
                            if text.is_empty() {
                                return;
                            }
                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                            if generation.is_some()
                                && copy_pane.upgrade().map(|pane| pane.read(cx).generation)
                                    == generation
                                && selected_lines.is_some()
                            {
                                state.update(cx, |state, cx| {
                                    // DiffState short-circuits equal ranges;
                                    // Base may already have cleared its local
                                    // selection flag before its notification
                                    // removes this still-equal stored range.
                                    state.set_selected_lines(None, cx);
                                    state.set_selected_lines(selected_lines.clone(), cx)
                                });
                            }
                            state.focus_handle(cx).focus(window, cx);
                        });
                    readonly_preview_menu_with_copy(
                        menu.action_context(menu_diff.focus_handle(cx)),
                        copy,
                        pane.upgrade()
                            .is_some_and(|pane| pane.read(cx).can_show_file_history()),
                    )
                })
                .into_any_element()
        } else {
            let text = match &self.status {
                Status::Empty => "选择一个文件以查看内容".into(),
                Status::Loading => "正在准备文件预览…".into(),
                Status::Error(error) => error.clone(),
                Status::Ready => "文件没有可显示的内容".into(),
            };
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p(px(24.))
                .text_size(px(13.))
                .text_color(cx.theme().muted_foreground)
                .child(text)
                .into_any_element()
        };
        view.child(
            div()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_hidden()
                .child(body),
        )
    }
}

/// Both controls receive the same read-only commands. Source copying uses its
/// native action; Diff provides a selection-preserving adapter for Base's menu.
fn readonly_preview_menu(menu: PopupMenu, can_copy: bool, can_history: bool) -> PopupMenu {
    readonly_preview_menu_with_copy(
        menu,
        PopupMenuItem::new("复制")
            .action(Box::new(Copy))
            .disabled(!can_copy),
        can_history,
    )
}

fn readonly_preview_menu_with_copy(
    menu: PopupMenu,
    copy: PopupMenuItem,
    can_history: bool,
) -> PopupMenu {
    menu.item(copy)
        .menu("全选", Box::new(SelectAll))
        .separator()
        .menu("查找", Box::new(FindInPreview))
        .menu("查找下一个", Box::new(NextPreviewMatch))
        .menu("查找上一个", Box::new(PreviousPreviewMatch))
        .separator()
        .menu_with_disabled("查看文件历史", Box::new(FileHistoryInPreview), !can_history)
}

#[cfg(test)]
mod headless_tests;

fn estimated_content_bytes(source: Option<&SourceFile>, diff: Option<&FileDiff>) -> usize {
    // Reserve control overhead even before the lazily created controls render.
    // The coefficients include shared payloads, parsed patch rows, search
    // positions and syntax/layout derivatives. The separately reserved editor
    // rope is counted once, including any larger deferred previous value.
    // GPUI/OS allocators and other service caches are outside this estimate.
    let mut total = 64 * 1024usize;
    if let Some(source) = source {
        total = total
            .saturating_add(source.path.capacity())
            .saturating_add(source.text.capacity());
    }
    if let Some(diff) = diff {
        let texts = diff
            .old_text
            .capacity()
            .saturating_add(diff.new_text.capacity());
        let line_text = diff.lines.iter().fold(0usize, |bytes, line| {
            bytes.saturating_add(line.text.capacity())
        });
        total = total
            .saturating_add(diff.path.capacity())
            .saturating_add(texts.saturating_mul(3))
            .saturating_add(diff.patch.capacity().saturating_mul(3))
            .saturating_add(line_text)
            .saturating_add(diff.lines.capacity().saturating_mul(
                std::mem::size_of::<gitviews_service::DiffLine>().saturating_add(64),
            ));
    }
    total
}

fn estimated_editor_bytes(text: &str, capacity: usize) -> usize {
    capacity.saturating_mul(2).saturating_add(
        text.bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            .saturating_add(1)
            .saturating_mul(16),
    )
}

fn step_index(current: usize, length: usize, forward: bool) -> usize {
    if length == 0 {
        0
    } else if forward {
        (current + 1) % length
    } else {
        (current + length - 1) % length
    }
}

fn next_change_index(current: Option<usize>, length: usize, forward: bool) -> usize {
    current.map_or_else(
        || if forward { 0 } else { length.saturating_sub(1) },
        |current| step_index(current, length, forward),
    )
}

fn new_hunk_line(header: &str) -> Option<usize> {
    header
        .split_whitespace()
        .find_map(|word| word.strip_prefix('+'))?
        .split(',')
        .next()?
        .parse()
        .ok()
}

fn find_diff_lines(diff: &FileDiff, query: &str) -> Vec<DiffLinePosition> {
    if query.is_empty() {
        return Vec::new();
    }
    diff.lines
        .iter()
        .filter(|line| line.kind != DiffLineKind::Section && line.text.contains(query))
        .filter_map(|line| {
            let (side, number) = if line.kind == DiffLineKind::Deleted {
                (DiffSide::Original, line.old_line?)
            } else {
                (DiffSide::Modified, line.new_line?)
            };
            Some(DiffLinePosition::new(diff.path.clone(), side, number))
        })
        .collect()
}

/// Merge whitespace-only old/new pairs into unchanged context. A paired delete
/// and addition consume one line on each side, as does their replacement
/// context line, so Git's original hunk ranges remain valid.
fn ignore_whitespace_patch(patch: &str) -> String {
    let lines = patch.lines().collect::<Vec<_>>();
    let mut output = String::with_capacity(patch.len());
    let mut index = 0;
    let mut in_hunk = false;
    while index < lines.len() {
        let line = lines[index];
        if line.starts_with("diff --git ") {
            in_hunk = false;
        }
        if line.starts_with("@@ ") {
            in_hunk = true;
        }
        if in_hunk && (line.starts_with('-') || line.starts_with('+')) {
            let mut old = Vec::new();
            let mut new = Vec::new();
            while index < lines.len()
                && (lines[index].starts_with('-') || lines[index].starts_with('+'))
            {
                let line = lines[index];
                if let Some(text) = line.strip_prefix('-') {
                    old.push(text);
                } else if let Some(text) = line.strip_prefix('+') {
                    new.push(text);
                }
                index += 1;
            }
            for offset in 0..old.len().max(new.len()) {
                match (old.get(offset), new.get(offset)) {
                    (Some(old), Some(new)) if old.trim() == new.trim() => {
                        output.push(' ');
                        output.push_str(new);
                        output.push('\n');
                    }
                    (old, new) => {
                        if let Some(old) = old {
                            output.push('-');
                            output.push_str(old);
                            output.push('\n');
                        }
                        if let Some(new) = new {
                            output.push('+');
                            output.push_str(new);
                            output.push('\n');
                        }
                    }
                }
            }
        } else {
            output.push_str(line);
            output.push('\n');
            index += 1;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{
        estimated_editor_bytes, find_diff_lines, ignore_whitespace_patch, new_hunk_line,
        next_change_index,
    };
    use gitviews_service::{DiffLine, DiffLineKind, FileDiff};
    use gpui_kit::component::diff::{DiffFile, DiffSide};

    #[test]
    fn ignored_whitespace_keeps_hunk_counts_parseable_and_real_changes_visible() {
        let patch = "diff --git a/test.rs b/test.rs\n--- a/test.rs\n+++ b/test.rs\n@@ -1,2 +1,2 @@\n-  unchanged\n-old\n+unchanged  \n+new\n";
        let files = DiffFile::parse(&ignore_whitespace_patch(patch)).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].additions(), 1);
        assert_eq!(files[0].deletions(), 1);
    }

    #[test]
    fn unicode_find_uses_real_side_and_source_line_without_duplicate_context_hits() {
        let diff = FileDiff {
            path: "中文.rs".into(),
            old_text: String::new(),
            new_text: String::new(),
            patch: String::new(),
            lines: vec![
                DiffLine {
                    old_line: Some(4),
                    new_line: Some(7),
                    kind: DiffLineKind::Context,
                    text: "共有 中文".into(),
                },
                DiffLine {
                    old_line: Some(5),
                    new_line: None,
                    kind: DiffLineKind::Deleted,
                    text: "旧 中文".into(),
                },
                DiffLine {
                    old_line: None,
                    new_line: Some(8),
                    kind: DiffLineKind::Added,
                    text: "新 中文".into(),
                },
            ],
        };
        let hits = find_diff_lines(&diff, "中文");
        assert_eq!(hits.len(), 3);
        assert_eq!((hits[0].side(), hits[0].line()), (DiffSide::Modified, 7));
        assert_eq!((hits[1].side(), hits[1].line()), (DiffSide::Original, 5));
        assert!(find_diff_lines(&diff, "").is_empty());
    }

    #[test]
    fn source_navigation_reads_new_line_from_git_hunk_metadata() {
        assert_eq!(new_hunk_line("@@ -4,2 +8,3 @@ fn 示例"), Some(8));
        assert_eq!(new_hunk_line("\\ No newline at end of file"), None);
        assert_eq!(next_change_index(None, 3, true), 0);
        assert_eq!(next_change_index(None, 3, false), 2);
        assert_eq!(next_change_index(Some(2), 3, true), 0);
        assert_eq!(next_change_index(Some(0), 3, false), 2);
    }

    #[test]
    fn editor_reservation_covers_first_render_and_a_larger_previous_rope() {
        let mut text = String::with_capacity(4096);
        text.push_str("中文\r\nsecond line\n");
        let reserved = estimated_editor_bytes(&text, text.capacity());
        let resident = estimated_editor_bytes(&text, text.len());
        assert!(reserved > resident);
        assert_eq!(reserved, reserved.max(resident));

        let old_rope = estimated_editor_bytes(&"line\n".repeat(4096), 20_480);
        assert!(old_rope > reserved);
        assert_eq!(reserved.max(old_rope), old_rope);
    }
}
