//! First-run Git engine choice and executable discovery.
use gitviews_service::{GitEngineKind, GitEnginePreferences, SystemGitInstallation, display_path};
use gpui_kit::{
    TestSupportExt as _,
    base::FocusTrapElement as _,
    component::{
        ActiveTheme, Disableable as _, ThemeMode,
        button::{Button, ButtonVariants as _},
        input::{Input, InputState},
    },
    prelude::FluentBuilder as _,
    *,
};
use std::path::PathBuf;

gpui_kit::actions!(gitviews_engine, [CloseGitEngine]);

pub(crate) enum GitEngineEvent {
    Detect,
    Browse,
    Apply(GitEnginePreferences),
    Recover,
    Close,
}

pub(crate) struct GitEngineDialog {
    open: bool,
    required: bool,
    selection: Option<GitEngineKind>,
    executable: String,
    input: Option<Entity<InputState>>,
    candidates: Vec<SystemGitInstallation>,
    discovery_complete: bool,
    busy: bool,
    error: Option<String>,
    recovery_available: bool,
    recovery_message: Option<String>,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
}

impl EventEmitter<GitEngineEvent> for GitEngineDialog {}

impl GitEngineDialog {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        cx.bind_keys([KeyBinding::new(
            "escape",
            CloseGitEngine,
            Some("GitViewsEngine"),
        )]);
        Self {
            open: false,
            required: false,
            selection: None,
            executable: String::new(),
            input: None,
            candidates: Vec::new(),
            discovery_complete: false,
            busy: false,
            error: None,
            recovery_available: false,
            recovery_message: None,
            focus: cx.focus_handle(),
            previous_focus: None,
        }
    }

    pub(crate) fn show(
        &mut self,
        preferences: GitEnginePreferences,
        required: bool,
        cx: &mut Context<Self>,
    ) {
        self.open = true;
        self.required = required;
        self.selection = preferences.engine;
        self.executable = preferences
            .executable
            .as_deref()
            .map(display_path)
            .unwrap_or_default();
        self.input = None;
        self.error = None;
        self.recovery_available = false;
        self.recovery_message = None;
        self.busy = false;
        self.candidates.clear();
        self.discovery_complete = false;
        cx.notify();
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }
    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.input = None;
        self.previous_focus = None;
        cx.notify();
    }
    pub(crate) fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        self.error = None;
        cx.notify();
    }
    pub(crate) fn set_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.busy = false;
        self.error = Some(error);
        cx.notify();
    }
    pub(crate) fn set_recovery_available(&mut self, available: bool, cx: &mut Context<Self>) {
        self.recovery_available = available;
        cx.notify();
    }
    pub(crate) fn set_recovered(&mut self, backup: Option<PathBuf>, cx: &mut Context<Self>) {
        self.recovery_available = false;
        self.busy = false;
        self.error = None;
        self.recovery_message = Some(match backup {
            Some(path) => format!(
                "损坏的配置已完整备份至：{}。请选择 Git 方案并保存。",
                display_path(&path)
            ),
            None => "配置文件已可读取，请重新选择 Git 方案并保存。".into(),
        });
        cx.notify();
    }
    pub(crate) fn set_candidates(
        &mut self,
        candidates: Vec<SystemGitInstallation>,
        cx: &mut Context<Self>,
    ) {
        self.busy = false;
        self.candidates = candidates;
        self.discovery_complete = true;
        cx.notify();
    }
    pub(crate) fn select_executable(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.executable = display_path(&path);
        self.selection = Some(GitEngineKind::System);
        if let Some(input) = &self.input {
            input.update(cx, |input, cx| {
                input.set_value(self.executable.clone(), window, cx)
            });
        }
        cx.notify();
    }
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.required || self.busy {
            return;
        }
        if let Some(previous) = self.previous_focus.take() {
            previous.focus(window, cx);
        }
        self.clear(cx);
        cx.emit(GitEngineEvent::Close);
    }
    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(engine) = self.selection else {
            self.set_error("请先选择内置 Git 或本地 Git。".into(), cx);
            return;
        };
        let executable = self
            .input
            .as_ref()
            .map(|input| input.read(cx).value().trim().trim_matches('"').to_owned())
            .unwrap_or_else(|| self.executable.clone());
        if engine == GitEngineKind::System && executable.is_empty() {
            self.set_error(
                "请自动检测并选择 Git，或手动填写 git.exe 的完整路径。".into(),
                cx,
            );
            return;
        }
        self.busy = true;
        self.error = None;
        cx.emit(GitEngineEvent::Apply(GitEnginePreferences {
            engine: Some(engine),
            executable: (engine == GitEngineKind::System).then(|| PathBuf::from(executable)),
        }));
        cx.notify();
    }
}

impl Render for GitEngineDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        if self.input.is_none() {
            self.previous_focus = window.focused(cx);
            self.input = Some(cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("例如 C:\\Program Files\\Git\\cmd\\git.exe")
                    .default_value(self.executable.clone())
            }));
            let focus = self.focus.clone();
            cx.defer_in(window, move |_, window, cx| focus.focus(window, cx));
        }
        let light = cx.theme().mode == ThemeMode::Light;
        let border = rgb(if light { 0xd0d7de } else { 0x30363d });
        let muted = rgb(if light { 0x59636e } else { 0x9198a1 });
        let mut body = div().flex().flex_col().gap(px(12.))
            .child("选择此设备使用的 Git 方案。可随时从“仓库 → Git 方案”重新选择。")
            .child(Button::new("git-engine-builtin").label(if self.selection == Some(GitEngineKind::BuiltIn) { "● 内置 Git · 无需安装" } else { "○ 内置 Git · 无需安装" }).disabled(self.busy).tab_index(1).on_click(cx.listener(|this, _, _, cx| { this.selection = Some(GitEngineKind::BuiltIn); cx.notify(); })))
            .child(div().text_color(muted).child("使用随程序提供的 libgit2，适合基础本地操作。认证使用应用的 HTTPS／SSH 窗口；不执行 Git hooks，不创建 GPG／SSH 签名提交，需要这些能力时会提示改用本地 Git。"))
            .child(Button::new("git-engine-system").label(if self.selection == Some(GitEngineKind::System) { "● 本地 Git · 兼容团队流程" } else { "○ 本地 Git · 兼容团队流程" }).disabled(self.busy).tab_index(2).on_click(cx.listener(|this, _, _, cx| { this.selection = Some(GitEngineKind::System); cx.notify(); })))
            .child(div().text_color(muted).child("使用选定的 Git 执行提交、远端等兼容操作，沿用该 Git 的 hooks、签名设置、凭据助手与 SSH 配置。签名工具和凭据助手需在系统中可用；浏览、差异与搜索仍由内置引擎加速。仓库中的 hooks 会在相应操作时执行。"));
        if self.selection == Some(GitEngineKind::System) {
            body = body
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            Button::new("git-engine-detect")
                                .label("自动检测 Git")
                                .disabled(self.busy)
                                .tab_index(3)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_busy(true, cx);
                                    cx.emit(GitEngineEvent::Detect);
                                })),
                        )
                        .child(
                            Button::new("git-engine-browse")
                                .label("手动选择文件…")
                                .disabled(self.busy)
                                .tab_index(4)
                                .on_click(
                                    cx.listener(|_, _, _, cx| cx.emit(GitEngineEvent::Browse)),
                                ),
                        ),
                )
                .child(
                    Input::new(self.input.as_ref().unwrap())
                        .id("git-engine-executable")
                        .aria_label("本地 Git 可执行文件的完整路径")
                        .w_full()
                        .h(px(34.))
                        .disabled(self.busy)
                        .tab_index(5),
                )
                .child(div().text_size(px(11.)).text_color(muted).child(
                    "自动检测 PATH 与常见安装位置；保存前验证文件和 Git 版本。路径可直接编辑。",
                ));
            if self.discovery_complete && self.candidates.is_empty() {
                body = body.child(
                    div()
                        .text_color(muted)
                        .child("未检测到 Git。请先安装 Git，或手动选择已有的 git.exe。"),
                );
            }
            for (index, candidate) in self.candidates.iter().enumerate() {
                let path = candidate.executable.clone();
                body = body.child(
                    Button::new(("git-engine-candidate", index))
                        .label(format!("{} · {}", candidate.version, display_path(&path)))
                        .disabled(self.busy)
                        .tab_index(10 + index as isize)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select_executable(path.clone(), window, cx)
                        })),
                );
            }
        }
        let card = div()
            .w(px(780.))
            .h(px(680.))
            .max_w(relative(0.94))
            .max_h(relative(0.94))
            .flex()
            .flex_col()
            .rounded(px(8.))
            .overflow_hidden()
            .border_1()
            .border_color(border)
            .bg(rgb(if light { 0xffffff } else { 0x161b22 }))
            .text_color(rgb(if light { 0x1f2328 } else { 0xf0f6fc }))
            .shadow_lg()
            .child(
                div()
                    .px(px(22.))
                    .py(px(16.))
                    .border_b_1()
                    .border_color(border)
                    .text_size(px(18.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if self.required {
                        "首次使用 · 选择 Git 方案"
                    } else {
                        "Git 方案"
                    }),
            )
            .child(
                div()
                    .id("git-engine-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p(px(22.))
                    .child(body),
            )
            .when_some(self.error.clone(), |card, error| {
                card.child(
                    div()
                        .id("git-engine-error")
                        .test_support()
                        .role(Role::Alert)
                        .aria_label(error.clone())
                        .px(px(22.))
                        .pb(px(10.))
                        .text_color(rgb(if light { 0xcf222e } else { 0xf85149 }))
                        .child(error),
                )
            })
            .when(self.recovery_available, |card| {
                card.child(
                    div().px(px(22.)).pb(px(12.)).child(
                        Button::new("git-engine-recover")
                            .label("备份损坏配置并重选")
                            .disabled(self.busy)
                            .tab_index(99)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.busy {
                                    this.set_busy(true, cx);
                                    cx.emit(GitEngineEvent::Recover);
                                }
                            })),
                    ),
                )
            })
            .when_some(self.recovery_message.clone(), |card, message| {
                card.child(
                    div()
                        .id("git-engine-recovery-message")
                        .test_support()
                        .role(Role::Status)
                        .aria_label(message.clone())
                        .px(px(22.))
                        .pb(px(10.))
                        .text_color(muted)
                        .child(message),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .px(px(22.))
                    .py(px(14.))
                    .border_t_1()
                    .border_color(border)
                    .when(!self.required, |footer| {
                        footer.child(
                            Button::new("git-engine-cancel")
                                .label("取消")
                                .disabled(self.busy)
                                .tab_index(100)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.close(window, cx)),
                                ),
                        )
                    })
                    .child(
                        Button::new("git-engine-save")
                            .label(if self.busy {
                                "正在检测或保存…"
                            } else {
                                "保存并使用"
                            })
                            .primary()
                            .disabled(self.busy || self.selection.is_none())
                            .tab_index(101)
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            );
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba(0x00000088))
            .key_context("GitViewsEngine")
            .track_focus(&self.focus)
            .tab_group()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .font_family("Noto Sans SC")
            .on_action(cx.listener(|this, _: &CloseGitEngine, window, cx| this.close(window, cx)))
            .child(card)
            .focus_trap("git-engine-focus-trap", &self.focus)
            .into_any_element()
    }
}
