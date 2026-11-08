//! Native history analysis with virtualized rows and explicit read/mutation events.
use gitviews_service::{GraphCommit, HistoryToolsRequest, HistoryToolsResult, display_path};
use gpui_kit::{
    TestSupportExt as _,
    base::FocusTrapElement as _,
    component::{
        ActiveTheme, Disableable as _, ThemeMode,
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
    },
    prelude::FluentBuilder as _,
    *,
};
use std::{path::PathBuf, sync::Arc};

gpui_kit::actions!(gitviews_history_tools, [CloseHistoryTools]);
pub(crate) enum HistoryToolsDialogEvent {
    Request(HistoryToolsRequest),
    Closed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Graph,
    File,
    Blame,
    Compare,
    Recovery,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StashPart {
    Tracked,
    Staged,
    Unstaged,
    Untracked,
}
impl StashPart {
    const ALL: [Self; 4] = [Self::Tracked, Self::Staged, Self::Unstaged, Self::Untracked];
    fn id(self) -> &'static str {
        match self {
            Self::Tracked => "history-stash-tracked",
            Self::Staged => "history-stash-staged",
            Self::Unstaged => "history-stash-unstaged",
            Self::Untracked => "history-stash-untracked",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Tracked => "全部已跟踪",
            Self::Staged => "已暂存",
            Self::Unstaged => "未暂存",
            Self::Untracked => "未跟踪",
        }
    }
    fn revisions(self, id: &str) -> (String, String) {
        match self {
            Self::Tracked => (format!("{id}^1"), id.into()),
            Self::Staged => (format!("{id}^1"), format!("{id}^2")),
            Self::Unstaged => (format!("{id}^2"), id.into()),
            Self::Untracked => (":empty".into(), format!("{id}^3")),
        }
    }
}
impl Mode {
    const ALL: [Self; 5] = [
        Self::Graph,
        Self::File,
        Self::Blame,
        Self::Compare,
        Self::Recovery,
    ];
    fn id(self) -> &'static str {
        match self {
            Self::Graph => "history-tools-mode-graph",
            Self::File => "history-tools-mode-file",
            Self::Blame => "history-tools-mode-blame",
            Self::Compare => "history-tools-mode-compare",
            Self::Recovery => "history-tools-mode-recovery",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Graph => "提交拓扑",
            Self::File => "文件历史",
            Self::Blame => "逐行归属",
            Self::Compare => "版本比较",
            Self::Recovery => "分支恢复",
        }
    }
}
#[derive(Clone, Copy)]
enum Field {
    Path,
    Revision,
    Left,
    Right,
    Name,
}
impl Field {
    const ALL: [Self; 5] = [
        Self::Path,
        Self::Revision,
        Self::Left,
        Self::Right,
        Self::Name,
    ];
    fn id(self) -> &'static str {
        match self {
            Self::Path => "history-tools-path",
            Self::Revision => "history-tools-revision",
            Self::Left => "history-tools-left",
            Self::Right => "history-tools-right",
            Self::Name => "history-tools-branch-name",
        }
    }
}
struct Inputs {
    fields: Vec<Entity<InputState>>,
    _subscriptions: Vec<Subscription>,
}
#[derive(Clone)]
struct GraphRow {
    commit: GraphCommit,
    lane: usize,
    incoming: bool,
    edges: Vec<(usize, usize, bool)>,
}

pub(crate) struct HistoryToolsDialog {
    read_only: bool,
    open: bool,
    root: PathBuf,
    expected_head: Option<String>,
    mode: Mode,
    busy: bool,
    mutating: bool,
    inputs: Option<Inputs>,
    drafts: [String; 5],
    result: Option<Arc<HistoryToolsResult>>,
    rows: Arc<Vec<GraphRow>>,
    lanes: Vec<String>,
    anchor: Option<String>,
    offset: usize,
    next_offset: usize,
    has_more: bool,
    append: bool,
    dirty: bool,
    selected_recovery: Option<String>,
    error: Option<String>,
    status: Option<String>,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    stash_id: Option<String>,
    stash_part: StashPart,
}
impl EventEmitter<HistoryToolsDialogEvent> for HistoryToolsDialog {}
impl HistoryToolsDialog {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        cx.bind_keys([KeyBinding::new(
            "escape",
            CloseHistoryTools,
            Some("GitViewsHistoryTools"),
        )]);
        Self {
            read_only: false,
            open: false,
            root: PathBuf::new(),
            expected_head: None,
            mode: Mode::Graph,
            busy: false,
            mutating: false,
            inputs: None,
            drafts: [
                "".into(),
                "".into(),
                "HEAD~1".into(),
                "HEAD".into(),
                "".into(),
            ],
            result: None,
            rows: Arc::default(),
            lanes: Vec::new(),
            anchor: None,
            offset: 0,
            next_offset: 0,
            has_more: false,
            append: false,
            dirty: false,
            selected_recovery: None,
            error: None,
            status: None,
            focus: cx.focus_handle(),
            previous_focus: None,
            stash_id: None,
            stash_part: StashPart::Tracked,
        }
    }
    pub(crate) fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        self.read_only = read_only;
        if read_only && self.mode == Mode::Recovery {
            self.mode = Mode::File;
        }
        cx.notify();
    }

    pub(crate) fn show(
        &mut self,
        root: PathBuf,
        expected_head: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.clear(cx);
        self.open = true;
        self.root = root;
        self.expected_head = expected_head;
        self.mode = Mode::Graph;
        self.drafts = [
            "".into(),
            "".into(),
            "HEAD~1".into(),
            "HEAD".into(),
            "".into(),
        ];
        self.request(false, cx);
    }
    pub(crate) fn show_file_history(
        &mut self,
        root: PathBuf,
        expected_head: Option<String>,
        path: String,
        cx: &mut Context<Self>,
    ) {
        self.clear(cx);
        self.open = true;
        self.root = root;
        self.expected_head = expected_head;
        self.mode = Mode::File;
        self.drafts = [path, "".into(), "HEAD~1".into(), "HEAD".into(), "".into()];
        self.request(false, cx);
    }

    pub(crate) fn root(&self) -> &std::path::Path {
        &self.root
    }

    pub(crate) fn set_path(&mut self, path: String, cx: &mut Context<Self>) {
        self.drafts[Field::Path as usize] = path;
        self.inputs = None;
        cx.notify();
    }
    pub(crate) fn show_stash(
        &mut self,
        root: PathBuf,
        expected_head: Option<String>,
        id: String,
        cx: &mut Context<Self>,
    ) {
        self.clear(cx);
        self.open = true;
        self.root = root;
        self.expected_head = expected_head;
        self.mode = Mode::Compare;
        self.stash_part = StashPart::Tracked;
        let (left, right) = self.stash_part.revisions(&id);
        self.stash_id = Some(id);
        self.drafts = ["".into(), "".into(), left, right, "".into()];
        self.request(false, cx);
    }
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }
    pub(crate) fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        if busy {
            self.error = None;
        } else {
            self.mutating = false;
        }
        cx.notify();
    }
    pub(crate) fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.busy = false;
        self.mutating = false;
        self.error = error.map(|message| if self.mode == Mode::Compare && self.stash_id.is_some() && self.stash_part == StashPart::Untracked {
            format!("未跟踪快照读取失败；未保存未跟踪文件的储藏没有此快照，可切换其他面板。{message}")
        }else{message});
        cx.notify();
    }
    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.busy = false;
        self.mutating = false;
        self.inputs = None;
        self.result = None;
        self.rows = Arc::default();
        self.lanes.clear();
        self.anchor = None;
        self.offset = 0;
        self.next_offset = 0;
        self.has_more = false;
        self.append = false;
        self.dirty = false;
        self.selected_recovery = None;
        self.error = None;
        self.status = None;
        self.previous_focus = None;
        self.stash_id = None;
        self.stash_part = StashPart::Tracked;
        cx.notify();
    }
    pub(crate) fn set_result(&mut self, result: HistoryToolsResult, cx: &mut Context<Self>) {
        self.busy = false;
        self.mutating = false;
        self.error = None;
        match &result {
            HistoryToolsResult::Commits(page) => {
                if !self.append || self.anchor != page.anchor {
                    self.rows = Arc::default();
                    self.lanes.clear();
                }
                let rows = Arc::make_mut(&mut self.rows);
                for commit in &page.commits {
                    if rows.len() >= 2_000 {
                        break;
                    }
                    rows.push(graph_row(commit.clone(), &mut self.lanes));
                }
                self.anchor = page.anchor.clone();
                self.next_offset = page.next_offset;
                self.has_more = page.has_more && rows.len() < 2_000;
                self.status = Some(format!(
                    "显示 {} 个提交 · 本页扫描 {} 个{}{}。点击提交可比较其第一个父提交；… 表示连接在可见通道以外。",
                    rows.len(),
                    page.scanned_commits,
                    if page.truncated && page.has_more {
                        " · 已达本次读取预算，可继续读取"
                    } else if page.truncated {
                        " · 已达历史读取边界"
                    } else {
                        ""
                    },
                    if rows.len() >= 2_000 {
                        " · 窗口达到 2000 行上限，请缩小文件范围"
                    } else {
                        ""
                    }
                ));
            }
            HistoryToolsResult::Blame(view) => {
                self.status = Some(format!(
                    "{} · {} · {} 行 · 未提交行单独标记",
                    view.path,
                    view.revision,
                    view.lines.len()
                ))
            }
            HistoryToolsResult::Comparison(view) => {
                self.status = Some(format!(
                    "{} → {} · {} 个文件 · +{} / -{}{}",
                    short(&view.left),
                    short(&view.right),
                    view.files.len(),
                    view.insertions,
                    view.deletions,
                    if view.truncated {
                        " · 文件列表或行统计受预算限制"
                    } else {
                        ""
                    }
                ))
            }
            HistoryToolsResult::Recovery(page) => {
                self.has_more = page.has_more;
                self.next_offset = self.offset.saturating_add(page.entries.len());
                self.selected_recovery = None;
                self.status = Some(format!(
                    "{} 个可恢复提交{}。恢复只创建新分支。",
                    page.entries.len(),
                    if page.truncated {
                        " · 部分历史因预算或权限未读取"
                    } else {
                        ""
                    }
                ));
            }
            HistoryToolsResult::Restored { summary } => {
                self.status = Some(summary.clone());
                self.selected_recovery = None;
                cx.notify();
                return;
            }
        }
        self.result = Some(Arc::new(result));
        cx.notify();
    }
    fn ensure_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.previous_focus.is_none() {
            self.previous_focus = window.focused(cx);
            self.focus.focus(window, cx);
        }
        if self.inputs.is_some() {
            return;
        }
        let fields: Vec<_> = Field::ALL
            .into_iter()
            .map(|field| {
                let value = self.drafts[field as usize].clone();
                cx.new(|cx| {
                    let mut input = InputState::new(window, cx);
                    input.set_value(value, window, cx);
                    input
                })
            })
            .collect();
        let subscriptions = fields
            .iter()
            .enumerate()
            .map(|(index, input)| {
                cx.subscribe_in(
                    input,
                    window,
                    move |this, input, event, _, cx| match event {
                        InputEvent::Change => {
                            this.drafts[index] = input.read(cx).value().to_string();
                            if index != Field::Name as usize {
                                this.dirty = true;
                            }
                            this.error = None;
                            cx.notify();
                        }
                        InputEvent::PressEnter { .. } if !this.busy => this.request(false, cx),
                        _ => {}
                    },
                )
            })
            .collect();
        self.inputs = Some(Inputs {
            fields,
            _subscriptions: subscriptions,
        });
    }
    fn value(&self, field: Field, cx: &App) -> String {
        let value = self.inputs.as_ref().map_or_else(
            || self.drafts[field as usize].clone(),
            |inputs| inputs.fields[field as usize].read(cx).value().to_string(),
        );
        if matches!(field, Field::Path) {
            value
        } else {
            value.trim().to_owned()
        }
    }
    fn set_value(
        &mut self,
        field: Field,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drafts[field as usize] = value.clone();
        if let Some(inputs) = &self.inputs {
            inputs.fields[field as usize]
                .update(cx, |input, cx| input.set_value(value, window, cx));
        }
    }
    fn choose_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if self.read_only && mode == Mode::Recovery {
            return;
        }
        if self.mutating {
            return;
        }
        self.mode = mode;
        self.result = None;
        self.rows = Arc::default();
        self.lanes.clear();
        self.anchor = None;
        self.offset = 0;
        self.has_more = false;
        self.error = None;
        self.status = None;
        self.selected_recovery = None;
        self.request(false, cx);
    }
    fn choose_stash_part(&mut self, part: StashPart, window: &mut Window, cx: &mut Context<Self>) {
        if self.mutating {
            return;
        }
        let Some(id) = self.stash_id.clone() else {
            return;
        };
        let (left, right) = part.revisions(&id);
        self.stash_part = part;
        self.set_value(Field::Left, left, window, cx);
        self.set_value(Field::Right, right, window, cx);
        self.set_value(Field::Path, String::new(), window, cx);
        self.result = None;
        self.request(false, cx);
    }
    fn request(&mut self, next: bool, cx: &mut Context<Self>) {
        if self.mutating {
            return;
        }
        if next && (!self.has_more || self.dirty) {
            return;
        }
        self.offset = if next { self.next_offset } else { 0 };
        self.append = next;
        if !next {
            self.anchor = None;
        }
        let path = self.value(Field::Path, cx);
        let optional_path = (!path.is_empty()).then_some(path.clone());
        let request = match self.mode {
            Mode::Graph | Mode::File => {
                if self.mode == Mode::File && path.is_empty() {
                    self.error = Some("请填写仓库内文件的相对路径。".into());
                    self.busy = false;
                    cx.notify();
                    return;
                }
                HistoryToolsRequest::Commits {
                    anchor: self.anchor.clone(),
                    offset: self.offset,
                    limit: 100,
                    path: if self.mode == Mode::File {
                        optional_path
                    } else {
                        None
                    },
                }
            }
            Mode::Blame => {
                if path.is_empty() {
                    self.error = Some("请填写需要逐行归属的文件相对路径。".into());
                    self.busy = false;
                    cx.notify();
                    return;
                }
                let revision = self.value(Field::Revision, cx);
                HistoryToolsRequest::Blame {
                    path,
                    revision: (!revision.is_empty()).then_some(revision),
                }
            }
            Mode::Compare => HistoryToolsRequest::Compare {
                left: self.value(Field::Left, cx),
                right: self.value(Field::Right, cx),
                path: optional_path,
            },
            Mode::Recovery => HistoryToolsRequest::RecoveryList {
                offset: self.offset,
                limit: 100,
            },
        };
        self.dirty = false;
        self.set_busy(true, cx);
        cx.emit(HistoryToolsDialogEvent::Request(request));
    }
    fn restore(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.read_only {
            return;
        }
        let name = self.value(Field::Name, cx);
        let Some(commit_id) = self.selected_recovery.clone() else {
            self.error = Some("请选择要恢复的提交。".into());
            cx.notify();
            return;
        };
        if name.is_empty() {
            self.error = Some("请填写新的本地分支名称。".into());
            cx.notify();
            return;
        }
        self.mutating = true;
        self.set_busy(true, cx);
        cx.emit(HistoryToolsDialogEvent::Request(
            HistoryToolsRequest::Restore {
                name,
                commit_id,
                expected_head: self.expected_head.clone(),
            },
        ));
    }
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mutating {
            return;
        }
        let previous = self.previous_focus.take();
        self.clear(cx);
        if let Some(previous) = previous {
            previous.focus(window, cx);
        }
        cx.emit(HistoryToolsDialogEvent::Closed);
    }
    fn input_row(&self, field: Field, label: &'static str) -> Div {
        div().flex().flex_col().gap(px(4.)).child(label).when_some(
            self.inputs.as_ref(),
            |d, inputs| {
                d.child(
                    Input::new(&inputs.fields[field as usize])
                        .id(field.id())
                        .w_full()
                        .h(px(30.))
                        .disabled(self.mutating),
                )
            },
        )
    }
}

fn graph_row(commit: GraphCommit, lanes: &mut Vec<String>) -> GraphRow {
    let found = lanes.iter().position(|id| id == &commit.id);
    let incoming = found.is_some();
    let lane = found.unwrap_or_else(|| {
        lanes.push(commit.id.clone());
        lanes.len() - 1
    });
    let before = lanes.clone();
    lanes.remove(lane);
    for (index, parent) in commit.parents.iter().enumerate() {
        if !lanes.contains(parent) && lanes.len() < 64 {
            lanes.insert((lane + index).min(lanes.len()), parent.clone());
        }
    }
    let mut edges = before
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != lane)
        .filter_map(|(index, id)| {
            lanes
                .iter()
                .position(|next| next == id)
                .map(|next| (index, next, false))
        })
        .collect::<Vec<_>>();
    for parent in &commit.parents {
        if let Some(next) = lanes.iter().position(|id| id == parent) {
            edges.push((lane, next, true));
        }
    }
    GraphRow {
        commit,
        lane,
        incoming,
        edges,
    }
}
fn short(value: &str) -> String {
    if value == ":empty" {
        return "空树".into();
    }
    value.chars().take(8).collect()
}
fn date_utc(timestamp: i64) -> String {
    let seconds = timestamp.rem_euclid(86_400);
    let days = i128::from(timestamp.div_euclid(86_400)) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    if !(0..=9_999).contains(&year) {
        return timestamp.to_string();
    }
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        seconds / 3_600,
        seconds % 3_600 / 60
    )
}
fn graph_glyph(row: &GraphRow) -> Div {
    let edges = row.edges.clone();
    let lane = row.lane;
    let incoming = row.incoming;
    div()
        .relative()
        .w(px(168.))
        .h(px(60.))
        .flex_shrink_0()
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let coordinate = |lane: usize, y: f32| {
                        point(
                            bounds.left() + px(8. + lane.min(10) as f32 * 14.),
                            bounds.top() + px(y),
                        )
                    };
                    for (from, to, starts_here) in &edges {
                        // Never merge unrelated hidden lanes into a false visible connection.
                        if *from > 10 || *to > 10 {
                            continue;
                        }
                        let mut line = PathBuilder::stroke(px(1.5));
                        line.move_to(coordinate(*from, if *starts_here { 30. } else { 0. }));
                        line.line_to(coordinate(*to, 60.));
                        if let Ok(path) = line.build() {
                            window.paint_path(
                                path,
                                rgb([0x58a6ff, 0x3fb950, 0xd2a8ff, 0xffa657][*from % 4]),
                            );
                        }
                    }
                    if incoming && lane <= 10 {
                        let mut line = PathBuilder::stroke(px(1.5));
                        line.move_to(coordinate(lane, 0.));
                        line.line_to(coordinate(lane, 30.));
                        if let Ok(path) = line.build() {
                            window.paint_path(path, rgb(0x58a6ff));
                        }
                    }
                },
            )
            .size_full(),
        )
        .when(lane <= 10, |d| {
            d.child(
                div()
                    .absolute()
                    .left(px(4. + lane as f32 * 14.))
                    .top(px(26.))
                    .size(px(8.))
                    .rounded_full()
                    .bg(rgb(0x58a6ff)),
            )
        })
        .when(
            lane > 10 || row.edges.iter().any(|(from, to, _)| *from > 10 || *to > 10),
            |d| {
                d.child(
                    div()
                        .absolute()
                        .right(px(2.))
                        .top(px(20.))
                        .text_size(px(11.))
                        .child("…"),
                )
            },
        )
}

impl Render for HistoryToolsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        self.ensure_inputs(window, cx);
        let light = cx.theme().mode == ThemeMode::Light;
        let border = rgb(if light { 0xd0d7de } else { 0x30363d });
        let muted: Hsla = rgb(if light { 0x59636e } else { 0x9198a1 }).into();
        let mode_controls = div().flex().gap(px(6.)).children(
            Mode::ALL
                .into_iter()
                .filter(|mode| !self.read_only || *mode != Mode::Recovery)
                .map(|mode| {
                    Button::new(mode.id())
                        .label(mode.label())
                        .when(self.mode == mode, |b| b.primary())
                        .disabled(self.mutating)
                        .on_click(cx.listener(move |this, _, _, cx| this.choose_mode(mode, cx)))
                }),
        );
        let mut form = div().flex().flex_col().gap(px(8.));
        if matches!(self.mode, Mode::File | Mode::Blame | Mode::Compare) {
            form = form.child(self.input_row(
                Field::Path,
                if self.mode == Mode::Compare {
                    "文件相对路径（可选；留空查看文件列表）"
                } else {
                    "文件相对路径"
                },
            ));
        }
        if self.mode == Mode::Blame {
            form = form.child(self.input_row(
                Field::Revision,
                "版本（留空分析当前工作文件；填写 HEAD 分析已提交版本）",
            ));
        }
        if self.mode == Mode::Compare {
            if self.stash_id.is_some() {
                form = form.child(div().flex().gap(px(6.)).children(
                    StashPart::ALL.into_iter().map(|part| {
                        Button::new(part.id())
                            .label(part.label())
                            .when(self.stash_part == part, |b| b.primary())
                            .disabled(self.mutating)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.choose_stash_part(part, window, cx)
                            }))
                    }),
                ));
            }
            form = form.child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(div().flex_1().child(self.input_row(Field::Left, "旧版本")))
                    .child(div().flex_1().child(self.input_row(Field::Right, "新版本"))),
            );
        }
        let mut content = div().flex_1().min_h_0().flex().flex_col();
        let entity = cx.entity();
        if let Some(result) = &self.result {
            match result.as_ref() {
                HistoryToolsResult::Commits(_) => {
                    let rows = Arc::clone(&self.rows);
                    let count = rows.len();
                    content = content.child(
                        uniform_list("history-tools-graph", count, move |range, _, _| {
                            range
                                .map(|index| {
                                    let row = &rows[index];
                                    let commit = row.commit.clone();
                                    let target = entity.clone();
                                    div()
                                        .id(("history-tools-commit", index))
                                        .test_support()
                                        .role(Role::Button)
                                        .aria_label(format!(
                                            "提交 {}，父提交 {}",
                                            commit.id,
                                            commit.parents.join("、")
                                        ))
                                        .h(px(60.))
                                        .w_full()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.))
                                        .cursor_pointer()
                                        .child(graph_glyph(row))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .flex()
                                                .flex_col()
                                                .gap(px(4.))
                                                .child(div().truncate().child(format!(
                                                    "{}  {}",
                                                    short(&commit.id),
                                                    commit.subject
                                                )))
                                                .child(
                                                    div()
                                                        .truncate()
                                                        .text_color(muted)
                                                        .text_size(px(11.))
                                                        .child(format!(
                                                            "{} · {} · 父提交 {}",
                                                            commit.author,
                                                            date_utc(commit.time),
                                                            if commit.parents.is_empty() {
                                                                "无".into()
                                                            } else {
                                                                commit
                                                                    .parents
                                                                    .iter()
                                                                    .map(|p| short(p))
                                                                    .collect::<Vec<_>>()
                                                                    .join("、")
                                                            }
                                                        )),
                                                ),
                                        )
                                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                                            target.update(cx, |this, cx| {
                                                if this.mutating {
                                                    return;
                                                }
                                                this.mode = Mode::Compare;
                                                this.set_value(
                                                    Field::Left,
                                                    commit
                                                        .parents
                                                        .first()
                                                        .cloned()
                                                        .unwrap_or_else(|| ":empty".into()),
                                                    window,
                                                    cx,
                                                );
                                                this.set_value(
                                                    Field::Right,
                                                    commit.id.clone(),
                                                    window,
                                                    cx,
                                                );
                                                this.set_value(
                                                    Field::Path,
                                                    String::new(),
                                                    window,
                                                    cx,
                                                );
                                                this.request(false, cx);
                                            })
                                        })
                                })
                                .collect::<Vec<_>>()
                        })
                        .flex_1()
                        .min_h_0()
                        .w_full(),
                    );
                }
                HistoryToolsResult::Blame(view) => {
                    let count = view.lines.len();
                    let result = Arc::clone(result);
                    content = content.child(
                        uniform_list("history-tools-blame", count, move |range, _, _| {
                            let HistoryToolsResult::Blame(view) = result.as_ref() else {
                                return Vec::new();
                            };
                            range
                                .map(|index| {
                                    let line = &view.lines[index];
                                    div()
                                        .id(("history-tools-blame-line", index))
                                        .test_support()
                                        .aria_label(format!(
                                            "{} {} {}",
                                            line.line_number,
                                            line.commit_id
                                                .as_ref()
                                                .map_or("未提交".into(), |id| short(id)),
                                            line.text
                                        ))
                                        .h(px(26.))
                                        .flex()
                                        .gap(px(12.))
                                        .font_family("Consolas")
                                        .text_size(px(11.))
                                        .child(
                                            div()
                                                .w(px(48.))
                                                .text_color(muted)
                                                .child(line.line_number.to_string()),
                                        )
                                        .child(
                                            div()
                                                .w(px(72.))
                                                .text_color(if line.commit_id.is_none() {
                                                    rgb(0xffa657).into()
                                                } else {
                                                    muted
                                                })
                                                .child(
                                                    line.commit_id
                                                        .as_ref()
                                                        .map_or("未提交".into(), |id| short(id)),
                                                ),
                                        )
                                        .child(div().w(px(130.)).truncate().child(format!(
                                            "{}:{}",
                                            line.author, line.original_line_number
                                        )))
                                        .child(div().flex_1().min_w_0().truncate().child(
                                            line.text.trim_end_matches(['\r', '\n']).to_owned(),
                                        ))
                                })
                                .collect::<Vec<_>>()
                        })
                        .flex_1()
                        .min_h_0()
                        .w_full(),
                    );
                }
                HistoryToolsResult::Comparison(view) => {
                    let count = view.files.len();
                    let result = Arc::clone(result);
                    let target = entity.clone();
                    content = content.child(
                        uniform_list("history-tools-compare-files", count, move |range, _, _| {
                            let HistoryToolsResult::Comparison(view) = result.as_ref() else {
                                return Vec::new();
                            };
                            range
                                .map(|index| {
                                    let file = view.files[index].clone();
                                    let target = target.clone();
                                    Button::new(("history-tools-compare-file", index))
                                        .ghost()
                                        .w_full()
                                        .h(px(30.))
                                        .label(format!(
                                            "{}  {}{}",
                                            file.status,
                                            file.path,
                                            file.old_path
                                                .as_ref()
                                                .map_or(String::new(), |old| format!(" ← {old}"))
                                        ))
                                        .on_click(move |_, window, cx| {
                                            target.update(cx, |this, cx| {
                                                this.set_value(
                                                    Field::Path,
                                                    file.path.clone(),
                                                    window,
                                                    cx,
                                                );
                                                this.request(false, cx);
                                            })
                                        })
                                })
                                .collect::<Vec<_>>()
                        })
                        .h(px((count.clamp(1, 6) * 30) as f32))
                        .w_full(),
                    );
                    if let Some(diff) = &view.diff {
                        let count = diff.lines.len();
                        let result = Arc::clone(self.result.as_ref().expect("comparison result"));
                        content = content.child(
                            uniform_list(
                                "history-tools-compare-diff",
                                count,
                                move |range, _, _| {
                                    let HistoryToolsResult::Comparison(view) = result.as_ref()
                                    else {
                                        return Vec::new();
                                    };
                                    let Some(diff) = &view.diff else {
                                        return Vec::new();
                                    };
                                    range
                                        .map(|index| {
                                            let line = &diff.lines[index];
                                            let color = match line.kind {
                                                gitviews_service::DiffLineKind::Added => {
                                                    rgb(0x2da44e)
                                                }
                                                gitviews_service::DiffLineKind::Deleted => {
                                                    rgb(0xcf222e)
                                                }
                                                _ => rgb(if light { 0x1f2328 } else { 0xc9d1d9 }),
                                            };
                                            div()
                                                .id(("history-tools-diff-line", index))
                                                .test_support()
                                                .aria_label(format!(
                                                    "{} {}",
                                                    match line.kind {
                                                        gitviews_service::DiffLineKind::Added =>
                                                            "新增",
                                                        gitviews_service::DiffLineKind::Deleted =>
                                                            "删除",
                                                        gitviews_service::DiffLineKind::Context =>
                                                            "上下文",
                                                        gitviews_service::DiffLineKind::Section =>
                                                            "片段",
                                                    },
                                                    line.text
                                                ))
                                                .h(px(24.))
                                                .font_family("Consolas")
                                                .text_size(px(11.))
                                                .flex()
                                                .gap(px(12.))
                                                .text_color(color)
                                                .child(format!(
                                                    "{:>5} {:>5} {} {}",
                                                    line.old_line
                                                        .map_or(String::new(), |n| n.to_string()),
                                                    line.new_line
                                                        .map_or(String::new(), |n| n.to_string()),
                                                    match line.kind {
                                                        gitviews_service::DiffLineKind::Added =>
                                                            '+',
                                                        gitviews_service::DiffLineKind::Deleted =>
                                                            '-',
                                                        _ => ' ',
                                                    },
                                                    line.text
                                                ))
                                        })
                                        .collect::<Vec<_>>()
                                },
                            )
                            .flex_1()
                            .min_h_0()
                            .w_full(),
                        );
                    } else {
                        content = content.child(div().mt(px(12.)).text_color(muted).child(
                            "选择文件查看两版本差异。UTF-16 会解码后比较；二进制文件会单独提示。",
                        ));
                    }
                }
                HistoryToolsResult::Recovery(page) => {
                    let count = page.entries.len();
                    let result = Arc::clone(result);
                    let selected = self.selected_recovery.clone();
                    content = content.child(
                        uniform_list("history-tools-recovery", count, move |range, _, _| {
                            let HistoryToolsResult::Recovery(page) = result.as_ref() else {
                                return Vec::new();
                            };
                            range
                                .map(|index| {
                                    let entry = page.entries[index].clone();
                                    let target = entity.clone();
                                    Button::new(("history-tools-recovery-entry", index))
                                        .w_full()
                                        .h(px(48.))
                                        .ghost()
                                        .when(selected.as_deref() == Some(&entry.commit_id), |b| {
                                            b.primary()
                                        })
                                        .label(format!(
                                            "{}  {} · {}",
                                            short(&entry.commit_id),
                                            entry.label,
                                            entry.source_reference
                                        ))
                                        .on_click(move |_, window, cx| {
                                            target.update(cx, |this, cx| {
                                                if this.busy {
                                                    return;
                                                }
                                                this.selected_recovery =
                                                    Some(entry.commit_id.clone());
                                                if this.value(Field::Name, cx).is_empty() {
                                                    this.set_value(
                                                        Field::Name,
                                                        format!(
                                                            "restored/{}",
                                                            short(&entry.commit_id)
                                                        ),
                                                        window,
                                                        cx,
                                                    );
                                                }
                                                cx.notify();
                                            })
                                        })
                                })
                                .collect::<Vec<_>>()
                        })
                        .flex_1()
                        .min_h_0()
                        .w_full(),
                    );
                }
                HistoryToolsResult::Restored { .. } => {}
            }
        } else {
            content = content.child(div().text_color(muted).child(if self.busy {
                "正在后台读取…"
            } else {
                "填写条件后读取。"
            }));
        }
        if self.mode == Mode::Recovery {
            form = form
                .child(self.input_row(Field::Name, "恢复为新的本地分支名称"))
                .child(
                    Button::new("history-tools-restore")
                        .label("恢复为新分支")
                        .primary()
                        .disabled(self.busy || self.selected_recovery.is_none())
                        .on_click(cx.listener(|this, _, _, cx| this.restore(cx))),
                );
        }
        let card = div()
            .w(px(1080.))
            .h(px(820.))
            .max_w(relative(0.96))
            .max_h(relative(0.94))
            .flex()
            .flex_col()
            .rounded(px(8.))
            .border_1()
            .border_color(border)
            .bg(rgb(if light { 0xffffff } else { 0x161b22 }))
            .text_color(rgb(if light { 0x1f2328 } else { 0xf0f6fc }))
            .shadow_lg()
            .child(
                div()
                    .p(px(18.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if self.read_only {
                                "历史分析（只读）"
                            } else {
                                "历史分析与恢复"
                            }),
                    )
                    .child(
                        div()
                            .id("history-tools-root")
                            .test_support()
                            .aria_label(display_path(&self.root))
                            .text_color(muted)
                            .mt(px(6.))
                            .child(display_path(&self.root)),
                    ),
            )
            .child(
                div()
                    .p(px(14.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(mode_controls)
                    .child(form),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px(px(14.))
                    .flex()
                    .flex_col()
                    .child(content),
            )
            .child(
                div()
                    .px(px(14.))
                    .py(px(8.))
                    .text_color(muted)
                    .child(self.status.clone().unwrap_or_default())
                    .when(self.dirty, |d| d.child(" · 条件已修改，请重新读取")),
            )
            .when_some(self.error.clone(), |d, error| {
                d.child(
                    div()
                        .id("history-tools-error")
                        .test_support()
                        .role(Role::Alert)
                        .aria_label(error.clone())
                        .px(px(14.))
                        .py(px(8.))
                        .text_color(rgb(0xcf222e))
                        .child(error),
                )
            })
            .child(
                div()
                    .p(px(14.))
                    .border_t_1()
                    .border_color(border)
                    .flex()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                Button::new("history-tools-load")
                                    .label(if self.busy {
                                        "读取中…"
                                    } else {
                                        "重新读取"
                                    })
                                    .disabled(self.busy)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.request(false, cx)),
                                    ),
                            )
                            .child(
                                Button::new("history-tools-next")
                                    .label("读取下一页")
                                    .disabled(
                                        self.busy
                                            || !self.has_more
                                            || self.dirty
                                            || !matches!(
                                                self.mode,
                                                Mode::Graph | Mode::File | Mode::Recovery
                                            ),
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| this.request(true, cx))),
                            ),
                    )
                    .child(
                        Button::new("history-tools-close")
                            .label(if self.busy && !self.mutating {
                                "取消并关闭"
                            } else {
                                "关闭"
                            })
                            .disabled(self.mutating)
                            .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
                    ),
            );
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba(0x00000088))
            .key_context("GitViewsHistoryTools")
            .track_focus(&self.focus)
            .tab_group()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .font_family("Noto Sans SC")
            .on_action(
                cx.listener(|this, _: &CloseHistoryTools, window, cx| this.close(window, cx)),
            )
            .child(card)
            .focus_trap("gitviews-history-tools-trap", &self.focus)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GraphCommit, HistoryToolsDialog, HistoryToolsDialogEvent, HistoryToolsRequest,
        HistoryToolsResult, graph_row,
    };
    use gitviews_service::{HistoryGraphPage, RecoveryEntry, RecoveryPage};
    use gpui_kit::{
        AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
        Subscription, TestAppContext, Window, WindowHandle, base::Root, div, px, size,
        test::TestWindowExt,
    };
    use std::{cell::RefCell, path::PathBuf, rc::Rc};
    fn commit(id: &str, parents: &[&str]) -> GraphCommit {
        GraphCommit {
            id: id.into(),
            parents: parents.iter().map(|p| (*p).into()).collect(),
            subject: "提交".into(),
            author: "作者".into(),
            time: 1_700_000_000,
        }
    }
    #[test]
    fn graph_lanes_split_and_join_at_the_correct_parent_across_page_boundaries() {
        let mut lanes = Vec::new();
        let merge = graph_row(commit("merge", &["a", "b"]), &mut lanes);
        assert_eq!(merge.edges, vec![(0, 0, true), (0, 1, true)]);
        assert_eq!(lanes, vec!["a", "b"]);
        let a = graph_row(commit("a", &["base"]), &mut lanes);
        assert_eq!(a.edges, vec![(1, 1, false), (0, 0, true)]);
        // The retained lane state is reused when the next page begins.
        let b = graph_row(commit("b", &["base"]), &mut lanes);
        assert_eq!(b.lane, 1);
        assert!(b.incoming);
        assert_eq!(b.edges, vec![(0, 0, false), (1, 0, true)]);
        let root = graph_row(commit("base", &[]), &mut lanes);
        assert_eq!(root.lane, 0);
        assert!(root.incoming);
        assert!(lanes.is_empty());
    }
    struct Harness {
        dialog: Entity<HistoryToolsDialog>,
        requests: Rc<RefCell<Vec<HistoryToolsRequest>>>,
        _subscription: Subscription,
    }
    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().relative().size_full().child(self.dialog.clone())
        }
    }
    fn harness(cx: &mut TestAppContext) -> (WindowHandle<Root>, Entity<Harness>) {
        let (window, view) =
            crate::ui::headless::open_window(cx, size(px(1200.), px(1000.)), |_, cx| {
                cx.new(|cx| {
                    let dialog = cx.new(HistoryToolsDialog::new);
                    let requests = Rc::new(RefCell::new(Vec::new()));
                    let recorded = Rc::clone(&requests);
                    let subscription = cx.subscribe(&dialog, move |_, _, event, _| {
                        if let HistoryToolsDialogEvent::Request(request) = event {
                            recorded.borrow_mut().push(request.clone());
                        }
                    });
                    dialog.update(cx, |dialog, cx| {
                        dialog.show(PathBuf::from("C:/中文/child"), Some("a".repeat(40)), cx)
                    });
                    Harness {
                        dialog,
                        requests,
                        _subscription: subscription,
                    }
                })
            });
        cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        (window, view)
    }
    #[gpui_kit::test]
    fn clicking_graph_node_compares_that_commit_to_its_actual_parent(cx: &mut TestAppContext) {
        let (window, h) = harness(cx);
        let id = "b".repeat(40);
        let parent = "a".repeat(40);
        cx.update_window(window.into(), |_, window, cx| {
            h.read(cx).dialog.clone().update(cx, |dialog, cx| {
                dialog.set_result(
                    HistoryToolsResult::Commits(HistoryGraphPage {
                        anchor: Some(id.clone()),
                        commits: vec![commit(&id, &[&parent])],
                        next_offset: 1,
                        has_more: true,
                        truncated: false,
                        scanned_commits: 1,
                    }),
                    cx,
                )
            });
            window.render_frame(cx);
            window.click(("history-tools-commit", 0usize), cx);
        })
        .unwrap();
        cx.read(|cx| {
            let requests = h.read(cx).requests.borrow();
            let Some(HistoryToolsRequest::Compare { left, right, path }) = requests.last() else {
                panic!("compare request expected")
            };
            assert_eq!(left, &parent);
            assert_eq!(right, &id);
            assert!(path.is_none());
        });
    }
    #[gpui_kit::test]
    fn pagination_keeps_captured_anchor_and_read_close_remains_available(cx: &mut TestAppContext) {
        let (window, h) = harness(cx);
        let anchor = "c".repeat(40);
        cx.update_window(window.into(), |_, window, cx| {
            h.read(cx).dialog.clone().update(cx, |dialog, cx| {
                dialog.set_result(
                    HistoryToolsResult::Commits(HistoryGraphPage {
                        anchor: Some(anchor.clone()),
                        commits: vec![commit(&anchor, &[])],
                        next_offset: 17,
                        has_more: true,
                        truncated: true,
                        scanned_commits: 17,
                    }),
                    cx,
                )
            });
            window.render_frame(cx);
            window.click("history-tools-next", cx);
            assert_ne!(window.find("history-tools-close").disabled(), Some(true));
            window.click("history-tools-close", cx);
        })
        .unwrap();
        cx.read(|cx| {
            let harness = h.read(cx);
            let requests = harness.requests.borrow();
            let Some(HistoryToolsRequest::Commits {
                anchor: Some(actual),
                offset,
                ..
            }) = requests.last()
            else {
                panic!("paged request expected")
            };
            assert_eq!(actual, &anchor);
            assert_eq!(*offset, 17);
            assert!(!harness.dialog.read(cx).is_open());
        });
    }
    #[gpui_kit::test]
    fn recovery_uses_selected_full_commit_and_blocks_duplicate_mutation(cx: &mut TestAppContext) {
        let (window, h) = harness(cx);
        let restored = "d".repeat(40);
        cx.update_window(window.into(), |_, window, cx| {
            window.click("history-tools-mode-recovery", cx);
            h.read(cx).dialog.clone().update(cx, |dialog, cx| {
                dialog.set_result(
                    HistoryToolsResult::Recovery(RecoveryPage {
                        entries: vec![RecoveryEntry {
                            commit_id: restored.clone(),
                            label: "删除的分支".into(),
                            source_reference: "refs/gitviews/deleted-branches/fixture".into(),
                            time: 1_700_000_000,
                        }],
                        has_more: false,
                        truncated: false,
                    }),
                    cx,
                )
            });
            window.render_frame(cx);
            window.click(("history-tools-recovery-entry", 0usize), cx);
            window.click("history-tools-branch-name", cx);
            window.press(crate::ui::headless::SELECT_ALL_KEY, cx);
            window.input("restored/中文", cx);
            window.click("history-tools-restore", cx);
            window.click("history-tools-restore", cx);
            window.press("escape", cx);
            assert!(h.read(cx).dialog.read(cx).is_open());
        })
        .unwrap();
        cx.read(|cx| {
            let requests = h.read(cx).requests.borrow();
            let restores = requests
                .iter()
                .filter_map(|request| {
                    if let HistoryToolsRequest::Restore {
                        name,
                        commit_id,
                        expected_head,
                    } = request
                    {
                        Some((name, commit_id, expected_head))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(restores.len(), 1);
            assert_eq!(restores[0].0, "restored/中文");
            assert_eq!(restores[0].1, &restored);
            assert_eq!(
                restores[0].2.as_deref(),
                Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            );
        });
    }

    #[gpui_kit::test]
    fn stash_part_buttons_use_captured_commit_and_missing_untracked_snapshot_is_recoverable(
        cx: &mut TestAppContext,
    ) {
        let (window, h) = harness(cx);
        let id = "e".repeat(40);
        cx.update_window(window.into(), |_, window, cx| {
            h.read(cx).dialog.clone().update(cx, |dialog, cx| {
                dialog.show_stash(
                    PathBuf::from("C:/中文/child"),
                    Some("a".repeat(40)),
                    id.clone(),
                    cx,
                )
            });
            window.render_frame(cx);
            window.click("history-stash-staged", cx);
            assert_eq!(
                window.find("history-tools-left").value(),
                Some(format!("{id}^1").as_str())
            );
            assert_eq!(
                window.find("history-tools-right").value(),
                Some(format!("{id}^2").as_str())
            );
            window.click("history-stash-unstaged", cx);
            assert_eq!(
                window.find("history-tools-left").value(),
                Some(format!("{id}^2").as_str())
            );
            assert_eq!(
                window.find("history-tools-right").value(),
                Some(id.as_str())
            );
            window.click("history-stash-untracked", cx);
            h.read(cx).dialog.clone().update(cx, |dialog, cx| {
                dialog.set_error(Some("parent does not exist".into()), cx)
            });
            window.render_frame(cx);
            assert!(
                window
                    .find("history-tools-error")
                    .label()
                    .unwrap()
                    .contains("未保存未跟踪文件")
            );
            window.click("history-stash-tracked", cx);
        })
        .unwrap();
        cx.read(|cx| {
            let requests = h.read(cx).requests.borrow();
            let Some(HistoryToolsRequest::Compare { left, right, path }) = requests.last() else {
                panic!("tracked comparison expected")
            };
            assert_eq!(left, &format!("{id}^1"));
            assert_eq!(right, &id);
            assert!(path.is_none());
        });
    }
}
