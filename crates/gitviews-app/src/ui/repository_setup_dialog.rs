//! Start a repository without first opening an existing working tree.
use gitviews_service::{RepositorySetupKind, RepositorySetupRequest};
use gpui_kit::{
    base::FocusTrapElement as _,
    component::{
        ActiveTheme, Disableable as _, ThemeMode,
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
    },
    *,
};
use std::path::PathBuf;

gpui_kit::actions!(gitviews_repository_setup, [CloseRepositorySetup]);

pub(super) enum RepositorySetupEvent {
    Run(RepositorySetupRequest),
    Browse,
    Cancel,
    Close,
}
struct Inputs {
    source: Entity<InputState>,
    destination: Entity<InputState>,
    branch: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}
pub(super) struct RepositorySetupDialog {
    kind: Option<RepositorySetupKind>,
    inputs: Option<Inputs>,
    busy: bool,
    suspended: bool,
    progress: String,
    error: Option<String>,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    system_git: bool,
}
impl EventEmitter<RepositorySetupEvent> for RepositorySetupDialog {}
impl RepositorySetupDialog {
    pub(super) fn new(cx: &mut Context<Self>) -> Self {
        cx.bind_keys([KeyBinding::new(
            "escape",
            CloseRepositorySetup,
            Some("GitViewsRepositorySetup"),
        )]);
        Self {
            kind: None,
            inputs: None,
            busy: false,
            suspended: false,
            progress: String::new(),
            error: None,
            focus: cx.focus_handle(),
            previous_focus: None,
            system_git: false,
        }
    }
    pub(super) fn show(
        &mut self,
        kind: RepositorySetupKind,
        system_git: bool,
        cx: &mut Context<Self>,
    ) {
        self.kind = Some(kind);
        self.inputs = None;
        self.busy = false;
        self.suspended = false;
        self.error = None;
        self.system_git = system_git;
        self.previous_focus = None;
        cx.notify();
    }
    pub(super) fn is_open(&self) -> bool {
        self.kind.is_some()
    }
    pub(super) fn clear(&mut self, cx: &mut Context<Self>) {
        self.kind = None;
        self.inputs = None;
        self.busy = false;
        self.suspended = false;
        self.previous_focus = None;
        cx.notify();
    }
    pub(super) fn suspend(&mut self, cx: &mut Context<Self>) {
        self.suspended = true;
        cx.notify();
    }
    pub(super) fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        self.suspended = false;
        self.error = None;
        if busy {
            self.progress = "正在准备仓库…".into();
        }
        cx.notify();
    }
    pub(super) fn set_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.busy = false;
        self.suspended = false;
        self.error = Some(message);
        cx.notify();
    }
    pub(super) fn set_progress(&mut self, message: String, cx: &mut Context<Self>) {
        if self.progress != message {
            self.progress = message;
            cx.notify();
        }
    }
    pub(super) fn set_destination(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.busy
            && let Some(inputs) = &self.inputs
        {
            inputs.destination.update(cx, |state, cx| {
                state.set_value(gitviews_service::display_path(&path), window, cx)
            });
        }
    }
    fn ensure_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.inputs.is_some() {
            return;
        }
        let source = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://example.com/team/repo.git 或 git@host:team/repo.git")
        });
        let destination = cx.new(|cx| {
            InputState::new(window, cx).placeholder("新仓库的绝对路径，例如 D:\\Projects\\my-repo")
        });
        let branch = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("main")
                .default_value("main")
        });
        let subscriptions = [&source, &destination, &branch]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.submit(cx);
                    } else if matches!(event, InputEvent::Change) {
                        this.error = None;
                        cx.notify();
                    }
                })
            })
            .collect();
        self.previous_focus = window.focused(cx);
        if self.kind == Some(RepositorySetupKind::Clone) {
            source.read(cx).focus_handle(cx).focus(window, cx);
        } else {
            destination.read(cx).focus_handle(cx).focus(window, cx);
        }
        self.inputs = Some(Inputs {
            source,
            destination,
            branch,
            _subscriptions: subscriptions,
        });
    }
    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let (Some(kind), Some(inputs)) = (self.kind, &self.inputs) else {
            return;
        };
        let request = RepositorySetupRequest {
            kind,
            source: inputs.source.read(cx).value().trim().to_owned(),
            destination: PathBuf::from(
                inputs.destination.read(cx).value().trim().trim_matches('"'),
            ),
            initial_branch: inputs.branch.read(cx).value().trim().to_owned(),
        };
        if !request.destination.is_absolute() {
            self.error = Some("请填写仓库的完整绝对路径。".into());
            cx.notify();
            return;
        }
        if kind == RepositorySetupKind::Clone && request.source.is_empty() {
            self.error = Some("请填写要克隆的仓库地址或本地路径。".into());
            cx.notify();
            return;
        }
        cx.emit(RepositorySetupEvent::Run(request));
    }
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            cx.emit(RepositorySetupEvent::Cancel);
        } else {
            if let Some(focus) = self.previous_focus.take() {
                focus.focus(window, cx);
            }
            cx.emit(RepositorySetupEvent::Close);
        }
    }
}
impl Render for RepositorySetupDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(kind) = self.kind else {
            return div().into_any_element();
        };
        if self.suspended {
            return div().into_any_element();
        }
        self.ensure_inputs(window, cx);
        let inputs = self.inputs.as_ref().unwrap();
        let light = cx.theme().mode == ThemeMode::Light;
        let muted = rgb(if light { 0x59636e } else { 0x9198a1 });
        let clone = kind == RepositorySetupKind::Clone;
        let mut card = div()
            .id("repository-setup-card")
            .test_support()
            .w(px(620.))
            .p(px(24.))
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(if light { 0xd0d7de } else { 0x3d444d }))
            .bg(rgb(if light { 0xffffff } else { 0x212830 }))
            .shadow_lg()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(div().text_size(px(19.)).child(if clone {
                "克隆仓库"
            } else {
                "新建仓库"
            }))
            .child(div().text_color(muted).child(if self.system_git {
                "使用本地 Git。沿用 Git 的凭据管理器、SSH 配置和 hooks。"
            } else {
                "使用内置 Git。HTTPS 令牌或 SSH 密钥在需要认证时输入，仅用于本次克隆。"
            }));
        if clone {
            card = card
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .child("仓库地址或本地仓库路径")
                        .child(
                            Input::new(&inputs.source)
                                .id("repository-setup-source")
                                .disabled(self.busy)
                                .tab_index(1)
                                .h(px(34.)),
                        ),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child("地址中请勿包含密码、令牌或查询参数。支持 HTTPS、SSH 和本地仓库。"),
                );
        }
        card = card
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child("目标仓库目录")
                    .child(
                        Input::new(&inputs.destination)
                            .id("repository-setup-destination")
                            .disabled(self.busy)
                            .tab_index(2)
                            .h(px(34.)),
                    ),
            )
            .child(
                Button::new("repository-setup-browse")
                    .label("选择空目录…")
                    .disabled(self.busy)
                    .tab_index(3)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(RepositorySetupEvent::Browse))),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child("目录必须不存在或为空。父文件夹需要已存在；已有文件会被保护。"),
            );
        if !clone {
            card = card
                .child(
                    div().flex().flex_col().gap(px(6.)).child("初始分支").child(
                        Input::new(&inputs.branch)
                            .id("repository-setup-branch")
                            .disabled(self.busy)
                            .tab_index(4)
                            .h(px(34.)),
                    ),
                )
                .child(
                    div().text_size(px(11.)).text_color(muted).child(
                        "创建后，可在“仓库 → Git 设置”中配置提交姓名和邮箱，再创建首次提交。",
                    ),
                );
        }
        if self.busy {
            card = card.child(
                div()
                    .id("repository-setup-progress")
                    .test_support()
                    .role(Role::Status)
                    .child(self.progress.clone()),
            );
        }
        if let Some(error) = &self.error {
            card = card.child(
                div()
                    .id("repository-setup-error")
                    .test_support()
                    .role(Role::Alert)
                    .text_color(rgb(if light { 0xcf222e } else { 0xf85149 }))
                    .child(error.clone()),
            );
        }
        card = card.child(
            div()
                .flex()
                .justify_end()
                .gap(px(8.))
                .child(
                    Button::new("repository-setup-close")
                        .label(if self.busy { "请求取消" } else { "取消" })
                        .tab_index(5)
                        .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
                )
                .child(
                    Button::new("repository-setup-submit")
                        .label(if self.busy {
                            "正在创建…"
                        } else if clone {
                            "克隆并打开"
                        } else {
                            "创建并打开"
                        })
                        .primary()
                        .disabled(self.busy)
                        .tab_index(6)
                        .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                ),
        );
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba(0x00000055))
            .key_context("GitViewsRepositorySetup")
            .track_focus(&self.focus)
            .focus_trap("repository-setup-focus-trap", &self.focus)
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .font_family("Noto Sans SC")
            .on_action(
                cx.listener(|this, _: &CloseRepositorySetup, window, cx| this.close(window, cx)),
            )
            .child(card)
            .into_any_element()
    }
}
