//! Repository-local Git preferences. Secrets belong to the operation's auth prompt.

use std::path::PathBuf;

#[cfg(test)]
mod headless_tests;

use gitviews_service::{AuthProtocol, GitSettings, display_path, remote_protocol};
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

gpui_kit::actions!(gitviews_git_settings, [CloseGitSettingsDialog]);

pub(crate) enum GitSettingsEvent {
    Apply(GitSettings),
    DiscoverSshKeys,
    Close,
}

#[derive(Clone, Copy)]
enum RemoteField {
    FetchUrl,
    PushUrl,
    HttpsUsername,
    SshUsername,
    PrivateKey,
}

impl RemoteField {
    const ALL: [Self; 5] = [
        Self::FetchUrl,
        Self::PushUrl,
        Self::HttpsUsername,
        Self::SshUsername,
        Self::PrivateKey,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::FetchUrl => "git-settings-fetch-url",
            Self::PushUrl => "git-settings-push-url",
            Self::HttpsUsername => "git-settings-https-username",
            Self::SshUsername => "git-settings-ssh-username",
            Self::PrivateKey => "git-settings-private-key",
        }
    }
}

struct FormInputs {
    user_name: Entity<InputState>,
    user_email: Entity<InputState>,
    // Retain every remote's inputs so switching remotes cannot discard edits.
    remotes: Vec<Vec<Entity<InputState>>>,
    _subscriptions: Vec<Subscription>,
}

pub(crate) struct GitSettingsDialog {
    open: bool,
    root: PathBuf,
    draft: GitSettings,
    selected: usize,
    inputs: Option<FormInputs>,
    loaded: bool,
    loading: bool,
    saving: bool,
    discovering: bool,
    discovery_consent: bool,
    discovered_keys: Option<Vec<PathBuf>>,
    error: Option<String>,
    validation: Option<String>,
    previous_focus: Option<FocusHandle>,
    focus: FocusHandle,
}

impl EventEmitter<GitSettingsEvent> for GitSettingsDialog {}

impl GitSettingsDialog {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        cx.bind_keys([KeyBinding::new(
            "escape",
            CloseGitSettingsDialog,
            Some("GitViewsGitSettings"),
        )]);
        Self {
            open: false,
            root: PathBuf::new(),
            draft: GitSettings {
                user_name: String::new(),
                user_email: String::new(),
                remotes: Vec::new(),
            },
            selected: 0,
            inputs: None,
            loaded: false,
            loading: false,
            saving: false,
            discovering: false,
            discovery_consent: false,
            discovered_keys: None,
            error: None,
            validation: None,
            previous_focus: None,
            focus: cx.focus_handle(),
        }
    }

    pub(crate) fn show(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.open = true;
        self.root = root;
        self.inputs = None;
        self.selected = 0;
        self.loaded = false;
        self.loading = true;
        self.saving = false;
        self.discovering = false;
        self.discovery_consent = false;
        self.discovered_keys = None;
        self.error = None;
        self.validation = None;
        cx.notify();
    }

    pub(crate) fn set_settings(&mut self, settings: GitSettings, cx: &mut Context<Self>) {
        self.draft = settings;
        self.inputs = None;
        self.selected = 0;
        self.loaded = true;
        self.loading = false;
        self.error = None;
        self.validation = None;
        cx.notify();
    }

    pub(crate) fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.error = error;
        self.loading = false;
        self.saving = false;
        self.discovering = false;
        cx.notify();
    }

    pub(crate) fn set_saving(&mut self, saving: bool, cx: &mut Context<Self>) {
        self.saving = saving;
        if saving {
            self.error = None;
        }
        cx.notify();
    }

    pub(crate) fn set_discovering(&mut self, discovering: bool, cx: &mut Context<Self>) {
        self.discovering = discovering;
        self.discovery_consent = false;
        if discovering {
            self.error = None;
        }
        cx.notify();
    }

    pub(crate) fn set_discovered_keys(&mut self, keys: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.discovering = false;
        self.discovery_consent = false;
        self.discovered_keys = Some(keys);
        cx.notify();
    }

    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.inputs = None;
        self.loaded = false;
        self.loading = false;
        self.saving = false;
        self.discovering = false;
        self.discovery_consent = false;
        self.discovered_keys = None;
        self.error = None;
        self.validation = None;
        self.previous_focus = None;
        cx.notify();
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    fn busy(&self) -> bool {
        self.loading || self.saving
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let previous_focus = self.previous_focus.take();
        self.clear(cx);
        if let Some(focus) = previous_focus {
            focus.focus(window, cx);
        }
        cx.emit(GitSettingsEvent::Close);
    }

    fn ensure_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.previous_focus.is_none() {
            self.previous_focus = window.focused(cx);
            self.focus.focus(window, cx);
        }
        if !self.loaded || self.inputs.is_some() {
            return;
        }
        let mut subscriptions = Vec::new();
        let mut make_input = |value: String| {
            let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                |this, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.validation = None;
                        cx.notify();
                    }
                },
            ));
            input
        };
        let user_name = make_input(self.draft.user_name.clone());
        let user_email = make_input(self.draft.user_email.clone());
        let remotes = self
            .draft
            .remotes
            .iter()
            .map(|remote| {
                RemoteField::ALL
                    .into_iter()
                    .map(|field| {
                        make_input(match field {
                            RemoteField::FetchUrl => remote.url.clone(),
                            RemoteField::PushUrl => remote.push_url.clone().unwrap_or_default(),
                            RemoteField::HttpsUsername => remote.https_username.clone(),
                            RemoteField::SshUsername => remote.ssh_username.clone(),
                            RemoteField::PrivateKey => remote
                                .ssh_private_key
                                .as_ref()
                                .map(|path| display_path(path))
                                .unwrap_or_default(),
                        })
                    })
                    .collect()
            })
            .collect();
        self.inputs = Some(FormInputs {
            user_name,
            user_email,
            remotes,
            _subscriptions: subscriptions,
        });
    }

    fn remote_value(&self, field: RemoteField, cx: &App) -> String {
        self.inputs
            .as_ref()
            .and_then(|inputs| inputs.remotes.get(self.selected))
            .map(|inputs| inputs[field as usize].read(cx).value().trim().to_owned())
            .unwrap_or_default()
    }

    fn uses_ssh(&self, cx: &App) -> bool {
        [RemoteField::FetchUrl, RemoteField::PushUrl]
            .into_iter()
            .any(|field| remote_protocol(&self.remote_value(field, cx)) == Some(AuthProtocol::Ssh))
    }

    fn settings(&self, cx: &App) -> Result<GitSettings, String> {
        let inputs = self.inputs.as_ref().ok_or("Git 配置尚未读取完成。")?;
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_owned();
        let mut draft = self.draft.clone();
        draft.user_name = value(&inputs.user_name);
        draft.user_email = value(&inputs.user_email);
        for (remote, inputs) in draft.remotes.iter_mut().zip(&inputs.remotes) {
            remote.url = value(&inputs[RemoteField::FetchUrl as usize]);
            if remote.url.is_empty() {
                return Err(format!("请填写远程 {} 的拉取地址。", remote.name));
            }
            let push_url = value(&inputs[RemoteField::PushUrl as usize]);
            remote.push_url = (!push_url.is_empty()).then_some(push_url);
            remote.https_username = value(&inputs[RemoteField::HttpsUsername as usize]);
            remote.ssh_username = value(&inputs[RemoteField::SshUsername as usize]);
            let private_key = value(&inputs[RemoteField::PrivateKey as usize]);
            remote.ssh_private_key = (!private_key.is_empty()).then(|| PathBuf::from(private_key));
        }
        Ok(draft)
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy() || self.discovering || !self.loaded {
            return;
        }
        match self.settings(cx) {
            Ok(settings) => {
                self.validation = None;
                self.set_saving(true, cx);
                cx.emit(GitSettingsEvent::Apply(settings));
            }
            Err(error) => {
                self.validation = Some(error);
                cx.notify();
            }
        }
    }

    fn input_row(&self, field: RemoteField, label: &'static str) -> Div {
        let row = div().flex().flex_col().gap(px(5.)).child(label);
        let Some(inputs) = self
            .inputs
            .as_ref()
            .and_then(|inputs| inputs.remotes.get(self.selected))
        else {
            return row;
        };
        row.child(
            Input::new(&inputs[field as usize])
                .id(field.id())
                .w_full()
                .h(px(32.))
                .tab_index(20 + field as isize)
                .disabled(self.busy()),
        )
    }
}

impl Render for GitSettingsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        self.ensure_inputs(window, cx);
        let light = cx.theme().mode == ThemeMode::Light;
        let border = rgb(if light { 0xd0d7de } else { 0x30363d });
        let muted: Hsla = rgb(if light { 0x59636e } else { 0x9198a1 }).into();
        let busy = self.busy();
        let uses_ssh = self.uses_ssh(cx);
        let section = |label: &'static str| {
            div()
                .mt(px(8.))
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(label)
        };
        let hint = |text: &'static str| div().text_size(px(11.)).text_color(muted).child(text);
        let mut form = div().flex().flex_col().gap(px(10.));
        if let Some(inputs) = &self.inputs {
            form = form
                .child(section("提交身份"))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(5.))
                        .child("用户名 · user.name")
                        .child(
                            Input::new(&inputs.user_name)
                                .id("git-settings-user-name")
                                .w_full()
                                .h(px(32.))
                                .tab_index(1)
                                .disabled(busy),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(5.))
                        .child("邮箱 · user.email")
                        .child(
                            Input::new(&inputs.user_email)
                                .id("git-settings-user-email")
                                .w_full()
                                .h(px(32.))
                                .tab_index(2)
                                .disabled(busy),
                        ),
                )
                .child(hint(
                    "仅修改当前仓库的 Git 配置；提交身份与远程认证账号分别设置。",
                ))
                .child(section("远程仓库与认证"));
            let mut selectors = div().flex().flex_wrap().gap(px(6.));
            for (index, remote) in self.draft.remotes.iter().enumerate() {
                selectors = selectors.child(
                    Button::new(("git-settings-remote", index))
                        .label(remote.name.clone())
                        .when(self.selected == index, |button| button.primary())
                        .disabled(busy)
                        .tab_index(10 + index as isize)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.busy() {
                                this.selected = index;
                                this.discovery_consent = false;
                                this.validation = None;
                                cx.notify();
                            }
                        })),
                );
            }
            form = form.child(selectors);
            if self.draft.remotes.is_empty() {
                form = form.child(hint(
                    "当前仓库尚未配置远程。添加远程后可在此设置 HTTPS 或 SSH 认证。",
                ));
            } else {
                let protocol = |value: String| match remote_protocol(&value) {
                    Some(AuthProtocol::Https) => "HTTPS · 账号和密码 / PAT",
                    Some(AuthProtocol::Ssh) => "SSH · 私钥或 SSH Agent",
                    None => "本地路径或其他协议",
                };
                let fetch = self.remote_value(RemoteField::FetchUrl, cx);
                let push = self.remote_value(RemoteField::PushUrl, cx);
                let protocols = format!(
                    "拉取：{}　推送：{}",
                    protocol(fetch.clone()),
                    protocol(if push.is_empty() { fetch } else { push }),
                );
                form = form
                    .child(self.input_row(RemoteField::FetchUrl, "拉取地址 · remote.url"))
                    .child(self.input_row(RemoteField::PushUrl, "推送地址 · 留空时使用拉取地址"))
                    .child(
                        div()
                            .id("git-settings-protocol")
                            .test_support()
                            .aria_label(protocols.clone())
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(protocols),
                    )
                    .child(section("HTTPS 账号密码"))
                    .child(self.input_row(RemoteField::HttpsUsername, "HTTPS 认证账号"))
                    .child(hint("密码或个人访问令牌（PAT）在每次操作需要认证时以隐藏字符输入，不写入配置。"))
                    .child(section("SSH 密钥"))
                    .child(self.input_row(RemoteField::SshUsername, "SSH 用户名 · 地址未指定时使用"))
                    .child(hint("地址中已有用户名时以地址为准；填写此项时须保持一致。"))
                    .child(self.input_row(RemoteField::PrivateKey, "SSH 私钥完整路径 · 留空时尝试 SSH Agent"))
                    .child(hint("仅保存所选私钥路径；私钥口令在操作需要时输入，不保存。可手动填写路径或经同意后查找。"))
                    .child(
                        Button::new("git-settings-discover")
                            .label(if self.discovering { "查找中…" } else { "发现 SSH 密钥" })
                            .disabled(busy || self.discovering || !uses_ssh)
                            .tab_index(30)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.busy() && !this.discovering && this.uses_ssh(cx) {
                                    this.discovery_consent = true;
                                    cx.notify();
                                }
                            })),
                    )
                    .when(!uses_ssh, |form| {
                        form.child(hint("将拉取或推送地址配置为 SSH 后，可启用密钥发现。"))
                    });
                if self.discovery_consent && uses_ssh {
                    form = form.child(
                        div()
                            .id("git-settings-discovery-consent")
                            .test_support()
                            .role(Role::Alert)
                            .aria_label("同意后仅在用户 .ssh 目录查找常见私钥及 SSH config 的 IdentityFile 路径，不读取私钥内容，不发送数据")
                            .p(px(12.))
                            .rounded(px(4.))
                            .border_1()
                            .border_color(border)
                            .flex()
                            .flex_col()
                            .gap(px(10.))
                            .child("同意后仅在用户 .ssh 目录查找常见私钥及 SSH config 的 IdentityFile 路径，不读取私钥内容，不发送数据。")
                            .child(
                                div().flex().gap(px(8.)).child(
                                    Button::new("git-settings-discovery-accept")
                                        .label("同意并查找")
                                        .primary()
                                        .disabled(busy || self.discovering)
                                        .tab_index(31)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if this.discovery_consent && !this.busy() && !this.discovering && this.uses_ssh(cx) {
                                                this.set_discovering(true, cx);
                                                cx.emit(GitSettingsEvent::DiscoverSshKeys);
                                            }
                                        })),
                                ).child(
                                    Button::new("git-settings-discovery-decline")
                                        .label("暂不")
                                        .disabled(busy)
                                        .tab_index(32)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.discovery_consent = false;
                                            cx.notify();
                                        })),
                                ),
                            ),
                    );
                }
                if uses_ssh && let Some(keys) = &self.discovered_keys {
                    let mut candidates = div().flex().flex_col().gap(px(5.));
                    if keys.is_empty() {
                        candidates =
                            candidates.child(hint("未发现候选私钥。可手动填写私钥完整路径。"));
                    } else {
                        candidates =
                            candidates.child(hint("选择一个候选路径，保存后才会用于当前远程："));
                        for (index, path) in keys.iter().enumerate() {
                            let value = display_path(path);
                            candidates = candidates.child(
                                Button::new(("git-settings-key", index))
                                    .label(value.clone())
                                    .w_full()
                                    .disabled(busy)
                                    .tab_index(40 + index as isize)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if this.busy() {
                                            return;
                                        }
                                        if let Some(input) = this
                                            .inputs
                                            .as_ref()
                                            .and_then(|inputs| inputs.remotes.get(this.selected))
                                            .map(|inputs| {
                                                inputs[RemoteField::PrivateKey as usize].clone()
                                            })
                                        {
                                            input.update(cx, |state, cx| {
                                                state.set_value(value.clone(), window, cx)
                                            });
                                            this.validation = None;
                                            cx.notify();
                                        }
                                    })),
                            );
                        }
                    }
                    form = form.child(candidates);
                }
            }
        } else if self.loading {
            form = form.child(hint("正在读取当前仓库的 Git 配置…"));
        }
        let error = self.validation.as_ref().or(self.error.as_ref()).cloned();
        let card = div()
            .w(px(760.))
            .h(px(780.))
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
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Git 配置"),
                    )
                    .child(
                        div()
                            .mt(px(6.))
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(display_path(&self.root)),
                    ),
            )
            .child(
                div()
                    .id("git-settings-form-scroll")
                    .test_support()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(22.))
                    .py(px(14.))
                    .child(form),
            )
            .when_some(error, |card, error| {
                card.child(
                    div()
                        .id("git-settings-error")
                        .test_support()
                        .role(Role::Alert)
                        .aria_label(error.clone())
                        .max_h(px(96.))
                        .overflow_y_scroll()
                        .px(px(22.))
                        .pb(px(10.))
                        .text_color(rgb(if light { 0xcf222e } else { 0xf85149 }))
                        .child(error),
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
                    .child(
                        Button::new("git-settings-cancel")
                            .label("取消")
                            .disabled(self.saving)
                            .tab_index(1000)
                            .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
                    )
                    .child(
                        Button::new("git-settings-save")
                            .label(if self.loading {
                                "读取中…"
                            } else if self.saving {
                                "保存中…"
                            } else {
                                "保存配置"
                            })
                            .primary()
                            .disabled(busy || self.discovering || !self.loaded)
                            .tab_index(1001)
                            .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                    ),
            );
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba(0x00000088))
            .key_context("GitViewsGitSettings")
            .track_focus(&self.focus)
            .tab_group()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .font_family("Noto Sans SC")
            .on_action(
                cx.listener(|this, _: &CloseGitSettingsDialog, window, cx| this.close(window, cx)),
            )
            .child(card)
            .focus_trap("gitviews-git-settings-trap", &self.focus)
            .into_any_element()
    }
}
