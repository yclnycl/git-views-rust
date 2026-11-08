//! Drive actual stash controls and verify the requests crossing the UI boundary.
use super::{StashAction, StashDialog, StashDialogEvent, StashEntry, StashRequest};
use gpui_kit::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription,
    TestAppContext, Window, WindowHandle, base::Root, div, px, size, test::TestWindowExt,
};
use std::{cell::RefCell, path::PathBuf, rc::Rc};

struct Harness {
    dialog: Entity<StashDialog>,
    requests: Rc<RefCell<Vec<StashRequest>>>,
    _subscription: Subscription,
}
impl Harness {
    fn new(cx: &mut Context<Self>) -> Self {
        let dialog = cx.new(|cx| {
            let mut dialog = StashDialog::new(cx);
            dialog.show(PathBuf::from("C:/workspace/子模块"), "子模块".into(), cx);
            dialog.set_entries(entries(), Some("c".repeat(40)), cx);
            dialog
        });
        let requests = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&requests);
        let subscription = cx.subscribe(&dialog, move |_, _, event, _| {
            if let StashDialogEvent::Apply(request) = event {
                recorded.borrow_mut().push(request.clone());
            }
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
fn entries() -> Vec<StashEntry> {
    vec![
        StashEntry {
            id: "a".repeat(40),
            index: 0,
            message: "保存的改动".into(),
        },
        StashEntry {
            id: "b".repeat(40),
            index: 1,
            message: "旧改动".into(),
        },
    ]
}
fn harness(cx: &mut TestAppContext) -> (WindowHandle<Root>, Entity<Harness>) {
    let (window, view) =
        crate::ui::headless::open_window(cx, size(px(1150.), px(950.)), |_, cx| {
            cx.new(Harness::new)
        });
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    (window, view)
}
fn requests(cx: &TestAppContext, harness: &Entity<Harness>) -> Vec<StashRequest> {
    cx.read(|cx| harness.read(cx).requests.borrow().clone())
}

#[gpui_kit::test]
fn saving_defaults_to_tracked_files_and_opt_ins_are_explicit(cx: &mut TestAppContext) {
    let (handle, view) = harness(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        assert_eq!(window.find("stash-apply").disabled(), Some(true));
        window.click("stash-message", cx);
        window.input("调试工作", cx);
        window.click("stash-save", cx);
    })
    .unwrap();
    let sent = requests(cx, &view);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].expected_head, Some("c".repeat(40)));
    assert_eq!(
        sent[0].action,
        StashAction::Save {
            message: "调试工作".into(),
            include_untracked: false,
            include_ignored: false
        }
    );
    view.update(cx, |this, cx| {
        this.dialog.update(cx, |dialog, cx| {
            dialog.set_entries(entries(), Some("c".repeat(40)), cx)
        })
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("stash-include-untracked", cx);
        window.click("stash-include-ignored", cx);
        window.click("stash-save", cx);
    })
    .unwrap();
    assert!(matches!(
        &requests(cx, &view)[1].action,
        StashAction::Save {
            include_untracked: true,
            include_ignored: true,
            ..
        }
    ));
}

#[gpui_kit::test]
fn drop_requires_second_confirmation_and_uses_stable_oid(cx: &mut TestAppContext) {
    let (handle, view) = harness(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.click(("stash-entry", 1usize), cx);
        window.click("stash-drop", cx);
        assert_ne!(window.find("stash-drop-confirm").disabled(), Some(true));
        window.click("stash-drop-cancel", cx);
    })
    .unwrap();
    assert!(requests(cx, &view).is_empty());
    // Inserting a newer entry changes the ordinal but selection follows the OID.
    view.update(cx, |this, cx| {
        this.dialog.update(cx, |dialog, cx| {
            let mut shifted = entries();
            for entry in &mut shifted {
                entry.index += 1;
            }
            shifted.insert(
                0,
                StashEntry {
                    id: "d".repeat(40),
                    index: 0,
                    message: "新的记录".into(),
                },
            );
            dialog.set_entries(shifted, Some("c".repeat(40)), cx);
        })
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("stash-drop", cx);
        window.click("stash-drop-confirm", cx);
    })
    .unwrap();
    assert_eq!(
        requests(cx, &view)[0].action,
        StashAction::Drop { id: "b".repeat(40) }
    );
}

#[gpui_kit::test]
fn apply_and_pop_are_distinct_and_busy_blocks_repeated_requests(cx: &mut TestAppContext) {
    let (handle, view) = harness(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.click(("stash-entry", 0usize), cx);
        window.click("stash-apply", cx);
        window.click("stash-apply", cx);
        window.click("stash-pop", cx);
        window.click("stash-save", cx);
        assert_eq!(window.find("stash-close").disabled(), Some(true));
    })
    .unwrap();
    assert_eq!(requests(cx, &view).len(), 1);
    assert_eq!(
        requests(cx, &view)[0].action,
        StashAction::Apply {
            id: "a".repeat(40),
            reinstate_index: true
        }
    );
    view.update(cx, |this, cx| {
        this.dialog.update(cx, |dialog, cx| {
            dialog.set_entries(entries(), Some("c".repeat(40)), cx)
        })
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("stash-reinstate-index", cx);
        window.click("stash-pop", cx);
    })
    .unwrap();
    assert_eq!(
        requests(cx, &view)[1].action,
        StashAction::Pop {
            id: "a".repeat(40),
            reinstate_index: false
        }
    );
}
