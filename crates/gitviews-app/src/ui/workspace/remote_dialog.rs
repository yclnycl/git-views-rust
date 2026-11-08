//! Ephemeral remote authentication UI. Credentials are emitted once and never persisted.

use std::path::PathBuf;

use gitviews_service::display_path;

#[cfg(test)]
mod headless_tests;

use gpui_kit::{
    base::FocusTrapElement as _,
    component::{
        ActiveTheme, Disableable as _, ThemeMode,
        button::{Button, ButtonVariants as _},
        input::{Input, InputContentType, InputEvent, InputState},
    },
    prelude::FluentBuilder as _,
    *,
};

gpui_kit::actions!(gitviews_remote_auth, [CloseRemoteDialog]);

pub(super) enum RemoteDialogPrompt {
    Https {
        remote: String,
        username: Option<String>,
        message: String,
    },
    Ssh {
        remote: String,
        username: Option<String>,
        private_key: Option<PathBuf>,
        message: String,
    },
}

// Intentionally no Debug: event values may contain a password or a private-key passphrase.
pub(super) enum RemoteDialogEvent {
    SubmitHttps {
        username: String,
        password: String,
    },
    SubmitSsh {
        username: String,
        private_key: PathBuf,
        passphrase: String,
    },
    Retry {
        username: String,
    },
    DiscoverSshKeys,
    Cancel,
}

struct FormInputs {
    username: Entity<InputState>,
    private_key: Option<Entity<InputState>>,
    password: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

/// Keep this entity in the workspace and subscribe to its events. `show` and
/// `clear` do not need a Window, so asynchronous service errors can call them.
pub(super) struct RemoteDialog {
    prompt: Option<RemoteDialogPrompt>,
    inputs: Option<FormInputs>,
    validation: Option<String>,
    previous_focus: Option<FocusHandle>,
    focus: FocusHandle,
    discovery_consent: bool,
    discovering: bool,
    discovered_keys: Vec<PathBuf>,
    discovery_message: Option<String>,
}

impl EventEmitter<RemoteDialogEvent> for RemoteDialog {}

impl RemoteDialog {
    pub(super) fn new(cx: &mut Context<Self>) -> Self {
        cx.bind_keys([KeyBinding::new(
            "escape",
            CloseRemoteDialog,
            Some("GitViewsRemoteAuth"),
        )]);
        Self {
            prompt: None,
            inputs: None,
            validation: None,
            previous_focus: None,
            focus: cx.focus_handle(),
            discovery_consent: false,
            discovering: false,
            discovered_keys: Vec::new(),
            discovery_message: None,
        }
    }

    pub(super) fn show(&mut self, prompt: RemoteDialogPrompt, cx: &mut Context<Self>) {
        // A new challenge starts with a fresh masked state, including its undo history.
        self.inputs = None;
        self.validation = None;
        self.discovery_consent = false;
        self.discovering = false;
        self.discovered_keys.clear();
        self.discovery_message = None;
        self.prompt = Some(prompt);
        cx.notify();
    }

    pub(super) fn clear(&mut self, cx: &mut Context<Self>) {
        self.prompt = None;
        self.inputs = None;
        self.validation = None;
        self.previous_focus = None;
        self.discovery_consent = false;
        self.discovering = false;
        self.discovered_keys.clear();
        self.discovery_message = None;
        cx.notify();
    }

    pub(super) fn is_open(&self) -> bool {
        self.prompt.is_some()
    }

    pub(super) fn allows_ssh_discovery(&self) -> bool {
        matches!(self.prompt, Some(RemoteDialogPrompt::Ssh { .. }))
    }

    pub(super) fn set_discovering(&mut self, discovering: bool, cx: &mut Context<Self>) {
        self.discovering = discovering;
        self.discovery_consent = false;
        self.discovery_message = None;
        cx.notify();
    }

    pub(super) fn set_discovered_keys(
        &mut self,
        result: Result<Vec<PathBuf>, String>,
        cx: &mut Context<Self>,
    ) {
        self.discovering = false;
        match result {
            Ok(keys) => {
                self.discovery_message = Some(if keys.is_empty() {
                    "未找到可用的私钥路径，可手动填写或使用 SSH Agent。".into()
                } else {
                    "请选择私钥路径，再输入口令并重试。".into()
                });
                self.discovered_keys = keys;
            }
            Err(error) => self.discovery_message = Some(error),
        }
        cx.notify();
    }

    fn ensure_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.inputs.is_some() {
            return;
        }
        let Some(prompt) = &self.prompt else {
            return;
        };
        let ssh = matches!(prompt, RemoteDialogPrompt::Ssh { .. });
        let username = match prompt {
            RemoteDialogPrompt::Https { username, .. }
            | RemoteDialogPrompt::Ssh { username, .. } => username.clone().unwrap_or_default(),
        };
        let username = if ssh && username.is_empty() {
            "git".to_owned()
        } else {
            username
        };
        let has_username = !username.trim().is_empty();
        let username = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("用户名")
                .default_value(username)
        });
        let key_path = match prompt {
            RemoteDialogPrompt::Ssh { private_key, .. } => private_key
                .as_ref()
                .map(|path| display_path(path))
                .unwrap_or_default(),
            _ => String::new(),
        };
        let private_key = ssh.then(|| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("私钥文件的完整路径")
                    .default_value(key_path)
            })
        });
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(if ssh {
                    "私钥口令（可选）"
                } else {
                    "密码或个人访问令牌"
                })
        });
        let mut subscriptions = vec![
            cx.subscribe_in(
                &username,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => {
                        if let Some(inputs) = &this.inputs {
                            let next = inputs.private_key.as_ref().unwrap_or(&inputs.password);
                            next.read(cx).focus_handle(cx).focus(window, cx);
                        }
                    }
                    InputEvent::Change => {
                        this.validation = None;
                        cx.notify();
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &password,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.submit(window, cx),
                    InputEvent::Change => {
                        this.validation = None;
                        cx.notify();
                    }
                    _ => {}
                },
            ),
        ];
        if let Some(key) = &private_key {
            subscriptions.push(cx.subscribe_in(
                key,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => {
                        if let Some(inputs) = &this.inputs {
                            inputs.password.read(cx).focus_handle(cx).focus(window, cx);
                        }
                    }
                    InputEvent::Change => {
                        this.validation = None;
                        cx.notify();
                    }
                    _ => {}
                },
            ));
        }
        if self.previous_focus.is_none() {
            self.previous_focus = window.focused(cx);
        }
        let first = if has_username {
            private_key.as_ref().unwrap_or(&password)
        } else {
            &username
        };
        first.read(cx).focus_handle(cx).focus(window, cx);
        self.inputs = Some(FormInputs {
            username,
            private_key,
            password,
            _subscriptions: subscriptions,
        });
    }

    fn finish(&mut self, event: RemoteDialogEvent, window: &mut Window, cx: &mut Context<Self>) {
        let previous_focus = self.previous_focus.take();
        self.clear(cx);
        if let Some(focus) = previous_focus {
            focus.focus(window, cx);
        }
        cx.emit(event);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(inputs) = &self.inputs else {
            return;
        };
        let username = inputs.username.read(cx).value().to_string();
        let password = inputs.password.read(cx).value().to_string();
        let private_key = inputs
            .private_key
            .as_ref()
            .map(|state| state.read(cx).value().to_string());
        match response(username, password, private_key) {
            Ok(event) => self.finish(event, window, cx),
            Err(message) => {
                self.validation = Some(message.into());
                cx.notify();
            }
        }
    }
}

impl Render for RemoteDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.is_open() {
            return div().into_any_element();
        }
        self.ensure_inputs(window, cx);
        let Some(prompt) = &self.prompt else {
            return div().into_any_element();
        };
        let (ssh, remote, message) = match prompt {
            RemoteDialogPrompt::Https {
                remote, message, ..
            } => (false, remote, message),
            RemoteDialogPrompt::Ssh {
                remote, message, ..
            } => (true, remote, message),
        };
        let Some(inputs) = &self.inputs else {
            return div().into_any_element();
        };
        let light = cx.theme().mode == ThemeMode::Light;
        let border = rgb(if light { 0xd0d7de } else { 0x3d444d });
        let muted = rgb(if light { 0x59636e } else { 0x9198a1 });
        let mut card = div()
            .id("remote-auth-form")
            .w(px(400.))
            .max_w(relative(0.94))
            .max_h(relative(0.94))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(12.))
            .p(px(18.))
            .rounded(px(8.))
            .border_1()
            .border_color(border)
            .bg(rgb(if light { 0xffffff } else { 0x161b22 }))
            .text_color(rgb(if light { 0x1f2328 } else { 0xf0f6fc }))
            .shadow_lg()
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if ssh { "SSH 认证" } else { "HTTPS 认证" }),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(display_remote(remote)),
            )
            .when(!message.is_empty(), |this| {
                this.child(
                    div()
                        .text_size(px(12.))
                        .text_color(muted)
                        .child(message.clone()),
                )
            })
            .child(
                div().flex().flex_col().gap(px(6.)).child("用户名").child(
                    Input::new(&inputs.username)
                        .id("remote-auth-username")
                        .content_type(InputContentType::Username)
                        .tab_index(1)
                        .h(px(32.)),
                ),
            );
        if let Some(key) = &inputs.private_key {
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child("私钥文件路径")
                    .child(
                        Input::new(key)
                            .id("remote-auth-private-key")
                            .tab_index(2)
                            .h(px(32.)),
                    ),
            );
        }
        card = card
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(if ssh {
                        "私钥口令（可选）"
                    } else {
                        "密码 / PAT"
                    })
                    .child(
                        Input::new(&inputs.password)
                            .id("remote-auth-password")
                            .content_type(InputContentType::Password)
                            .tab_index(3)
                            .h(px(32.)),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child("凭据仅用于本次操作，不保存到磁盘。"),
            )
            .when_some(self.validation.clone(), |this, validation| {
                this.child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(if light { 0xcf222e } else { 0xf85149 }))
                        .child(validation),
                )
            });
        if ssh {
            card =
                card.child(div().text_size(px(11.)).text_color(muted).child(
                    "SSH 地址中已指定的用户名优先；如需更改，请在 Git 设置中修改仓库地址。",
                ));
            card = card.child(
                Button::new("remote-auth-discover")
                    .label(if self.discovering {
                        "正在查找 SSH 密钥…"
                    } else {
                        "自动发现 SSH 密钥路径"
                    })
                    .disabled(self.discovering)
                    .tab_index(4)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.discovery_consent = true;
                        cx.notify();
                    })),
            );
            if self.discovery_consent {
                card = card.child(div().text_size(px(11.)).child(
                    "同意后仅查找用户 .ssh 目录中的常见私钥和 SSH config 的 IdentityFile 路径，不读取私钥内容，不发送数据。"
                )).child(div().flex().gap(px(8.))
                    .child(Button::new("remote-auth-discover-consent").label("同意并查找")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.allows_ssh_discovery() && this.discovery_consent && !this.discovering {
                                this.set_discovering(true, cx);
                                cx.emit(RemoteDialogEvent::DiscoverSshKeys);
                            }
                        })))
                    .child(Button::new("remote-auth-discover-decline").label("暂不")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.discovery_consent = false;
                            cx.notify();
                        }))));
            }
            if let Some(message) = &self.discovery_message {
                card = card.child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(message.clone()),
                );
            }
            for (index, path) in self.discovered_keys.iter().enumerate() {
                let path = display_path(path);
                card = card.child(
                    Button::new(("remote-auth-key", index))
                        .label(path.clone())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(key) = this
                                .inputs
                                .as_ref()
                                .and_then(|inputs| inputs.private_key.as_ref())
                            {
                                key.update(cx, |input, cx| {
                                    input.set_value(path.clone(), window, cx)
                                });
                            }
                            this.validation = None;
                            cx.notify();
                        })),
                );
            }
            card = card.child(
                Button::new("remote-auth-agent")
                    .label("重试 SSH Agent")
                    .tab_index(4)
                    .on_click(cx.listener(|this, _, window, cx| {
                        let username = this
                            .inputs
                            .as_ref()
                            .map(|inputs| inputs.username.read(cx).value().trim().to_owned())
                            .unwrap_or_default();
                        if username.is_empty() {
                            this.validation = Some("请输入 SSH 用户名。".into());
                            cx.notify();
                        } else {
                            this.finish(RemoteDialogEvent::Retry { username }, window, cx);
                        }
                    })),
            );
        }
        card = card.child(
            div()
                .flex()
                .justify_end()
                .gap(px(8.))
                .child(
                    Button::new("remote-auth-cancel")
                        .label("取消")
                        .tab_index(5)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.finish(RemoteDialogEvent::Cancel, window, cx);
                        })),
                )
                .child(
                    Button::new("remote-auth-submit")
                        .label(if ssh {
                            "使用私钥重试"
                        } else {
                            "验证并重试"
                        })
                        .primary()
                        .tab_index(6)
                        .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                ),
        );
        div()
            .absolute()
            .inset_0()
            .occlude()
            .key_context("GitViewsRemoteAuth")
            .track_focus(&self.focus)
            .focus_trap("remote-auth-focus-trap", &self.focus)
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .font_family("Noto Sans SC")
            .on_action(cx.listener(|this, _: &CloseRemoteDialog, window, cx| {
                this.finish(RemoteDialogEvent::Cancel, window, cx);
            }))
            .child(card)
            .into_any_element()
    }
}

fn response(
    username: String,
    password: String,
    private_key: Option<String>,
) -> Result<RemoteDialogEvent, &'static str> {
    let username = username.trim().to_owned();
    if username.is_empty() {
        return Err("请填写用户名。");
    }
    if let Some(private_key) = private_key {
        let private_key = PathBuf::from(private_key.trim().trim_matches('"'));
        if !private_key.is_absolute() {
            return Err("请填写私钥文件的完整路径，或重试 SSH Agent。");
        }
        Ok(RemoteDialogEvent::SubmitSsh {
            username,
            private_key,
            passphrase: password,
        })
    } else if password.is_empty() {
        Err("请填写密码或个人访问令牌（PAT）。")
    } else {
        Ok(RemoteDialogEvent::SubmitHttps { username, password })
    }
}

/// Never echo URL userinfo, query tokens or fragments into the authentication UI.
fn display_remote(remote: &str) -> String {
    let remote = remote.trim().split(['?', '#']).next().unwrap_or_default();
    if let Some((scheme, location)) = remote.split_once("://") {
        let (authority, path) = location.split_once('/').unwrap_or((location, ""));
        let host = authority.rsplit('@').next().unwrap_or_default();
        if path.is_empty() {
            format!("{scheme}://{host}")
        } else {
            format!("{scheme}://{host}/{path}")
        }
    } else {
        remote.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{RemoteDialogEvent, display_remote, response};

    #[test]
    fn remote_label_omits_url_credentials_and_tokens() {
        assert_eq!(
            display_remote(
                "https://user:private-token@example.com/org/repo.git?token=hidden#hidden"
            ),
            "https://example.com/org/repo.git"
        );
        assert_eq!(
            display_remote("git@example.com:org/repo.git"),
            "git@example.com:org/repo.git"
        );
        assert_eq!(
            display_remote("ssh://git@example.com/org/repo.git"),
            "ssh://example.com/org/repo.git"
        );
    }

    #[test]
    fn https_secret_whitespace_is_preserved_and_empty_credentials_do_not_submit() {
        let response = response(" alice ".into(), " token with spaces ".into(), None)
            .unwrap_or_else(|_| panic!("valid credentials"));
        assert!(
            matches!(response, RemoteDialogEvent::SubmitHttps { username, password } if username == "alice" && password == " token with spaces ")
        );
        assert!(super::response("alice".into(), String::new(), None).is_err());
        assert!(super::response(String::new(), "token".into(), None).is_err());
    }

    #[test]
    fn ssh_requires_an_explicit_absolute_key_and_allows_an_empty_passphrase() {
        assert!(response("git".into(), String::new(), Some("relative-key".into())).is_err());
        assert!(response("git".into(), String::new(), Some(String::new())).is_err());
        let key = std::env::temp_dir().join("gitviews-auth-key");
        let response = response(
            "git".into(),
            String::new(),
            Some(format!("\"{}\"", key.display())),
        )
        .unwrap_or_else(|_| panic!("valid key path"));
        assert!(
            matches!(response, RemoteDialogEvent::SubmitSsh { username, private_key, passphrase } if username == "git" && private_key == key && passphrase.is_empty())
        );
    }
}
