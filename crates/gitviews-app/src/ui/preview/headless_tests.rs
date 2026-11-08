//! Exercise the menus over the real readonly Editor and both Diff layouts.
use super::{FileHistoryInPreview, PreviewMode, PreviewPane, PreviewPaneEvent, Status};
use gitviews_service::{FileDiff, SourceFile, TextEncoding};
use gpui_kit::{
    AppContext as _, Context, Entity, IntoElement, MouseButton, ParentElement as _, Render,
    Styled as _, Subscription, TestAppContext, Window, WindowHandle,
    base::Root,
    component::{
        diff::{DiffFile, DiffLineRange, DiffSide},
        input::{Copy, Paste},
    },
    div, point, px, size,
    test::{ClickOptions, TestWindowExt as _},
};
use std::{cell::Cell, rc::Rc, sync::Arc};

const SOURCE: &str = "第一行中文\n第二行目标\n第三行\n";
const PATCH: &str = "diff --git a/src/中文.rs b/src/中文.rs\n--- a/src/中文.rs\n+++ b/src/中文.rs\n@@ -1,3 +1,3 @@\n 第一行中文\n-旧内容\n+第二行目标\n 第三行\n";

struct Harness {
    pane: Entity<PreviewPane>,
    history_requests: Rc<Cell<usize>>,
    _subscription: Subscription,
}

impl Harness {
    fn new(mode: PreviewMode, cx: &mut Context<Self>) -> Self {
        let pane = cx.new(|cx| {
            let mut pane = PreviewPane::new_deferred(cx);
            pane.set_mode(mode, cx);
            pane.set_history_available(true, cx);
            if mode == PreviewMode::Source {
                pane.set_source(
                    Arc::new(SourceFile {
                        path: "src/中文.rs".into(),
                        text: SOURCE.into(),
                        encoding: TextEncoding::Utf8,
                        line_count: 4,
                    }),
                    cx,
                );
            } else {
                pane.diff_content = Some(Arc::new(FileDiff {
                    path: "src/中文.rs".into(),
                    old_text: "第一行中文\n旧内容\n第三行\n".into(),
                    new_text: SOURCE.into(),
                    patch: PATCH.into(),
                    lines: Vec::new(),
                }));
                // Test the actual control's menu/selection, independently of
                // background parsing timing (covered by the parser tests).
                pane.diff.update(cx, |state, cx| {
                    state.set_files(DiffFile::parse(PATCH).unwrap(), cx);
                    state.set_selected_lines(
                        Some(DiffLineRange::new("src/中文.rs", DiffSide::Modified, 2, 2)),
                        cx,
                    );
                });
                pane.diff_ready = true;
                pane.status = Status::Ready;
            }
            pane
        });
        let history_requests = Rc::new(Cell::new(0));
        let requests = history_requests.clone();
        let subscription = cx.subscribe(&pane, move |_, _, event, _| {
            if matches!(event, PreviewPaneEvent::FileHistory) {
                requests.set(requests.get() + 1);
            }
        });
        Self {
            pane,
            history_requests,
            _subscription: subscription,
        }
    }
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.pane.clone())
    }
}

fn harness(
    mode: PreviewMode,
    cx: &mut TestAppContext,
) -> (WindowHandle<Root>, Entity<Harness>, Entity<PreviewPane>) {
    let (handle, view) = crate::ui::headless::open_window(cx, size(px(960.), px(600.)), |_, cx| {
        cx.new(|cx| Harness::new(mode, cx))
    });
    cx.update_window(handle.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    let pane = view.read_with(cx, |view, _| view.pane.clone());
    (handle, view, pane)
}

fn open_menu(handle: WindowHandle<Root>, mode: PreviewMode, cx: &mut TestAppContext) {
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // Hit the first source line, or an actual diff code row below its
        // hunk heading. Do not rely on an empty part of the preview surface.
        window.click_with_options(
            if mode == PreviewMode::Source {
                "preview-source-context"
            } else {
                "preview-diff-context"
            },
            ClickOptions::new()
                .with_button(MouseButton::Right)
                .with_offset(point(
                    px(115.),
                    if mode == PreviewMode::Source {
                        px(12.)
                    } else {
                        px(52.)
                    },
                )),
            cx,
        );
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("popup-menu").visible());
        for label in [
            "复制",
            "全选",
            "查找",
            "查找下一个",
            "查找上一个",
            "查看文件历史",
        ] {
            assert!(window.within("popup-menu").find_by_label(label).visible());
        }
        for label in ["Copy", "Select All", "Cut", "Paste", "剪切", "粘贴"] {
            assert!(
                window
                    .within("popup-menu")
                    .try_find_by_label(label)
                    .is_none()
            );
        }
    })
    .unwrap();
}

fn click_menu(handle: WindowHandle<Root>, item: usize, cx: &mut TestAppContext) {
    cx.update_window(handle.into(), |_, window, cx| {
        window.within("popup-menu").click(item, cx);
    })
    .unwrap();
    // PopupMenu dispatches through Window::dispatch_action, which queues the
    // action with cx.defer. Let that run before checking clipboard/view state.
    cx.run_until_parked();
}

#[gpui_kit::test]
fn source_menu_is_chinese_and_keeps_readonly_selection_commands(cx: &mut TestAppContext) {
    let (handle, view, pane) = harness(PreviewMode::Source, cx);
    let editor = pane.read_with(cx, |pane, _| pane.editor.clone().unwrap());
    editor.update(cx, |editor, cx| {
        editor.set_selected_range(0.."第一行中文".len(), cx);
    });
    open_menu(handle, PreviewMode::Source, cx);
    cx.update_window(handle.into(), |_, window, cx| {
        assert_eq!(editor.read(cx).selected_range(), 0.."第一行中文".len());
        window.render_frame(cx);
    })
    .unwrap();
    click_menu(handle, 0, cx);
    cx.update_window(handle.into(), |_, _, cx| {
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("第一行中文".into())
        );
    })
    .unwrap();
    open_menu(handle, PreviewMode::Source, cx);
    click_menu(handle, 1, cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.dispatch_action(Box::new(Copy), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle.into(), |_, window, cx| {
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(SOURCE.into())
        );
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("禁止写入".into()));
        window.dispatch_action(Box::new(Paste), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        editor.read_with(cx, |editor, _| editor.value()).as_ref(),
        SOURCE
    );
    assert_eq!(view.read_with(cx, |view, _| view.history_requests.get()), 0);
    open_menu(handle, PreviewMode::Source, cx);
    click_menu(handle, 3, cx);
    assert!(editor.read_with(cx, |editor, _| editor.search_activation_revision()) > 0);
}

#[gpui_kit::test]
fn split_and_unified_menus_copy_real_side_and_emit_history(cx: &mut TestAppContext) {
    for mode in [PreviewMode::Split, PreviewMode::Unified] {
        let (handle, view, pane) = harness(mode, cx);
        open_menu(handle, mode, cx);
        if mode == PreviewMode::Split {
            click_menu(handle, 0, cx);
        } else {
            cx.update_window(handle.into(), |_, window, cx| {
                window.press("up", cx);
                window.press("down", cx);
                window.press("enter", cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
        cx.update_window(handle.into(), |_, _, cx| {
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some("第二行目标\n".into())
            );
            assert_eq!(
                pane.read(cx).diff.read(cx).selected_lines(),
                Some(DiffLineRange::new("src/中文.rs", DiffSide::Modified, 2, 2))
            );
        })
        .unwrap();
        open_menu(handle, mode, cx);
        click_menu(handle, 1, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.dispatch_action(Box::new(Copy), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle.into(), |_, _, cx| {
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some(SOURCE.into())
            );
        })
        .unwrap();
        open_menu(handle, mode, cx);
        click_menu(handle, 7, cx);
        assert_eq!(view.read_with(cx, |view, _| view.history_requests.get()), 1);

        pane.update(cx, |pane, cx| {
            pane.diff
                .update(cx, |state, cx| state.set_selected_lines(None, cx));
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string("未复制".into()));
        });
        open_menu(handle, mode, cx);
        click_menu(handle, 0, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("popup-menu").visible());
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some("未复制".into())
            );
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
    }
}

#[gpui_kit::test]
fn history_requires_explicit_target_and_fresh_payload(cx: &mut TestAppContext) {
    let (handle, view, pane) = harness(PreviewMode::Source, cx);
    for state in [0, 1, 2] {
        pane.update(cx, |pane, cx| match state {
            0 => pane.set_history_available(false, cx),
            1 => {
                pane.set_history_available(true, cx);
                pane.set_loading(cx);
            }
            _ => pane.set_error("无法重新读取文件".into(), cx),
        });
        open_menu(handle, PreviewMode::Source, cx);
        click_menu(handle, 7, cx);
        cx.update_window(handle.into(), |_, window, cx| {
            // This component does not advertise aria-disabled. Verify the
            // actual disabled gesture: no action and no menu dismissal.
            window.render_frame(cx);
            assert!(window.find("popup-menu").visible());
            assert_eq!(view.read(cx).history_requests.get(), 0);
            window.press("escape", cx);
            let editor = pane.read(cx).editor.clone().unwrap();
            editor.update(cx, |editor, cx| editor.focus(window, cx));
            window.dispatch_action(Box::new(FileHistoryInPreview), cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(view.read_with(cx, |view, _| view.history_requests.get()), 0);
    }
    pane.update(cx, |pane, cx| {
        pane.clear(cx);
        pane.show_file_history(cx);
        assert!(!pane.can_show_file_history());
    });
    assert_eq!(view.read_with(cx, |view, _| view.history_requests.get()), 0);
}
