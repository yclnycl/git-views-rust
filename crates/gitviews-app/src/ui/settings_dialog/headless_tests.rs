//! Exercise the real settings controls, focus trap and emitted requests without a native window.

use std::{cell::RefCell, path::PathBuf, rc::Rc};

use gitviews_service::{AppSettings, ThemePreference};
use gpui_kit::{
    AppContext, Context, Entity, FocusHandle, IntoElement, ParentElement, Render, Styled,
    Subscription, TestAppContext, Window, WindowHandle, base::Root, component::button::Button, div,
    px, size, test::TestWindowExt,
};

use super::{SettingsDialog, SettingsEvent};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Request {
    Apply(AppSettings),
    GitSettings,
    Reload,
    Close,
}

struct Harness {
    dialog: Entity<SettingsDialog>,
    outside_focus: FocusHandle,
    requests: Rc<RefCell<Vec<Request>>>,
    _subscription: Subscription,
}

impl Harness {
    fn new(settings: AppSettings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let outside_focus = cx.focus_handle();
        outside_focus.focus(window, cx);
        let dialog = cx.new(|cx| {
            let mut dialog = SettingsDialog::new(cx);
            dialog.show(settings, PathBuf::from("headless-config.toml"), cx);
            dialog
        });
        let requests = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&requests);
        let subscription = cx.subscribe(&dialog, move |_, _, event, _| {
            recorded.borrow_mut().push(match event {
                SettingsEvent::Apply(settings) => Request::Apply(*settings),
                SettingsEvent::GitSettings => Request::GitSettings,
                SettingsEvent::Reload => Request::Reload,
                SettingsEvent::Close => Request::Close,
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
                Button::new("outside-settings")
                    .label("Outside the settings dialog")
                    .track_focus(&self.outside_focus),
            )
            .child(self.dialog.clone())
    }
}

fn harness(
    cx: &mut TestAppContext,
    settings: AppSettings,
) -> (WindowHandle<Root>, Entity<Harness>) {
    let (window, view) =
        crate::ui::headless::open_window(cx, size(px(1100.), px(900.)), move |window, cx| {
            cx.new(|cx| Harness::new(settings, window, cx))
        });
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .expect("draw settings dialog");
    (window, view)
}

fn replace_font(window: &mut Window, value: &str, cx: &mut gpui_kit::App) {
    window.click("settings-font-size", cx);
    window.press("ctrl-a", cx);
    window.input(value, cx);
    assert_eq!(window.find("settings-font-size").value(), Some(value));
}

fn requests(cx: &TestAppContext, harness: &Entity<Harness>) -> Vec<Request> {
    cx.read(|cx| harness.read(cx).requests.borrow().clone())
}

#[gpui_kit::test]
fn tab_and_shift_tab_stay_inside_settings_and_escape_restores_focus(cx: &mut TestAppContext) {
    let (handle, harness) = harness(cx, AppSettings::default());
    cx.update_window(handle.into(), |_, window, cx| {
        for key in ["tab", "shift-tab"] {
            // There are 20 controls, so this crosses both ends of the focus order twice.
            for _ in 0..44 {
                window.press(key, cx);
                let dialog = &harness.read(cx).dialog;
                assert!(dialog.read(cx).focus.contains_focused(window, cx));
                assert_eq!(window.find("outside-settings").focused(), Some(false));
            }
        }
        window.press("escape", cx);
        assert!(window.try_find("settings-save").is_none());
        assert!(harness.read(cx).outside_focus.is_focused(window));
    })
    .expect("navigate settings focus");
    assert_eq!(requests(cx, &harness), [Request::Close]);
}

#[gpui_kit::test]
fn invalid_input_cannot_apply_and_enter_submits_the_corrected_draft(cx: &mut TestAppContext) {
    let (handle, harness) = harness(cx, AppSettings::default());
    cx.update_window(handle.into(), |_, window, cx| {
        replace_font(window, "NaN", cx);
    })
    .expect("type invalid settings and flush the input Change event");
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("settings-save", cx);
        assert!(
            window.try_find("settings-error-scroll").is_some(),
            "invalid click: draft={:?}, validation={:?}, saving={}",
            harness.read(cx).dialog.read(cx).draft,
            harness.read(cx).dialog.read(cx).validation,
            harness.read(cx).dialog.read(cx).saving
        );
    })
    .expect("submit invalid settings");
    assert!(requests(cx, &harness).is_empty());
    cx.read(|cx| {
        let dialog = harness.read(cx).dialog.read(cx);
        assert!(dialog.is_open());
        assert!(dialog.validation.is_some());
        assert!(!dialog.saving);
    });

    cx.update_window(handle.into(), |_, window, cx| {
        replace_font(window, "14.5", cx);
    })
    .expect("type corrected settings");
    cx.update_window(handle.into(), |_, window, cx| {
        window.press("enter", cx);
    })
    .expect("submit corrected settings with Enter");
    let mut expected = AppSettings::default();
    expected.appearance.editor_font_size = 14.5;
    assert_eq!(requests(cx, &harness), [Request::Apply(expected)]);
    cx.read(|cx| {
        let dialog = harness.read(cx).dialog.read(cx);
        assert!(dialog.validation.is_none());
        assert!(dialog.saving);
    });
}

#[gpui_kit::test]
fn restore_defaults_only_changes_the_draft_until_save_is_pressed(cx: &mut TestAppContext) {
    let mut initial = AppSettings::default();
    initial.appearance.theme = ThemePreference::Light;
    initial.appearance.editor_font_size = 19.;
    initial.refresh.scan_interval_secs = 60;
    let (handle, harness) = harness(cx, initial);
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("settings-defaults", cx);
        assert_eq!(window.find("settings-font-size").value(), Some("12.5"));
        assert_eq!(window.find("settings-scan-interval").value(), Some("10"));
    })
    .expect("restore draft defaults");
    assert!(requests(cx, &harness).is_empty());
    cx.read(|cx| {
        let dialog = harness.read(cx).dialog.read(cx);
        assert_eq!(dialog.draft, AppSettings::default());
        assert!(!dialog.saving);
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("settings-save", cx);
    })
    .expect("apply the default draft");
    assert_eq!(
        requests(cx, &harness),
        [Request::Apply(AppSettings::default())]
    );
}

#[gpui_kit::test]
fn cancelling_discards_the_edited_draft_when_the_owner_reopens_settings(cx: &mut TestAppContext) {
    let initial = AppSettings::default();
    let (handle, harness) = harness(cx, initial);
    cx.update_window(handle.into(), |_, window, cx| {
        replace_font(window, "18", cx);
        window.click(("settings-theme", 1usize), cx);
        window.click("settings-cancel", cx);
        assert!(window.try_find("settings-save").is_none());
        assert!(harness.read(cx).outside_focus.is_focused(window));
    })
    .expect("cancel the draft");
    assert_eq!(requests(cx, &harness), [Request::Close]);
    cx.update_window(handle.into(), |_, window, cx| {
        harness.read(cx).dialog.clone().update(cx, |dialog, cx| {
            dialog.show(initial, PathBuf::from("headless-config.toml"), cx);
        });
        window.render_frame(cx);
        assert_eq!(window.find("settings-font-size").value(), Some("12.5"));
        assert_eq!(harness.read(cx).dialog.read(cx).draft, initial);
    })
    .expect("reopen with the owner's applied settings");
    assert_eq!(requests(cx, &harness), [Request::Close]);
}

#[gpui_kit::test]
fn reloading_disables_edits_and_duplicate_requests_but_allows_cancel(cx: &mut TestAppContext) {
    let (handle, harness) = harness(cx, AppSettings::default());
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("settings-reload", cx);
        for id in ["settings-save", "settings-reload", "settings-defaults"] {
            assert_eq!(window.find(id).disabled(), Some(true));
            window.click(id, cx);
        }
        assert_eq!(
            window.find(("settings-theme", 1usize)).disabled(),
            Some(true)
        );
        window.click(("settings-theme", 1usize), cx);
        window.click("settings-font-size", cx);
        window.press("ctrl-a", cx);
        window.input("99", cx);
    })
    .expect("try edits during reload");
    // The disabled input rejects typing; owner completion is not required to dismiss a read.
    cx.read(|cx| {
        let dialog = harness.read(cx).dialog.read(cx);
        assert!(dialog.loading);
        assert_eq!(dialog.draft, AppSettings::default());
    });
    assert_eq!(requests(cx, &harness), [Request::Reload]);
    cx.update_window(handle.into(), |_, window, cx| {
        assert_eq!(window.find("settings-font-size").value(), Some("12.5"));
        assert_ne!(window.find("settings-cancel").disabled(), Some(true));
        window.click("settings-cancel", cx);
        assert!(harness.read(cx).outside_focus.is_focused(window));
    })
    .expect("cancel the reload");
    assert_eq!(requests(cx, &harness), [Request::Reload, Request::Close]);
}

#[gpui_kit::test]
fn saving_blocks_cancel_and_escape_until_the_owner_reports_a_result(cx: &mut TestAppContext) {
    let (handle, harness) = harness(cx, AppSettings::default());
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("settings-save", cx);
        for id in ["settings-save", "settings-cancel", "settings-reload"] {
            assert_eq!(window.find(id).disabled(), Some(true));
            window.click(id, cx);
        }
        window.press("escape", cx);
        assert!(window.find("settings-save").visible());
    })
    .expect("try dismissing a save in progress");
    assert_eq!(
        requests(cx, &harness),
        [Request::Apply(AppSettings::default())]
    );

    cx.update_window(handle.into(), |_, window, cx| {
        harness.read(cx).dialog.clone().update(cx, |dialog, cx| {
            dialog.set_error(Some("Cannot write this test file".into()), cx);
        });
        window.render_frame(cx);
        assert_ne!(window.find("settings-save").disabled(), Some(true));
        assert_ne!(window.find("settings-cancel").disabled(), Some(true));
        window.press("escape", cx);
        assert!(window.try_find("settings-save").is_none());
        assert!(harness.read(cx).outside_focus.is_focused(window));
    })
    .expect("owner failure restores editable controls and cancellation");
    assert_eq!(
        requests(cx, &harness),
        [Request::Apply(AppSettings::default()), Request::Close]
    );
}
