//! Exercise the real three-way controls, explicit abort and editor protection.
use super::{
    ConflictAction, ConflictDialog, ConflictDialogEvent, ConflictOperation, ConflictRequest,
    ConflictResolution, ConflictState, ConflictVersions,
};
use gitviews_service::ConflictFile;
use gpui_kit::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription,
    TestAppContext, Window, WindowHandle, base::Root, div, px, size, test::TestWindowExt,
};
use std::{cell::RefCell, path::PathBuf, rc::Rc};
#[derive(Clone)]
enum Request {
    Apply(ConflictRequest),
    Load(String),
}
struct Harness {
    dialog: Entity<ConflictDialog>,
    requests: Rc<RefCell<Vec<Request>>>,
    _subscription: Subscription,
}
impl Harness {
    fn new(cx: &mut Context<Self>) -> Self {
        let dialog = cx.new(|cx| {
            let mut dialog = ConflictDialog::new(cx);
            dialog.show(PathBuf::from("C:/workspace/子模块"), "子模块".into(), cx);
            dialog.set_state(state(), cx);
            dialog
        });
        let requests = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&requests);
        let subscription = cx.subscribe(&dialog, move |_, _, event, _| match event {
            ConflictDialogEvent::Apply(request) => {
                recorded.borrow_mut().push(Request::Apply(request.clone()))
            }
            ConflictDialogEvent::LoadFile(path) => {
                recorded.borrow_mut().push(Request::Load(path.clone()))
            }
            _ => {}
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
fn state() -> ConflictState {
    ConflictState {
        kind: ConflictOperation::Merge,
        label: "正在合并".into(),
        head: Some("c".repeat(40)),
        files: vec![
            ConflictFile {
                path: "note.txt".into(),
            },
            ConflictFile {
                path: "other.txt".into(),
            },
        ],
        can_continue: false,
        can_abort: true,
        backup_ref: Some("refs/gitviews/backups/original".into()),
    }
}
fn versions(path: &str) -> ConflictVersions {
    ConflictVersions {
        path: path.into(),
        base: Some("base\n".into()),
        ours: Some("ours\n".into()),
        theirs: Some("theirs\n".into()),
        result: "conflicted\n".into(),
        binary: false,
        token: "snapshot-token".into(),
    }
}
fn harness(cx: &mut TestAppContext) -> (WindowHandle<Root>, Entity<Harness>) {
    let (window, view) =
        crate::ui::headless::open_window(cx, size(px(1250.), px(1000.)), |_, cx| {
            cx.new(Harness::new)
        });
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    (window, view)
}
fn requests(cx: &TestAppContext, view: &Entity<Harness>) -> Vec<Request> {
    cx.read(|cx| view.read(cx).requests.borrow().clone())
}
fn load(handle: WindowHandle<Root>, view: &Entity<Harness>, cx: &mut TestAppContext) {
    cx.update_window(handle.into(), |_, window, cx| {
        window.click(("conflict-file", 0usize), cx)
    })
    .unwrap();
    view.update(cx, |this, cx| {
        this.dialog.update(cx, |dialog, cx| {
            dialog.set_versions(versions("note.txt"), cx)
        })
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

#[gpui_kit::test]
fn sides_are_read_only_and_editing_result_submits_snapshot_token(cx: &mut TestAppContext) {
    let (handle, view) = harness(cx);
    load(handle, &view, cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("conflict-side-0", cx);
        window.press("ctrl-a", cx);
        window.input("must not edit", cx);
        assert_eq!(
            view.read(cx)
                .dialog
                .read(cx)
                .editors
                .as_ref()
                .unwrap()
                .sides[0]
                .read(cx)
                .value()
                .as_ref(),
            "base\n"
        );
        window.click("conflict-result", cx);
        window.press("ctrl-a", cx);
        window.input("合并结果\n", cx);
        window.click("conflict-save-result", cx);
    })
    .unwrap();
    let sent = requests(cx, &view);
    assert!(matches!(&sent[0],Request::Load(path)if path=="note.txt"));
    let Request::Apply(request) = sent.last().unwrap() else {
        panic!("resolve request")
    };
    assert_eq!(request.expected_head, Some("c".repeat(40)));
    assert_eq!(
        request.action,
        ConflictAction::Resolve {
            path: "note.txt".into(),
            token: "snapshot-token".into(),
            resolution: ConflictResolution::Edited("合并结果\n".into())
        }
    );
}

#[gpui_kit::test]
fn abort_requires_second_confirmation_and_cancel_preserves_operation(cx: &mut TestAppContext) {
    let (handle, view) = harness(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        assert_eq!(window.find("conflict-continue").disabled(), Some(true));
        window.click("conflict-abort", cx);
        assert!(window.find("conflict-abort-confirmation").visible());
        window.click("conflict-abort-cancel", cx);
    })
    .unwrap();
    assert!(requests(cx, &view).is_empty());
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("conflict-abort", cx);
        window.click("conflict-abort-confirm", cx);
        window.click("conflict-abort", cx);
    })
    .unwrap();
    let sent = requests(cx, &view);
    assert_eq!(sent.len(), 1);
    assert!(matches!(&sent[0],Request::Apply(request)if request.action==ConflictAction::Abort));
}

#[gpui_kit::test]
fn unsaved_result_is_protected_when_switching_and_late_reads_are_rejected(cx: &mut TestAppContext) {
    let (handle, view) = harness(cx);
    load(handle, &view, cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("conflict-result", cx);
        window.press("ctrl-a", cx);
        window.input("keep editor work", cx);
        window.click(("conflict-file", 1usize), cx);
        assert!(window.find("conflict-unsaved-confirmation").visible());
        window.click("conflict-unsaved-cancel", cx);
        assert_eq!(
            view.read(cx)
                .dialog
                .read(cx)
                .editors
                .as_ref()
                .unwrap()
                .result
                .read(cx)
                .value()
                .as_ref(),
            "keep editor work"
        );
        window.click(("conflict-file", 1usize), cx);
        window.click("conflict-unsaved-discard", cx);
    })
    .unwrap();
    view.update(cx, |this, cx| {
        this.dialog.update(cx, |dialog, cx| {
            dialog.set_versions(versions("note.txt"), cx);
            assert!(dialog.versions.is_none());
            dialog.set_versions(versions("other.txt"), cx);
        })
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    assert!(matches!(requests(cx,&view).last().unwrap(),Request::Load(path)if path=="other.txt"));
}

#[gpui_kit::test]
fn choosing_deleted_side_emits_side_choice_and_busy_blocks_repeat(cx: &mut TestAppContext) {
    let (handle, view) = harness(cx);
    load(handle, &view, cx);
    view.update(cx, |this, cx| {
        this.dialog.update(cx, |dialog, cx| {
            let mut version = versions("note.txt");
            version.theirs = None;
            dialog.set_versions(version, cx);
        })
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("conflict-use-theirs", cx);
        window.click("conflict-use-ours", cx);
        assert_eq!(window.find("conflict-close").disabled(), Some(true));
    })
    .unwrap();
    let sent = requests(cx, &view);
    assert_eq!(sent.len(), 2);
    assert!(
        matches!(&sent[1],Request::Apply(request)if matches!(request.action,ConflictAction::Resolve{resolution:ConflictResolution::Theirs,..}))
    );
}
