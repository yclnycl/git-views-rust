//! Repository-scoped branch management. Mutations are dispatched by Workspace.
use std::path::PathBuf;

use gitviews_service::{BranchAction, BranchInfo, RepositoryDetails, display_path};
use gpui_kit::{
    TestSupportExt as _,
    base::FocusTrapElement as _,
    component::{
        ActiveTheme, Disableable as _, ThemeMode,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        input::{Input, InputEvent, InputState},
    },
    prelude::FluentBuilder as _,
    *,
};

gpui_kit::actions!(gitviews_branches, [CloseBranchDialog]);

pub(crate) enum BranchDialogEvent {
    Request(BranchAction),
    Reload,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Create,
    Rename,
    Delete,
    Upstream,
}
impl Mode {
    const ALL: [Self; 4] = [Self::Create, Self::Rename, Self::Delete, Self::Upstream];
    fn label(self) -> &'static str {
        match self {
            Self::Create => "创建",
            Self::Rename => "重命名",
            Self::Delete => "删除",
            Self::Upstream => "设置上游",
        }
    }
    fn id(self) -> &'static str {
        match self {
            Self::Create => "branch-mode-create",
            Self::Rename => "branch-mode-rename",
            Self::Delete => "branch-mode-delete",
            Self::Upstream => "branch-mode-upstream",
        }
    }
}

struct FormInputs {
    name: Entity<InputState>,
    start: Entity<InputState>,
    upstream: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

pub(crate) struct BranchDialog {
    open: bool,
    root: PathBuf,
    details: RepositoryDetails,
    selected: Option<String>,
    mode: Mode,
    force: bool,
    busy: bool,
    confirming: Option<BranchAction>,
    inputs: Option<FormInputs>,
    error: Option<String>,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
}
impl EventEmitter<BranchDialogEvent> for BranchDialog {}

impl BranchDialog {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        cx.bind_keys([KeyBinding::new(
            "escape",
            CloseBranchDialog,
            Some("GitViewsBranches"),
        )]);
        Self {
            open: false,
            root: PathBuf::new(),
            details: RepositoryDetails::default(),
            selected: None,
            mode: Mode::Create,
            force: false,
            busy: false,
            confirming: None,
            inputs: None,
            error: None,
            focus: cx.focus_handle(),
            previous_focus: None,
        }
    }
    pub(crate) fn show(
        &mut self,
        root: PathBuf,
        details: RepositoryDetails,
        cx: &mut Context<Self>,
    ) {
        self.root = root;
        self.selected = details
            .branches
            .iter()
            .find(|b| b.is_current && !b.is_remote)
            .or_else(|| details.branches.iter().find(|b| !b.is_remote))
            .map(|b| b.name.clone());
        self.details = details;
        self.mode = Mode::Create;
        self.force = false;
        self.busy = false;
        self.confirming = None;
        self.inputs = None;
        self.error = None;
        self.previous_focus = None;
        self.open = true;
        cx.notify();
    }
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }
    pub(crate) fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        if busy {
            self.error = None;
        }
        cx.notify();
    }
    pub(crate) fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.busy = false;
        self.confirming = None;
        self.error = error;
        cx.notify();
    }
    pub(crate) fn replace_details(&mut self, details: RepositoryDetails, cx: &mut Context<Self>) {
        let exists = self.selected.as_ref().is_some_and(|name| {
            details
                .branches
                .iter()
                .any(|b| !b.is_remote && &b.name == name)
        });
        self.details = details;
        if !exists {
            self.selected = self
                .details
                .branches
                .iter()
                .find(|b| !b.is_remote)
                .map(|b| b.name.clone());
            self.mode = Mode::Create;
        }
        self.inputs = None;
        self.busy = false;
        self.confirming = None;
        self.force = false;
        self.error = None;
        cx.notify();
    }
    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.inputs = None;
        self.confirming = None;
        self.force = false;
        self.busy = false;
        self.error = None;
        self.previous_focus = None;
        cx.notify();
    }
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if self.confirming.take().is_some() {
            cx.notify();
            return;
        }
        let previous = self.previous_focus.take();
        self.clear(cx);
        if let Some(previous) = previous {
            previous.focus(window, cx);
        }
        cx.emit(BranchDialogEvent::Closed);
    }
    fn selected_branch(&self) -> Option<&BranchInfo> {
        self.details
            .branches
            .iter()
            .find(|b| !b.is_remote && self.selected.as_deref() == Some(&b.name))
    }
    fn occupied(&self) -> bool {
        self.selected_branch().is_some_and(|b| {
            b.is_current
                || self
                    .details
                    .worktrees
                    .iter()
                    .any(|w| w.branch.as_deref() == Some(&b.name))
        })
    }
    fn available(&self, mode: Mode) -> bool {
        !self.busy
            && self.confirming.is_none()
            && (mode == Mode::Create
                || self.selected_branch().is_some()
                    && (!matches!(mode, Mode::Rename | Mode::Delete) || !self.occupied()))
    }
    fn reset_form(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if !self.available(mode) {
            return;
        }
        self.mode = mode;
        self.inputs = None;
        self.confirming = None;
        self.force = false;
        self.error = None;
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
        let name = cx.new(|cx| {
            InputState::new(window, cx).placeholder("本地分支短名称，例如 feature/new-work")
        });
        let start = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("留空从当前 HEAD 创建，也可填写分支、标签或提交")
        });
        let upstream =
            cx.new(|cx| InputState::new(window, cx).placeholder("例如 origin/main；留空取消上游"));
        if let Some(current) = self.selected_branch().and_then(|b| b.upstream.as_ref()) {
            upstream.update(cx, |input, cx| input.set_value(current.clone(), window, cx));
        }
        let subscriptions = [&name, &start, &upstream]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, event, _, cx| match event {
                    InputEvent::Change => {
                        this.error = None;
                        this.confirming = None;
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => this.submit(cx),
                    _ => {}
                })
            })
            .collect();
        self.inputs = Some(FormInputs {
            name,
            start,
            upstream,
            _subscriptions: subscriptions,
        });
    }
    fn build_action(&self, cx: &App) -> Result<BranchAction, String> {
        let inputs = self.inputs.as_ref().ok_or("请先填写分支信息。")?;
        let name = inputs.name.read(cx).value().to_string();
        let nonempty = |value: String| (!value.trim().is_empty()).then(|| value.trim().to_owned());
        if self.mode == Mode::Create {
            let name = nonempty(name).ok_or("请填写新分支名称。")?;
            return Ok(BranchAction::Create {
                name,
                start_point: nonempty(inputs.start.read(cx).value().to_string()),
            });
        }
        let selected = self
            .selected_branch()
            .ok_or("请选择一个本地分支。")?
            .name
            .clone();
        if matches!(self.mode, Mode::Rename | Mode::Delete) && self.occupied() {
            return Err("此分支正在当前或其他工作树中使用，请先切换该工作树的分支。".into());
        }
        Ok(match self.mode {
            Mode::Create => unreachable!(),
            Mode::Rename => BranchAction::Rename {
                old: selected,
                new: nonempty(name).ok_or("请填写重命名后的分支名称。")?,
            },
            Mode::Delete => BranchAction::Delete {
                name: selected,
                force: self.force,
            },
            Mode::Upstream => BranchAction::SetUpstream {
                name: selected,
                upstream: nonempty(inputs.upstream.read(cx).value().to_string()),
            },
        })
    }
    fn submit(&mut self, cx: &mut Context<Self>) {
        if !self.available(self.mode) {
            return;
        }
        match self.build_action(cx) {
            Ok(action @ BranchAction::Delete { .. }) => {
                self.confirming = Some(action);
                self.error = None;
                cx.notify();
            }
            Ok(action) => {
                self.set_busy(true, cx);
                cx.emit(BranchDialogEvent::Request(action));
            }
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }
    fn confirm(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if let Some(action) = self.confirming.clone() {
            self.set_busy(true, cx);
            cx.emit(BranchDialogEvent::Request(action));
        }
    }
    fn input(&self, input: &Entity<InputState>, id: &'static str, label: &'static str) -> Div {
        div().flex().flex_col().gap(px(6.)).child(label).child(
            Input::new(input)
                .id(id)
                .w_full()
                .h(px(32.))
                .disabled(self.busy || self.confirming.is_some()),
        )
    }
}

impl Render for BranchDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        self.ensure_inputs(window, cx);
        let light = cx.theme().mode == ThemeMode::Light;
        let muted: Hsla = rgb(if light { 0x59636e } else { 0x9198a1 }).into();
        let border = rgb(if light { 0xd0d7de } else { 0x30363d });
        let unavailable = self.busy || self.confirming.is_some();
        let branches: Vec<_> = self
            .details
            .branches
            .iter()
            .filter(|b| !b.is_remote)
            .cloned()
            .collect();
        let selected = self.selected.clone();
        let entity = cx.entity();
        let count = branches.len();
        let occupied: Vec<_> = self
            .details
            .worktrees
            .iter()
            .filter_map(|w| w.branch.clone())
            .collect();
        let list = uniform_list("branch-manager-list", count, move |range, _, _| {
            range
                .map(|index| {
                    let branch = &branches[index];
                    let name = branch.name.clone();
                    let target = entity.clone();
                    let suffix = if branch.is_current {
                        " · 当前"
                    } else if occupied.contains(&name) {
                        " · 工作树占用"
                    } else {
                        ""
                    };
                    Button::new(("branch-select", index))
                        .w_full()
                        .h(px(46.))
                        .ghost()
                        .when(selected.as_ref() == Some(&name), |b| b.primary())
                        .disabled(unavailable)
                        .label(format!(
                            "{}{suffix}{}",
                            name,
                            branch
                                .upstream
                                .as_ref()
                                .map_or(String::new(), |u| format!(" → {u}"))
                        ))
                        .on_click(move |_, _, cx| {
                            target.update(cx, |this, cx| {
                                if this.busy || this.confirming.is_some() {
                                    return;
                                }
                                this.selected = Some(name.clone());
                                this.mode = Mode::Create;
                                this.force = false;
                                this.inputs = None;
                                this.error = None;
                                cx.notify();
                            })
                        })
                })
                .collect::<Vec<_>>()
        })
        .h(px((count.clamp(1, 5) * 46) as f32))
        .w_full();
        let mut form = div().flex().flex_col().gap(px(12.));
        if self.mode != Mode::Create {
            form = form.child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                "目标分支：{}",
                self.selected.as_deref().unwrap_or("未选择")
            )));
        }
        if let Some(inputs) = &self.inputs {
            form = match self.mode {
                Mode::Create => form.child(self.input(&inputs.name, "branch-name", "新分支名称")).child(self.input(&inputs.start, "branch-start", "起点（可选）")).child(div().text_color(muted).child("创建分支不会切换当前工作目录。")),
                Mode::Rename => form.child(self.input(&inputs.name, "branch-name", "新分支名称")).child(div().text_color(muted).child("保留上游、分支配置和历史记录。被任意工作树占用的分支须先切换。")),
                Mode::Delete => form.child(div().text_color(muted).child("默认仅删除已合并到当前 HEAD 的分支。删除前保留分支提交作为恢复引用；工作目录和文件保持不变。")).child(Checkbox::new("branch-force-delete").label("强制删除未合并分支（仍保留恢复引用）").checked(self.force).disabled(unavailable).on_change(cx.listener(|this, value, _, cx| { if !this.busy && this.confirming.is_none() { this.force = *value; this.error = None; cx.notify(); } }))),
                Mode::Upstream => form.child(self.input(&inputs.upstream, "branch-upstream", "上游分支（留空取消）")).child(div().text_color(muted).child(format!("可选远端分支：{}", self.details.branches.iter().filter(|b| b.is_remote).take(8).map(|b| b.name.as_str()).collect::<Vec<_>>().join("、")))),
            };
        }
        if let Some(BranchAction::Delete { name, force }) = &self.confirming {
            let prompt = if *force {
                format!("确认强制删除分支 {name}？该分支可能包含未合并提交。删除前会保留恢复引用。")
            } else {
                format!("确认删除分支 {name}？仅在已合并且未被工作树占用时执行。")
            };
            form = form.child(
                div()
                    .id("branch-confirmation")
                    .test_support()
                    .role(Role::Alert)
                    .aria_label(prompt.clone())
                    .border_1()
                    .border_color(rgb(0xcf222e))
                    .p(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(prompt)
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(
                                Button::new("branch-confirm-cancel")
                                    .label("返回")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirming = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("branch-confirm-apply")
                                    .label("确认删除")
                                    .danger()
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, _, cx| this.confirm(cx))),
                            ),
                    ),
            );
        }
        let card =
            div()
                .w(px(820.))
                .max_w(relative(0.94))
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
                        .p(px(20.))
                        .border_b_1()
                        .border_color(border)
                        .child(
                            div()
                                .text_size(px(16.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("管理分支"),
                        )
                        .child(
                            div()
                                .id("branch-root-path")
                                .test_support()
                                .aria_label(display_path(&self.root))
                                .mt(px(6.))
                                .text_color(muted)
                                .child(display_path(&self.root)),
                        ),
                )
                .child(
                    div()
                        .id("branch-manager-body")
                        .min_h_0()
                        .overflow_y_scroll()
                        .p(px(20.))
                        .flex()
                        .flex_col()
                        .gap(px(14.))
                        .child(list)
                        .when(count == 0, |d| d.child("暂无本地分支，可从已有提交创建。"))
                        .child(div().flex().gap(px(6.)).children(Mode::ALL.into_iter().map(
                            |mode| {
                                Button::new(mode.id())
                                    .label(mode.label())
                                    .when(mode == self.mode, |b| b.primary())
                                    .disabled(!self.available(mode))
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.reset_form(mode, cx)
                                        }),
                                    )
                            },
                        )))
                        .when(self.occupied(), |d| {
                            d.child(
                                div()
                                    .text_color(muted)
                                    .child("所选分支正在工作树中使用，暂不允许删除或重命名。"),
                            )
                        })
                        .child(form)
                        .when_some(self.error.clone(), |d, error| {
                            d.child(
                                div()
                                    .id("branch-error")
                                    .test_support()
                                    .role(Role::Alert)
                                    .aria_label(error.clone())
                                    .text_color(rgb(0xcf222e))
                                    .child(error),
                            )
                        }),
                )
                .child(
                    div()
                        .p(px(16.))
                        .border_t_1()
                        .border_color(border)
                        .flex()
                        .justify_between()
                        .child(
                            Button::new("branch-reload")
                                .label("重新读取")
                                .disabled(unavailable)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_busy(true, cx);
                                    cx.emit(BranchDialogEvent::Reload);
                                })),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(
                                    Button::new("branch-close")
                                        .label("关闭")
                                        .disabled(self.busy)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.close(window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("branch-apply")
                                        .label(if self.busy {
                                            "执行中…"
                                        } else if self.mode == Mode::Delete {
                                            "删除…"
                                        } else {
                                            "执行操作"
                                        })
                                        .primary()
                                        .disabled(!self.available(self.mode))
                                        .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                                ),
                        ),
                );
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba(0x00000088))
            .key_context("GitViewsBranches")
            .track_focus(&self.focus)
            .tab_group()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .font_family("Noto Sans SC")
            .on_action(
                cx.listener(|this, _: &CloseBranchDialog, window, cx| this.close(window, cx)),
            )
            .child(card)
            .focus_trap("gitviews-branches-trap", &self.focus)
            .into_any_element()
    }
}

#[cfg(test)]
mod headless_tests {
    use super::{BranchAction, BranchDialog, BranchDialogEvent, BranchInfo, RepositoryDetails};
    use gitviews_service::WorktreeInfo;
    use gpui_kit::{
        App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render,
        Styled as _, Subscription, TestAppContext, Window, WindowHandle, base::Root,
        component::button::Button, div, px, size, test::TestWindowExt,
    };
    use std::{cell::RefCell, path::PathBuf, rc::Rc};

    struct Harness {
        dialog: Entity<BranchDialog>,
        requests: Rc<RefCell<Vec<BranchAction>>>,
        _subscription: Subscription,
    }
    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .relative()
                .size_full()
                .child(Button::new("branch-outside").label("Outside"))
                .child(self.dialog.clone())
        }
    }
    fn details() -> RepositoryDetails {
        RepositoryDetails {
            branches: [
                ("main", true, false),
                ("feature", false, false),
                ("occupied", false, false),
                ("origin/main", false, true),
            ]
            .into_iter()
            .map(|(name, is_current, is_remote)| BranchInfo {
                name: name.into(),
                commit_id: "0123456789abcdef".into(),
                subject: "fixture".into(),
                is_current,
                upstream: None,
                is_remote,
            })
            .collect(),
            worktrees: vec![WorktreeInfo {
                branch: Some("occupied".into()),
                ..WorktreeInfo::default()
            }],
        }
    }
    fn harness(cx: &mut TestAppContext) -> (WindowHandle<Root>, Entity<Harness>) {
        let (window, view) =
            crate::ui::headless::open_window(cx, size(px(1100.), px(950.)), |_, cx| {
                cx.new(|cx| {
                    let dialog = cx.new(|cx| {
                        let mut dialog = BranchDialog::new(cx);
                        dialog.show(PathBuf::from("C:/repository/中文"), details(), cx);
                        dialog
                    });
                    let requests = Rc::new(RefCell::new(Vec::new()));
                    let recorded = Rc::clone(&requests);
                    let subscription = cx.subscribe(&dialog, move |_, _, event, _| {
                        if let BranchDialogEvent::Request(action) = event {
                            recorded.borrow_mut().push(action.clone());
                        }
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
    fn input(window: &mut Window, id: &'static str, value: &str, cx: &mut App) {
        window.click(id, cx);
        window.press("ctrl-a", cx);
        window.input(value, cx);
    }
    #[gpui_kit::test]
    fn create_uses_explicit_start_and_busy_prevents_duplicate_requests(cx: &mut TestAppContext) {
        let (handle, harness) = harness(cx);
        cx.update_window(handle.into(), |_, window, cx| {
            input(window, "branch-name", "feature/中文", cx);
            input(window, "branch-start", "origin/main", cx);
            window.click("branch-apply", cx);
            window.click("branch-apply", cx);
            window.click("branch-close", cx);
            window.press("escape", cx);
            assert!(harness.read(cx).dialog.read(cx).is_open());
            harness.read(cx).dialog.clone().update(cx, |dialog, cx| {
                dialog.set_error(Some("模拟失败".into()), cx)
            });
            window.render_frame(cx);
            assert_eq!(window.find("branch-name").value(), Some("feature/中文"));
        })
        .unwrap();
        cx.read(|cx| {
            assert_eq!(
                *harness.read(cx).requests.borrow(),
                vec![BranchAction::Create {
                    name: "feature/中文".into(),
                    start_point: Some("origin/main".into())
                }]
            )
        });
    }
    #[gpui_kit::test]
    fn occupied_branches_are_protected_and_force_delete_requires_second_click(
        cx: &mut TestAppContext,
    ) {
        let (handle, harness) = harness(cx);
        cx.update_window(handle.into(), |_, window, cx| {
            for index in [0usize, 2] {
                window.click(("branch-select", index), cx);
                assert_eq!(window.find("branch-mode-delete").disabled(), Some(true));
                assert_eq!(window.find("branch-mode-rename").disabled(), Some(true));
            }
            window.click(("branch-select", 1usize), cx);
            window.click("branch-mode-delete", cx);
            assert!(!harness.read(cx).dialog.read(cx).force);
            window.click("branch-force-delete", cx);
            window.click("branch-apply", cx);
            assert!(harness.read(cx).requests.borrow().is_empty());
            assert!(
                window
                    .find("branch-confirmation")
                    .label()
                    .unwrap()
                    .contains("强制删除")
            );
            window.click("branch-confirm-apply", cx);
            window.click("branch-confirm-apply", cx);
        })
        .unwrap();
        cx.read(|cx| {
            assert_eq!(
                *harness.read(cx).requests.borrow(),
                vec![BranchAction::Delete {
                    name: "feature".into(),
                    force: true
                }]
            )
        });
    }
    #[gpui_kit::test]
    fn escape_cancels_confirmation_and_target_or_mode_change_clears_force(cx: &mut TestAppContext) {
        let (handle, harness) = harness(cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.click(("branch-select", 1usize), cx);
            window.click("branch-mode-delete", cx);
            window.click("branch-force-delete", cx);
            window.click("branch-apply", cx);
            window.press("escape", cx);
            assert!(harness.read(cx).requests.borrow().is_empty());
            assert!(harness.read(cx).dialog.read(cx).is_open());
            window.click("branch-mode-create", cx);
            window.click("branch-mode-delete", cx);
            assert!(!harness.read(cx).dialog.read(cx).force);
            window.click("branch-apply", cx);
            window.click("branch-confirm-cancel", cx);
            assert!(harness.read(cx).requests.borrow().is_empty());
        })
        .unwrap();
    }
    #[gpui_kit::test]
    fn rename_and_upstream_requests_are_scoped_to_selected_local_branch(cx: &mut TestAppContext) {
        let (handle, harness) = harness(cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.click(("branch-select", 1usize), cx);
            window.click("branch-mode-rename", cx);
            input(window, "branch-name", "renamed", cx);
            window.click("branch-apply", cx);
        })
        .unwrap();
        cx.read(|cx| {
            assert_eq!(
                *harness.read(cx).requests.borrow(),
                vec![BranchAction::Rename {
                    old: "feature".into(),
                    new: "renamed".into()
                }]
            )
        });
        cx.update_window(handle.into(), |_, window, cx| {
            harness
                .read(cx)
                .dialog
                .clone()
                .update(cx, |dialog, cx| dialog.set_error(None, cx));
            window.render_frame(cx);
            window.click("branch-mode-upstream", cx);
            input(window, "branch-upstream", "origin/main", cx);
            window.click("branch-apply", cx);
        })
        .unwrap();
        cx.read(|cx| {
            assert_eq!(
                harness.read(cx).requests.borrow()[1],
                BranchAction::SetUpstream {
                    name: "feature".into(),
                    upstream: Some("origin/main".into())
                }
            )
        });
    }
}
