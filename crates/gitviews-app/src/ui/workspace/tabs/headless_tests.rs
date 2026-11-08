//! Real pointer dispatch through production preview tabs and close/activation state.

use super::super::{FileTab, Workspace, WorkspaceOptions};
use gitviews_service::{AppSettings, Head, RepositorySnapshot, SettingsStore};
use gpui_kit::{
    App, AppContext as _, Context, Entity, InputEvent as _, InteractiveElement as _, IntoElement,
    Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, ParentElement as _, Render, Styled as _,
    Subscription, TestAppContext, Window, WindowHandle,
    base::Root,
    div, px, size,
    test::{TestEventExt as _, TestQueryExt as _, TestWindowExt as _},
};
use std::path::PathBuf;

struct Harness {
    workspace: Entity<Workspace>,
    middle_bubbles: usize,
    _subscription: Subscription,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, _, _, _| {
                    this.middle_bubbles += 1;
                }),
            )
            .child(
                self.workspace
                    .update(cx, |workspace, cx| workspace.render_preview_tabs(cx)),
            )
    }
}

fn harness(cx: &mut TestAppContext, active: usize) -> (WindowHandle<Root>, Entity<Harness>) {
    let (window, harness) =
        crate::ui::headless::open_window(cx, size(px(1000.), px(300.)), |window, cx| {
            cx.new(|cx| {
                let workspace = cx.new(|cx| {
                    let mut workspace = Workspace::new(
                        WorkspaceOptions {
                            initial_paths: Vec::new(),
                            state_file: PathBuf::from("unused-tab-test/repositories.json"),
                            preview: true,
                            settings: AppSettings::default(),
                            settings_store: SettingsStore::new("unused-tab-test/config.toml"),
                            settings_error: None,
                        },
                        window,
                        cx,
                    );
                    workspace.opened_files = ["src/甲.rs", "lib/乙.rs", "test/丙.rs"]
                        .into_iter()
                        .map(|path| FileTab {
                            root: PathBuf::from("headless-repository"),
                            path: path.into(),
                            staged: false,
                            revision: None,
                        })
                        .collect();
                    workspace.activate_tab(active, cx);
                    workspace
                });
                let subscription = cx.observe(&workspace, |_, _, cx| cx.notify());
                Harness {
                    workspace,
                    middle_bubbles: 0,
                    _subscription: subscription,
                }
            })
        });
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
    (window, harness)
}

fn click_tab(window: &mut Window, index: usize, button: MouseButton, cx: &mut App) {
    window.render_frame(cx);
    let position = window.find(("preview-tab", index)).bounds().center();
    click_position(window, position, button, cx);
}

fn click_position(
    window: &mut Window,
    position: gpui_kit::Point<gpui_kit::Pixels>,
    button: MouseButton,
    cx: &mut App,
) {
    window.pointer_move(position, None, cx);
    window.dispatch_event(
        MouseDownEvent {
            button,
            position,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }
        .to_platform_input(),
        cx,
    );
    window.render_frame(cx);
    window.dispatch_event(
        MouseUpEvent {
            button,
            position,
            modifiers: Modifiers::default(),
            click_count: 1,
        }
        .to_platform_input(),
        cx,
    );
    window.render_frame(cx);
}

#[gpui_kit::test]
fn middle_click_closes_the_inactive_tab_without_switching_or_canceling_the_active_preview(
    cx: &mut TestAppContext,
) {
    let (window, harness) = harness(cx, 2);
    cx.update_window(window.into(), |_, window, cx| {
        let workspace = harness.read(cx).workspace.clone();
        let (active_pane, request) = workspace.update(cx, |workspace, cx| {
            workspace.diff_task = Some(cx.spawn(async |_, _| std::future::pending::<()>().await));
            (workspace.preview_pane.clone(), workspace.diff_request_id)
        });
        click_tab(window, 0, MouseButton::Middle, cx);
        assert_eq!(harness.read(cx).middle_bubbles, 0);
        let state = workspace.read(cx);
        assert_eq!(
            state
                .opened_files
                .iter()
                .map(|tab| tab.path.as_str())
                .collect::<Vec<_>>(),
            ["lib/乙.rs", "test/丙.rs"]
        );
        assert_eq!(state.active_tab, Some(1));
        assert_eq!(state.preview_pane, active_pane);
        assert_eq!(state.diff_request_id, request);
        assert!(state.diff_task.is_some());

        click_tab(window, 0, MouseButton::Right, cx);
        assert_eq!(workspace.read(cx).opened_files.len(), 2);
        assert_eq!(workspace.read(cx).active_tab, Some(1));
        click_tab(window, 0, MouseButton::Left, cx);
        assert_eq!(workspace.read(cx).opened_files.len(), 2);
        assert_eq!(workspace.read(cx).active_tab, Some(0));
        assert_eq!(
            workspace.read(cx).selected_file.as_deref(),
            Some("lib/乙.rs")
        );
    })
    .unwrap();
}

#[gpui_kit::test]
fn middle_click_active_tab_selects_a_neighbor_and_last_close_cancels_the_pending_preview(
    cx: &mut TestAppContext,
) {
    let (window, harness) = harness(cx, 1);
    cx.update_window(window.into(), |_, window, cx| {
        let workspace = harness.read(cx).workspace.clone();
        for (index, next_path) in [(1, Some("test/丙.rs")), (1, Some("src/甲.rs")), (0, None)] {
            let request = workspace.update(cx, |workspace, cx| {
                workspace.diff_loading = true;
                workspace.diff_task =
                    Some(cx.spawn(async |_, _| std::future::pending::<()>().await));
                workspace.diff_request_id
            });
            click_tab(window, index, MouseButton::Middle, cx);
            let state = workspace.read(cx);
            assert!(state.diff_task.is_none());
            assert!(!state.diff_loading);
            assert!(state.diff_request_id > request);
            assert_eq!(state.selected_file.as_deref(), next_path);
            if next_path.is_none() {
                assert!(state.opened_files.is_empty());
                assert!(state.active_tab.is_none());
                assert!(state.preview_deck.active_key().is_none());
            }
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn ordinary_workspace_tabs_close_on_middle_click_including_the_nested_close_icon(
    cx: &mut TestAppContext,
) {
    // Render the complete Workspace. Ordinary tabs come from populate_tabs and
    // DesignRenderer; the history-only render_preview_tabs harness is bypassed.
    let (window, workspace) =
        crate::ui::headless::open_window(cx, size(px(1680.), px(1000.)), |window, cx| {
            cx.new(|cx| {
                let mut view = Workspace::new(
                    WorkspaceOptions {
                        initial_paths: Vec::new(),
                        state_file: "unused-ordinary-tabs/repositories.json".into(),
                        preview: true,
                        settings: AppSettings::default(),
                        settings_store: SettingsStore::new("unused-ordinary-tabs/config.toml"),
                        settings_error: None,
                    },
                    window,
                    cx,
                );
                let root = PathBuf::from("headless-ordinary-repository");
                view.opened_files = ["src/甲.rs", "lib/乙.rs", "test/丙.rs"]
                    .into_iter()
                    .map(|path| FileTab {
                        root: root.clone(),
                        path: path.into(),
                        staged: false,
                        revision: None,
                    })
                    .collect();
                // Activate before installing the synthetic repository so this
                // fixture does not schedule any backend preview read.
                view.activate_tab(2, cx);
                view.repository = Some(RepositorySnapshot {
                    root,
                    name: "普通标签测试".into(),
                    head: Head::Branch("main".into()),
                    changes: Vec::new(),
                    recent_commits: Vec::new(),
                });
                view.preview = false;
                view
            })
        });
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(!workspace.read(cx).history);
        assert_eq!(window.find_all_by_label("实际文件标签").len(), 3);
        let (pane, request) = workspace.update(cx, |view, cx| {
            view.diff_task = Some(cx.spawn(async |_, _| std::future::pending::<()>().await));
            (view.preview_pane.clone(), view.diff_request_id)
        });

        let first_tab = window.find_all_by_label("实际文件标签")[0].bounds();
        click_position(window, first_tab.center(), MouseButton::Middle, cx);
        let view = workspace.read(cx);
        assert_eq!(
            view.opened_files
                .iter()
                .map(|tab| tab.path.as_str())
                .collect::<Vec<_>>(),
            ["lib/乙.rs", "test/丙.rs"]
        );
        assert_eq!(view.active_tab, Some(1));
        assert_eq!(view.preview_pane, pane);
        assert_eq!(view.diff_request_id, request);
        assert!(view.diff_task.is_some());
        assert_eq!(window.find_all_by_label("实际文件标签").len(), 2);

        // A middle click targeting the close icon must bubble to its tab and
        // close exactly once; primary-click handlers must not activate a sibling.
        let close = window.find_all_by_label("关闭实际文件标签")[1].bounds();
        click_position(window, close.center(), MouseButton::Middle, cx);
        let view = workspace.read(cx);
        assert_eq!(view.opened_files.len(), 1);
        assert_eq!(view.opened_files[0].path, "lib/乙.rs");
        assert_eq!(view.active_tab, Some(0));
        assert_eq!(view.selected_file.as_deref(), Some("lib/乙.rs"));
        assert_eq!(window.find_all_by_label("实际文件标签").len(), 1);
        // Neighbor activation normally queues a background freshness read. The
        // event assertions need no I/O from this synthetic repository.
        workspace.update(cx, |view, _| view.diff_task = None);

        let last_tab = window.find_all_by_label("实际文件标签")[0].bounds();
        click_position(window, last_tab.center(), MouseButton::Middle, cx);
        let view = workspace.read(cx);
        assert!(view.opened_files.is_empty());
        assert!(view.active_tab.is_none());
        assert!(view.selected_file.is_none());
        assert!(view.diff_task.is_none());
        assert!(window.find_all_by_label("实际文件标签").is_empty());
    })
    .unwrap();
}
