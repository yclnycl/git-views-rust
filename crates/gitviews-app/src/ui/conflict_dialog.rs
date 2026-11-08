//! Three-way conflict editor. Reads and writes are performed by Workspace.
use gitviews_service::{
    ConflictAction, ConflictOperation, ConflictRequest, ConflictResolution, ConflictState,
    ConflictVersions, display_path,
};
use gpui_kit::{
    TestSupportExt as _,
    base::FocusTrapElement as _,
    component::{
        ActiveTheme, Disableable as _, ThemeMode,
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState, Textarea, TextareaState},
    },
    prelude::FluentBuilder as _,
    *,
};
use std::path::PathBuf;
#[cfg(test)]
mod headless_tests;
gpui_kit::actions!(gitviews_conflicts, [CloseConflictDialog]);

pub(crate) enum ConflictDialogEvent {
    Apply(ConflictRequest),
    LoadFile(String),
    Reload,
    Close,
}
struct Editors {
    sides: [Entity<TextareaState>; 3],
    result: Entity<TextareaState>,
    _subscription: Subscription,
}
pub(crate) struct ConflictDialog {
    open: bool,
    root: PathBuf,
    name: String,
    state: Option<ConflictState>,
    versions: Option<ConflictVersions>,
    selected: Option<String>,
    target: Option<Entity<InputState>>,
    editors: Option<Editors>,
    busy: bool,
    loading_file: bool,
    confirming_abort: bool,
    confirming_selection: Option<String>,
    error: Option<String>,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
}
impl EventEmitter<ConflictDialogEvent> for ConflictDialog {}
impl ConflictDialog {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        cx.bind_keys([KeyBinding::new(
            "escape",
            CloseConflictDialog,
            Some("GitViewsConflicts"),
        )]);
        Self {
            open: false,
            root: PathBuf::new(),
            name: String::new(),
            state: None,
            versions: None,
            selected: None,
            target: None,
            editors: None,
            busy: false,
            loading_file: false,
            confirming_abort: false,
            confirming_selection: None,
            error: None,
            focus: cx.focus_handle(),
            previous_focus: None,
        }
    }
    pub(crate) fn show(&mut self, root: PathBuf, name: String, cx: &mut Context<Self>) {
        self.clear(cx);
        self.open = true;
        self.root = root;
        self.name = name;
        self.busy = true;
        cx.notify();
    }
    pub(crate) fn set_state(&mut self, state: ConflictState, cx: &mut Context<Self>) {
        if self
            .selected
            .as_ref()
            .is_some_and(|path| !state.files.iter().any(|file| &file.path == path))
        {
            self.selected = None;
            self.versions = None;
            self.editors = None;
        }
        self.state = Some(state);
        self.busy = false;
        self.confirming_abort = false;
        self.error = None;
        cx.notify();
    }
    pub(crate) fn show_file(
        &mut self,
        root: PathBuf,
        name: String,
        path: String,
        cx: &mut Context<Self>,
    ) {
        self.show(root, name, cx);
        self.selected = Some(path);
        self.loading_file = true;
    }
    pub(crate) fn set_versions(&mut self, versions: ConflictVersions, cx: &mut Context<Self>) {
        // A slow read for a previously selected path cannot replace the editor.
        if self.selected.as_deref() != Some(&versions.path) {
            return;
        }
        self.versions = Some(versions);
        self.editors = None;
        self.loading_file = false;
        self.error = None;
        cx.notify();
    }
    pub(crate) fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.error = error;
        self.busy = false;
        self.loading_file = false;
        self.confirming_abort = false;
        cx.notify();
    }
    pub(crate) fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        if busy {
            self.error = None;
        }
        cx.notify();
    }
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }
    #[cfg(test)]
    pub(crate) fn selected_path(&self) -> Option<&str> {
        self.selected.as_deref()
    }
    #[cfg(test)]
    pub(crate) fn versions(&self) -> Option<&ConflictVersions> {
        self.versions.as_ref()
    }
    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.state = None;
        self.versions = None;
        self.selected = None;
        self.target = None;
        self.editors = None;
        self.busy = false;
        self.loading_file = false;
        self.confirming_abort = false;
        self.confirming_selection = None;
        self.error = None;
        self.previous_focus = None;
        cx.notify();
    }
    fn dirty_editor(&self, cx: &App) -> bool {
        self.editors
            .as_ref()
            .zip(self.versions.as_ref())
            .is_some_and(|(editors, versions)| {
                editors.result.read(cx).value().as_ref() != versions.result
            })
    }
    fn select(&mut self, path: String, cx: &mut Context<Self>) {
        if self.busy || self.loading_file || self.confirming_abort {
            return;
        }
        if self.dirty_editor(cx) {
            self.confirming_selection = Some(path);
            cx.notify();
            return;
        }
        self.load_file(path, cx);
    }
    fn load_file(&mut self, path: String, cx: &mut Context<Self>) {
        self.selected = Some(path.clone());
        self.versions = None;
        self.editors = None;
        self.loading_file = true;
        self.confirming_selection = None;
        self.error = None;
        cx.emit(ConflictDialogEvent::LoadFile(path));
        cx.notify();
    }
    fn submit(&mut self, action: ConflictAction, cx: &mut Context<Self>) {
        if self.busy || self.loading_file || self.state.is_none() {
            return;
        }
        if action == ConflictAction::Continue && self.dirty_editor(cx) {
            self.error = Some("编辑器中的合并结果尚未保存，请先保存并标记解决。".into());
            cx.notify();
            return;
        }
        let head = self.state.as_ref().and_then(|state| state.head.clone());
        self.confirming_abort = false;
        self.busy = true;
        self.error = None;
        cx.emit(ConflictDialogEvent::Apply(ConflictRequest {
            expected_head: head,
            action,
        }));
        cx.notify();
    }
    fn resolve(&mut self, resolution: ConflictResolution, cx: &mut Context<Self>) {
        if let Some(versions) = &self.versions {
            self.submit(
                ConflictAction::Resolve {
                    path: versions.path.clone(),
                    token: versions.token.clone(),
                    resolution,
                },
                cx,
            );
        }
    }
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.loading_file {
            return;
        }
        if self.dirty_editor(cx) {
            self.error =
                Some("合并结果尚未保存，请先保存并标记解决，或重新读取该文件后关闭。".into());
            cx.notify();
            return;
        }
        let previous = self.previous_focus.take();
        self.clear(cx);
        if let Some(focus) = previous {
            focus.focus(window, cx);
        }
        cx.emit(ConflictDialogEvent::Close);
    }
}
impl Render for ConflictDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        if self.previous_focus.is_none() {
            self.previous_focus = window.focused(cx);
            self.focus.focus(window, cx);
        }
        if self.target.is_none() {
            self.target = Some(cx.new(|cx| {
                InputState::new(window, cx).placeholder("目标分支或提交 ID，例如 feature/login")
            }));
        }
        if self.editors.is_none()
            && let Some(versions) = &self.versions
        {
            let mut textarea = |value: String| {
                cx.new(|cx| TextareaState::new(window, cx).rows(7).default_value(value))
            };
            let sides = [
                textarea(
                    versions
                        .base
                        .clone()
                        .unwrap_or_else(|| "（此版本不存在，表示删除）".into()),
                ),
                textarea(
                    versions
                        .ours
                        .clone()
                        .unwrap_or_else(|| "（此版本不存在，表示删除）".into()),
                ),
                textarea(
                    versions
                        .theirs
                        .clone()
                        .unwrap_or_else(|| "（此版本不存在，表示删除）".into()),
                ),
            ];
            let result = textarea(versions.result.clone());
            let subscription = cx.subscribe(&result, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            });
            self.editors = Some(Editors {
                sides,
                result,
                _subscription: subscription,
            });
        }
        let light = cx.theme().mode == ThemeMode::Light;
        let border = rgb(if light { 0xd0d7de } else { 0x30363d });
        let muted: Hsla = rgb(if light { 0x59636e } else { 0x9198a1 }).into();
        let busy = self.busy
            || self.loading_file
            || self.confirming_abort
            || self.confirming_selection.is_some();
        let kind = self.state.as_ref().map(|state| state.kind);
        let can_start = kind == Some(ConflictOperation::Clean)
            && self
                .state
                .as_ref()
                .is_some_and(|state| state.files.is_empty() && state.head.is_some());
        let can_continue =
            self.state.as_ref().is_some_and(|state| state.can_continue) && !self.dirty_editor(cx);
        let can_abort = self.state.as_ref().is_some_and(|state| state.can_abort);
        let mut form = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .id("conflict-operation-state")
                    .test_support()
                    .aria_label(
                        self.state
                            .as_ref()
                            .map(|state| state.label.clone())
                            .unwrap_or_else(|| "正在读取…".into()),
                    )
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(
                        self.state
                            .as_ref()
                            .map(|state| state.label.clone())
                            .unwrap_or_else(|| "正在读取操作状态…".into()),
                    ),
            )
            .when_some(
                self.state
                    .as_ref()
                    .and_then(|state| state.backup_ref.clone()),
                |form, reference| {
                    form.child(
                        div()
                            .text_color(muted)
                            .child(format!("操作前备份：{reference}")),
                    )
                },
            )
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        Input::new(self.target.as_ref().unwrap())
                            .id("conflict-target")
                            .flex_1()
                            .disabled(busy || !can_start),
                    )
                    .child(
                        Button::new("conflict-start-merge")
                            .label("合并到当前分支")
                            .disabled(busy || !can_start)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let target = this
                                    .target
                                    .as_ref()
                                    .unwrap()
                                    .read(cx)
                                    .value()
                                    .trim()
                                    .to_owned();
                                if !target.is_empty() {
                                    this.submit(ConflictAction::Merge { target }, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("conflict-start-rebase")
                            .label("当前分支变基到…")
                            .disabled(busy || !can_start)
                            .on_click(cx.listener(|this, _, _, cx| {
                                let onto = this
                                    .target
                                    .as_ref()
                                    .unwrap()
                                    .read(cx)
                                    .value()
                                    .trim()
                                    .to_owned();
                                if !onto.is_empty() {
                                    this.submit(ConflictAction::Rebase { onto }, cx);
                                }
                            })),
                    ),
            );
        let mut files = div()
            .id("conflict-file-list")
            .test_support()
            .flex()
            .flex_wrap()
            .gap(px(6.));
        if let Some(state) = &self.state {
            for (index, file) in state.files.iter().enumerate() {
                let path = file.path.clone();
                files = files.child(
                    Button::new(("conflict-file", index))
                        .label(path.clone())
                        .when(self.selected.as_ref() == Some(&path), |button| {
                            button.primary()
                        })
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, _, cx| this.select(path.clone(), cx))),
                );
            }
        }
        form = form.child(
            div()
                .id("conflict-files-scroll")
                .max_h(px(84.))
                .overflow_y_scroll()
                .child(files),
        );
        if self.loading_file {
            form = form.child(div().text_color(muted).child("正在读取所选文件的三个版本…"));
        }
        if let (Some(versions), Some(editors)) = (&self.versions, &self.editors) {
            let mut sides = div().flex().gap(px(10.));
            for (index, label) in ["共同基础 · Base", "当前侧 · Ours", "传入侧 · Theirs"]
                .into_iter()
                .enumerate()
            {
                sides = sides.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(5.))
                        .child(label)
                        .child(
                            div()
                                .id(SharedString::from(format!("conflict-side-{index}")))
                                .test_support()
                                .h(px(165.))
                                .w_full()
                                .child(
                                    Textarea::new(&editors.sides[index])
                                        .readonly(true)
                                        .h(px(165.))
                                        .w_full(),
                                ),
                        ),
                );
            }
            form=form.child(div().font_weight(FontWeight::SEMIBOLD).child(versions.path.clone())).child(sides)
                .child(div().flex().gap(px(8.))
                    .child(Button::new("conflict-use-ours").label(if versions.ours.is_some(){"采用 Ours 并标记解决"}else{"采用 Ours 删除文件"}).disabled(busy)
                        .on_click(cx.listener(|this,_,_,cx|this.resolve(ConflictResolution::Ours,cx))))
                    .child(Button::new("conflict-use-theirs").label(if versions.theirs.is_some(){"采用 Theirs 并标记解决"}else{"采用 Theirs 删除文件"}).disabled(busy)
                        .on_click(cx.listener(|this,_,_,cx|this.resolve(ConflictResolution::Theirs,cx)))))
                .child(div().child("最终合并结果（可编辑，保存后会暂存并标记解决）"))
                .child(div().id("conflict-result").test_support().h(px(170.)).w_full()
                    .child(Textarea::new(&editors.result).h(px(170.)).w_full().disabled(busy||versions.binary)))
                .child(div().flex().gap(px(8.))
                    .child(Button::new("conflict-save-result").label("保存结果并标记解决").primary().disabled(busy||versions.binary)
                        .on_click(cx.listener(|this,_,_,cx|{if let Some(editors)=&this.editors{let text=editors.result.read(cx).value().to_string();this.resolve(ConflictResolution::Edited(text),cx);}})))
                    .child(Button::new("conflict-reload-file").label("重新读取文件").disabled(busy)
                        .on_click(cx.listener(|this,_,_,cx|{if let Some(path)=this.selected.clone(){this.select(path,cx);}}))))
                .child(div().text_color(muted).child("采用版本或保存结果前会备份原工作内容。变基时 Ours 是目标分支，Theirs 是正在重放的提交。"));
        } else if self
            .state
            .as_ref()
            .is_some_and(|state| !state.files.is_empty())
            && !self.loading_file
        {
            form = form.child(
                div()
                    .text_color(muted)
                    .child("选择一个冲突文件，查看基础、当前侧、传入侧并编辑最终结果。"),
            );
        }
        let mut card = div()
            .w(px(1120.))
            .h(px(850.))
            .max_w(relative(0.96))
            .max_h(relative(0.95))
            .flex()
            .flex_col()
            .rounded(px(8.))
            .border_1()
            .border_color(border)
            .overflow_hidden()
            .bg(rgb(if light { 0xffffff } else { 0x161b22 }))
            .text_color(rgb(if light { 0x1f2328 } else { 0xf0f6fc }))
            .shadow_lg()
            .child(
                div()
                    .px(px(20.))
                    .py(px(14.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("合并、变基与冲突解决 · {}", self.name)),
                    )
                    .child(
                        div()
                            .id("conflict-repository-path")
                            .test_support()
                            .aria_label(display_path(&self.root))
                            .mt(px(6.))
                            .text_color(muted)
                            .child(display_path(&self.root)),
                    ),
            )
            .child(
                div()
                    .id("conflict-form-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(20.))
                    .py(px(14.))
                    .child(form),
            );
        if self.confirming_abort {
            let text = "确认中止本次合并或变基？本轮已暂存或编辑的解决内容将从工作区丢弃，并回到操作前提交。原提交和解决过程文件的备份仍会保留。";
            card = card.child(
                div()
                    .id("conflict-abort-confirmation")
                    .test_support()
                    .role(Role::Alert)
                    .aria_label(text)
                    .px(px(20.))
                    .py(px(10.))
                    .child(text)
                    .child(
                        div()
                            .mt(px(8.))
                            .flex()
                            .gap(px(8.))
                            .child(
                                Button::new("conflict-abort-cancel")
                                    .label("继续解决")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirming_abort = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("conflict-abort-confirm")
                                    .label("确认中止并恢复")
                                    .primary()
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if this.confirming_abort {
                                            this.submit(ConflictAction::Abort, cx);
                                        }
                                    })),
                            ),
                    ),
            );
        }
        if let Some(path) = self.confirming_selection.clone() {
            card = card.child(
                div()
                    .id("conflict-unsaved-confirmation")
                    .test_support()
                    .role(Role::Alert)
                    .aria_label("合并结果尚未保存，切换或重新读取会丢弃编辑器中的修改")
                    .px(px(20.))
                    .py(px(10.))
                    .child("合并结果尚未保存，切换或重新读取会丢弃编辑器中的修改。")
                    .child(
                        div()
                            .mt(px(8.))
                            .flex()
                            .gap(px(8.))
                            .child(
                                Button::new("conflict-unsaved-cancel")
                                    .label("保留编辑")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirming_selection = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("conflict-unsaved-discard")
                                    .label("放弃编辑并读取")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.load_file(path.clone(), cx)
                                    })),
                            ),
                    ),
            );
        }
        if let Some(error) = &self.error {
            card = card.child(
                div()
                    .id("conflict-error")
                    .test_support()
                    .role(Role::Alert)
                    .aria_label(error.clone())
                    .px(px(20.))
                    .py(px(8.))
                    .text_color(rgb(if light { 0xcf222e } else { 0xf85149 }))
                    .child(error.clone()),
            );
        }
        card = card.child(
            div()
                .px(px(20.))
                .py(px(12.))
                .border_t_1()
                .border_color(border)
                .flex()
                .justify_between()
                .child(
                    Button::new("conflict-reload")
                        .label(if self.busy {
                            "处理中…"
                        } else {
                            "重新读取状态"
                        })
                        .disabled(busy || self.dirty_editor(cx))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.busy {
                                this.set_busy(true, cx);
                                cx.emit(ConflictDialogEvent::Reload);
                            }
                        })),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            Button::new("conflict-abort")
                                .label("中止操作…")
                                .disabled(busy || !can_abort)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if !this.busy {
                                        this.confirming_abort = true;
                                        cx.notify();
                                    }
                                })),
                        )
                        .child(
                            Button::new("conflict-continue")
                                .label("继续完成操作")
                                .primary()
                                .disabled(busy || !can_continue)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.submit(ConflictAction::Continue, cx)
                                })),
                        )
                        .child(
                            Button::new("conflict-close")
                                .label("关闭")
                                .disabled(self.busy || self.loading_file)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.close(window, cx)),
                                ),
                        ),
                ),
        );
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba(0x00000088))
            .key_context("GitViewsConflicts")
            .track_focus(&self.focus)
            .tab_group()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .font_family("Noto Sans SC")
            .on_action(
                cx.listener(|this, _: &CloseConflictDialog, window, cx| this.close(window, cx)),
            )
            .child(card)
            .focus_trap("gitviews-conflict-trap", &self.focus)
            .into_any_element()
    }
}
