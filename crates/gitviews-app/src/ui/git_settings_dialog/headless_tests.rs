//! Exercise real controls, including the consent boundary and separate remote drafts.

use std::{cell::RefCell, path::PathBuf, rc::Rc};

use gitviews_service::GitRemoteSettings;
use gpui_kit::{
    App, AppContext, Context, Entity, FocusHandle, IntoElement, ParentElement, Render, ScrollDelta,
    Styled, Subscription, TestAppContext, Window, WindowHandle, base::Root,
    component::button::Button, div, point, px, size, test::TestWindowExt,
};

use super::{GitSettings, GitSettingsDialog, GitSettingsEvent};

#[derive(Clone)]
enum Request {
    Apply(GitSettings),
    Discover,
    Close,
}

struct Harness {
    dialog: Entity<GitSettingsDialog>,
    outside_focus: FocusHandle,
    requests: Rc<RefCell<Vec<Request>>>,
    _subscription: Subscription,
}

impl Harness {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let outside_focus = cx.focus_handle();
        outside_focus.focus(window, cx);
        let dialog = cx.new(|cx| {
            let mut dialog = GitSettingsDialog::new(cx);
            dialog.show(PathBuf::from("headless-repository"), cx);
            dialog.set_settings(settings(), cx);
            dialog
        });
        let requests = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&requests);
        let subscription = cx.subscribe(&dialog, move |_, _, event, _| {
            recorded.borrow_mut().push(match event {
                GitSettingsEvent::Apply(settings) => Request::Apply(settings.clone()),
                GitSettingsEvent::DiscoverSshKeys => Request::Discover,
                GitSettingsEvent::Close => Request::Close,
            });
        });
        Self {
            dialog,
            outside_focus,
            requests,
            _subscription: subscription,
        }
    }
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .relative()
            .size_full()
            .child(
                Button::new("outside-git-settings")
                    .label("Outside")
                    .track_focus(&self.outside_focus),
            )
            .child(self.dialog.clone())
    }
}

fn settings() -> GitSettings {
    GitSettings {
        user_name: "开发者".into(),
        user_email: "developer@example.com".into(),
        remotes: vec![
            GitRemoteSettings {
                name: "origin".into(),
                url: "https://example.com/team/repository.git".into(),
                push_url: None,
                https_username: "https-user".into(),
                ssh_username: String::new(),
                ssh_private_key: None,
            },
            GitRemoteSettings {
                name: "backup".into(),
                url: "git@example.com:team/backup.git".into(),
                push_url: None,
                https_username: String::new(),
                ssh_username: "git".into(),
                ssh_private_key: None,
            },
        ],
    }
}

fn harness(cx: &mut TestAppContext) -> (WindowHandle<Root>, Entity<Harness>) {
    let (window, view) =
        crate::ui::headless::open_window(cx, size(px(1150.), px(950.)), |window, cx| {
            cx.new(|cx| Harness::new(window, cx))
        });
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .expect("draw Git settings");
    (window, view)
}

fn requests(cx: &TestAppContext, harness: &Entity<Harness>) -> Vec<Request> {
    cx.read(|cx| harness.read(cx).requests.borrow().clone())
}

fn scroll(window: &mut Window, bottom: bool, cx: &mut App) {
    window.scroll(
        "git-settings-form-scroll",
        ScrollDelta::Pixels(point(px(0.), px(if bottom { -10000. } else { 10000. }))),
        cx,
    );
}

fn input(window: &mut Window, id: &'static str, value: &str, cx: &mut App) {
    window.click(id, cx);
    window.press(crate::ui::headless::SELECT_ALL_KEY, cx);
    window.input(value, cx);
    assert_eq!(window.find(id).value(), Some(value));
}

#[gpui_kit::test]
fn ssh_discovery_requires_ssh_url_and_explicit_consent(cx: &mut TestAppContext) {
    let (handle, harness) = harness(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        scroll(window, true, cx);
        assert_eq!(window.find("git-settings-discover").disabled(), Some(true));
        window.click("git-settings-discover", cx);
        assert!(window.try_find("git-settings-discovery-consent").is_none());
        scroll(window, false, cx);
        // A distinct SSH push URL also enables discovery for an HTTPS fetch remote.
        input(
            window,
            "git-settings-push-url",
            "git@example.com:team/repository.git",
            cx,
        );
    })
    .expect("enter SSH push URL");
    assert!(requests(cx, &harness).is_empty());
    cx.update_window(handle.into(), |_, window, cx| {
        scroll(window, true, cx);
        assert_ne!(window.find("git-settings-discover").disabled(), Some(true));
        window.click("git-settings-discover", cx);
        scroll(window, true, cx);
        assert!(window.find("git-settings-discovery-consent").visible());
        window.click("git-settings-discovery-decline", cx);
        assert!(window.try_find("git-settings-discovery-consent").is_none());
    })
    .expect("decline discovery");
    assert!(requests(cx, &harness).is_empty());
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("git-settings-discover", cx);
        scroll(window, true, cx);
        window.click("git-settings-discovery-accept", cx);
        assert_eq!(window.find("git-settings-discover").disabled(), Some(true));
    })
    .expect("accept discovery");
    assert!(matches!(
        requests(cx, &harness).as_slice(),
        [Request::Discover]
    ));
}

#[gpui_kit::test]
fn all_remote_drafts_survive_selection_discovery_and_errors_until_save(cx: &mut TestAppContext) {
    let (handle, harness) = harness(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        input(window, "git-settings-user-name", "新的提交作者", cx);
        scroll(window, true, cx);
        input(window, "git-settings-https-username", "new-https-user", cx);
        scroll(window, false, cx);
        window.click(("git-settings-remote", 1usize), cx);
        scroll(window, true, cx);
        input(window, "git-settings-ssh-username", "ssh-user", cx);
        window.click("git-settings-discover", cx);
        scroll(window, true, cx);
        window.click("git-settings-discovery-accept", cx);
    })
    .expect("edit both remote drafts and accept discovery");
    let selected_path = PathBuf::from("C:/headless-ssh/id_ed25519");
    cx.update_window(handle.into(), |_, window, cx| {
        harness.read(cx).dialog.clone().update(cx, |dialog, cx| {
            dialog.set_discovered_keys(vec![selected_path.clone()], cx);
        });
        scroll(window, true, cx);
        // Discovery supplies candidates without silently selecting a private key.
        assert_eq!(window.find("git-settings-private-key").value(), Some(""));
        window.click(("git-settings-key", 0usize), cx);
        assert_eq!(
            window.find("git-settings-private-key").value(),
            Some(gitviews_service::display_path(&selected_path).as_str())
        );
        harness.read(cx).dialog.clone().update(cx, |dialog, cx| {
            dialog.set_error(Some("保存失败，可重试".into()), cx);
        });
        window.render_frame(cx);
        assert_eq!(
            window.find("git-settings-ssh-username").value(),
            Some("ssh-user")
        );
        scroll(window, false, cx);
        window.click(("git-settings-remote", 0usize), cx);
        assert_eq!(
            window.find("git-settings-user-name").value(),
            Some("新的提交作者")
        );
        scroll(window, true, cx);
        assert_eq!(
            window.find("git-settings-https-username").value(),
            Some("new-https-user")
        );
        window.click("git-settings-save", cx);
    })
    .expect("select key, preserve draft after error, and save");
    let requests = requests(cx, &harness);
    let [Request::Discover, Request::Apply(saved)] = requests.as_slice() else {
        panic!("expected discovery and one save request");
    };
    assert_eq!(saved.user_name, "新的提交作者");
    assert_eq!(saved.remotes[0].https_username, "new-https-user");
    assert_eq!(saved.remotes[1].ssh_username, "ssh-user");
    assert_eq!(
        saved.remotes[1].ssh_private_key.as_ref(),
        Some(&selected_path)
    );
}

#[gpui_kit::test]
fn failed_initial_load_does_not_enable_save_and_escape_restores_focus(cx: &mut TestAppContext) {
    let (handle, harness) = harness(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        for key in ["tab", "shift-tab"] {
            for _ in 0..40 {
                window.press(key, cx);
                assert!(
                    harness
                        .read(cx)
                        .dialog
                        .read(cx)
                        .focus
                        .contains_focused(window, cx)
                );
                assert_eq!(window.find("outside-git-settings").focused(), Some(false));
            }
        }
        harness.read(cx).dialog.clone().update(cx, |dialog, cx| {
            dialog.show(PathBuf::from("failed-repository"), cx);
            dialog.set_error(Some("无法读取 Git 配置".into()), cx);
        });
        window.render_frame(cx);
        assert_eq!(window.find("git-settings-save").disabled(), Some(true));
        window.click("git-settings-save", cx);
        window.press("escape", cx);
        assert!(window.try_find("git-settings-save").is_none());
        assert!(harness.read(cx).outside_focus.is_focused(window));
    })
    .expect("trap focus, fail initial load and close");
    assert!(matches!(
        requests(cx, &harness).as_slice(),
        [Request::Close]
    ));
}
