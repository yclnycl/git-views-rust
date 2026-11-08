//! Exercise consent and key selection without reading user files or using a network.

use std::{cell::RefCell, rc::Rc};

use gpui_kit::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription,
    TestAppContext, Window, WindowHandle, base::Root, div, px, size, test::TestWindowExt,
};

use super::{RemoteDialog, RemoteDialogEvent, RemoteDialogPrompt};

// Record only event kinds; credentials do not enter test diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Request {
    Https,
    Ssh,
    Retry,
    Discover,
    Cancel,
}

struct Harness {
    dialog: Entity<RemoteDialog>,
    requests: Rc<RefCell<Vec<Request>>>,
    _subscription: Subscription,
}

impl Harness {
    fn new(prompt: RemoteDialogPrompt, cx: &mut Context<Self>) -> Self {
        let dialog = cx.new(|cx| {
            let mut dialog = RemoteDialog::new(cx);
            dialog.show(prompt, cx);
            dialog
        });
        let requests = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&requests);
        let subscription = cx.subscribe(&dialog, move |_, _, event, _| {
            recorded.borrow_mut().push(match event {
                RemoteDialogEvent::SubmitHttps { .. } => Request::Https,
                RemoteDialogEvent::SubmitSsh { .. } => Request::Ssh,
                RemoteDialogEvent::Retry { .. } => Request::Retry,
                RemoteDialogEvent::DiscoverSshKeys => Request::Discover,
                RemoteDialogEvent::Cancel => Request::Cancel,
            });
        });
        Self {
            dialog,
            requests,
            _subscription: subscription,
        }
    }
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().relative().size_full().child(self.dialog.clone())
    }
}

fn harness(
    cx: &mut TestAppContext,
    prompt: RemoteDialogPrompt,
) -> (WindowHandle<Root>, Entity<Harness>) {
    let (window, view) =
        crate::ui::headless::open_window(cx, size(px(1100.), px(1100.)), move |_, cx| {
            cx.new(|cx| Harness::new(prompt, cx))
        });
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .expect("draw authentication dialog");
    (window, view)
}

fn ssh_prompt() -> RemoteDialogPrompt {
    RemoteDialogPrompt::Ssh {
        remote: "git@example.test:team/repository.git".into(),
        username: Some("git".into()),
        private_key: None,
        message: "SSH 认证需要凭据。".into(),
    }
}

fn replace_input(window: &mut Window, id: &'static str, value: &str, cx: &mut App) {
    window.click(id, cx);
    window.press(crate::ui::headless::SELECT_ALL_KEY, cx);
    window.input(value, cx);
}

fn requests(cx: &TestAppContext, harness: &Entity<Harness>) -> Vec<Request> {
    cx.read(|cx| harness.read(cx).requests.borrow().clone())
}

#[gpui_kit::test]
fn https_form_has_no_ssh_discovery_controls(cx: &mut TestAppContext) {
    let (handle, harness) = harness(
        cx,
        RemoteDialogPrompt::Https {
            remote: "https://example.test/team/repository.git".into(),
            username: None,
            message: "请输入 HTTPS 账号和密码。".into(),
        },
    );
    cx.update_window(handle.into(), |_, window, _| {
        assert!(window.try_find("remote-auth-username").is_some());
        assert!(window.try_find("remote-auth-password").is_some());
        assert!(window.try_find("remote-auth-private-key").is_none());
        assert!(window.try_find("remote-auth-discover").is_none());
        assert!(window.try_find("remote-auth-discover-consent").is_none());
    })
    .expect("inspect HTTPS authentication controls");
    assert!(requests(cx, &harness).is_empty());
}

#[gpui_kit::test]
fn discovery_requires_consent_and_preserves_typed_credentials(cx: &mut TestAppContext) {
    let (handle, harness) = harness(cx, ssh_prompt());
    assert!(requests(cx, &harness).is_empty());
    cx.update_window(handle.into(), |_, window, cx| {
        replace_input(window, "remote-auth-private-key", "typed-key-path", cx);
        replace_input(window, "remote-auth-password", "fixture passphrase", cx);
        window.click("remote-auth-discover", cx);
        assert!(window.try_find("remote-auth-discover-consent").is_some());
        assert!(window.try_find("remote-auth-discover-decline").is_some());
    })
    .expect("request discovery and show consent");
    assert!(requests(cx, &harness).is_empty());

    cx.update_window(handle.into(), |_, window, cx| {
        window.click("remote-auth-discover-decline", cx);
        assert!(window.try_find("remote-auth-discover-consent").is_none());
        assert!(window.try_find("remote-auth-private-key").is_some());
    })
    .expect("decline without starting discovery");
    assert!(requests(cx, &harness).is_empty());

    cx.update_window(handle.into(), |_, window, cx| {
        window.click("remote-auth-discover", cx);
        window.click("remote-auth-discover-consent", cx);
        assert!(window.try_find("remote-auth-discover-consent").is_none());
        assert_eq!(window.find("remote-auth-discover").disabled(), Some(true));
        window.click("remote-auth-discover", cx);
        assert_eq!(
            window.find("remote-auth-private-key").value(),
            Some("typed-key-path")
        );
        assert!(window.try_find("remote-auth-submit").is_some());
    })
    .expect("consent starts exactly one discovery and retains the form");
    assert_eq!(requests(cx, &harness), [Request::Discover]);
    cx.read(|cx| {
        let dialog = harness.read(cx).dialog.read(cx);
        let inputs = dialog
            .inputs
            .as_ref()
            .expect("authentication inputs retained");
        assert_eq!(
            inputs.password.read(cx).value().to_string(),
            "fixture passphrase"
        );
        assert!(dialog.is_open());
        assert!(dialog.discovering);
    });
}

#[gpui_kit::test]
fn choosing_discovered_path_populates_key_input_without_resetting_passphrase(
    cx: &mut TestAppContext,
) {
    let (handle, harness) = harness(cx, ssh_prompt());
    let candidate = std::env::temp_dir().join("gitviews-headless-key with spaces");
    cx.update_window(handle.into(), |_, window, cx| {
        replace_input(window, "remote-auth-password", "fixture passphrase", cx);
        let dialog = harness.read(cx).dialog.clone();
        dialog.update(cx, |dialog, cx| {
            dialog.set_discovered_keys(Ok(vec![candidate.clone()]), cx);
        });
        window.render_frame(cx);
        window.click(("remote-auth-key", 0usize), cx);
        assert_eq!(
            window.find("remote-auth-private-key").value(),
            candidate.to_str()
        );
        assert!(window.try_find("remote-auth-submit").is_some());
    })
    .expect("select a supplied discovery result");
    assert!(requests(cx, &harness).is_empty());
    cx.read(|cx| {
        let dialog = harness.read(cx).dialog.read(cx);
        let inputs = dialog
            .inputs
            .as_ref()
            .expect("authentication inputs retained");
        assert_eq!(
            inputs.password.read(cx).value().to_string(),
            "fixture passphrase"
        );
        assert!(!dialog.discovering);
    });
}
