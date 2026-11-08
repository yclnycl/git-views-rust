//! Pointer and input dispatch through the virtualized file list with synthetic repository data.

use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc};

use gitviews_service::{FileChange, FileTreeEntry, FileTreeKind, RepositoryTree, SubmoduleInfo};
use gpui_kit::{
    App, AppContext, Context, Entity, InputEvent as _, IntoElement, Modifiers, MouseButton,
    MouseDownEvent, MouseUpEvent, ParentElement, Render, Styled, Subscription, TestAppContext,
    Window, WindowHandle,
    base::Root,
    component::input::{Input, InputEvent, InputState},
    div, point, px, size,
    test::{TestEventExt, TestSupportExt, TestWindowExt},
};

use super::{FileList, FileListEvent, Row};
use gpui_kit::InteractiveElement as _;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Request {
    File { path: String, staged: bool },
    Submodule(PathBuf),
    Branches(PathBuf),
}

struct Harness {
    list: Entity<FileList>,
    filter: Entity<InputState>,
    requests: Rc<RefCell<Vec<Request>>>,
    _subscriptions: Vec<Subscription>,
}

impl Harness {
    fn new(
        tree: RepositoryTree,
        changes: Vec<FileChange>,
        tree_mode: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let list = cx.new(|cx| {
            let mut list = FileList::new();
            list.set_read_only(false, cx);
            list.set_data(repository_root(), Arc::new(tree), changes, cx);
            list.set_tree_mode(tree_mode, cx);
            list
        });
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("筛选文件"));
        let requests = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&requests);
        let subscriptions = vec![
            cx.subscribe(&list, move |_, _, event, _| {
                recorded.borrow_mut().push(match event {
                    FileListEvent::OpenFile { path, staged } => Request::File {
                        path: path.clone(),
                        staged: *staged,
                    },
                    FileListEvent::OpenSubmodule(path) => Request::Submodule(path.clone()),
                    FileListEvent::SubmoduleBranches(path) => Request::Branches(path.clone()),
                    FileListEvent::UpdateIndex { .. } | FileListEvent::FileHistory { .. } => return,
                });
            }),
            cx.subscribe(&filter, |this, input, event, cx| {
                if matches!(event, InputEvent::Change) {
                    let query = input.read(cx).value().to_string();
                    this.list.update(cx, |list, cx| list.set_query(query, cx));
                }
            }),
        ];
        Self {
            list,
            filter,
            requests,
            _subscriptions: subscriptions,
        }
    }
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                Input::new(&self.filter)
                    .id("file-filter")
                    .h(px(32.))
                    .flex_shrink_0(),
            )
            .child(
                div()
                    .id("file-list-surface")
                    .test_support()
                    .flex_1()
                    .min_h_0()
                    .child(self.list.clone()),
            )
    }
}

fn repository_root() -> PathBuf {
    PathBuf::from("headless-parent/父 仓库")
}

fn harness(
    cx: &mut TestAppContext,
    tree: RepositoryTree,
    changes: Vec<FileChange>,
    tree_mode: bool,
) -> (WindowHandle<Root>, Entity<Harness>) {
    let (window, harness) =
        crate::ui::headless::open_window(cx, size(px(520.), px(460.)), move |window, cx| {
            cx.new(|cx| Harness::new(tree, changes, tree_mode, window, cx))
        });
    settle(cx, window, &harness);
    (window, harness)
}

fn settle(cx: &mut TestAppContext, window: WindowHandle<Root>, harness: &Entity<Harness>) {
    // This executes and awaits production row builds, including a coalesced second generation.
    cx.run_until_parked();
    cx.read(|cx| {
        let list = harness.read(cx).list.read(cx);
        assert!(!list.preparing);
        assert!(!list.rows_pending);
        assert!(list.rows_task.is_none());
    });
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .expect("paint completed file rows");
}

fn click_row(window: &mut Window, index: usize, count: usize, cx: &mut App) {
    window.render_frame(cx);
    let bounds = window.find("file-list-surface").bounds();
    // The production UniformList row height is 32px. Use the observed surface's real
    // window position instead of hard-coding titlebar, input or root offsets.
    let position = bounds.origin + point(px(28.), px(16. + index as f32 * 32.));
    assert!(position.y < bounds.bottom(), "row must be visible to click");
    window.pointer_move(position, None, cx);
    for click_count in 1..=count {
        window.dispatch_event(
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count,
                first_mouse: false,
            }
            .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
        window.dispatch_event(
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count,
            }
            .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
    }
}

fn requests(cx: &TestAppContext, harness: &Entity<Harness>) -> Vec<Request> {
    cx.read(|cx| harness.read(cx).requests.borrow().clone())
}

fn change(path: &str, index_status: char, worktree_status: char) -> FileChange {
    FileChange {
        path: path.into(),
        index_status,
        worktree_status,
    }
}

fn right_click_row(window: &mut Window, index: usize, cx: &mut App) {
    let bounds = window.find("file-list-surface").bounds();
    let position = bounds.origin + point(px(28.), px(16. + index as f32 * 32.));
    window.pointer_move(position, None, cx);
    window.dispatch_event(
        MouseDownEvent {
            button: MouseButton::Right,
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
            button: MouseButton::Right,
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
fn right_click_copies_the_clicked_file_root_without_opening_or_changing_selection(
    cx: &mut TestAppContext,
) {
    let path = "src/中文 空格.rs";
    let (window, harness) = harness(
        cx,
        RepositoryTree::default(),
        vec![change(path, ' ', 'M')],
        false,
    );
    for root in [
        repository_root(),
        PathBuf::from("headless-other-worktree/另一 工作树/vendor/子模块"),
    ] {
        cx.update_window(window.into(), |_, _, cx| {
            harness.read(cx).list.clone().update(cx, |list, cx| {
                list.set_data(
                    root.clone(),
                    Arc::new(RepositoryTree::default()),
                    vec![change(path, ' ', 'M')],
                    cx,
                );
            });
        })
        .unwrap();
        settle(cx, window, &harness);
        cx.update_window(window.into(), |_, window, cx| {
            right_click_row(window, 1, cx);
        })
        .unwrap();
        // ContextMenu builds and focuses its menu with window.defer after dispatch.
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("popup-menu").visible());
            for _ in 0..3 {
                window.press("down", cx);
            }
            window.press("enter", cx);
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some(crate::ui::paths::display_path(&root.join(path)))
            );
            assert!(harness.read(cx).list.read(cx).selected.is_none());
        })
        .unwrap();
        assert!(requests(cx, &harness).is_empty());
    }
}

#[gpui_kit::test]
fn right_click_directory_copies_relative_path_without_collapsing(cx: &mut TestAppContext) {
    let (window, harness) = harness(
        cx,
        RepositoryTree {
            entries: vec![
                entry("src", FileTreeKind::Directory),
                entry("src/main.rs", FileTreeKind::File),
            ],
            ..Default::default()
        },
        Vec::new(),
        true,
    );
    cx.update_window(window.into(), |_, window, cx| {
        right_click_row(window, 0, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("popup-menu").visible());
        window.press("down", cx);
        window.press("down", cx);
        window.press("enter", cx);
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("src".into())
        );
        assert!(!harness.read(cx).list.read(cx).collapsed.contains("src"));
    })
    .unwrap();
    assert!(requests(cx, &harness).is_empty());
}

fn entry(path: &str, kind: FileTreeKind) -> FileTreeEntry {
    FileTreeEntry {
        path: path.into(),
        kind,
        index_status: ' ',
        worktree_status: ' ',
    }
}

fn module(path: &str, initialized: bool) -> SubmoduleInfo {
    SubmoduleInfo {
        name: path.into(),
        path: path.into(),
        url: None,
        head_id: None,
        index_id: None,
        workdir_id: None,
        initialized,
        dirty: true,
    }
}

#[gpui_kit::test]
fn conflict_rows_expose_resolution_and_no_stage_button_in_tree_and_grouped_views(
    cx: &mut TestAppContext,
) {
    let statuses = ["DD", "AU", "UD", "UA", "DU", "AA", "UU"];
    for tree_mode in [false, true] {
        let changes = statuses
            .map(|status| {
                let mut chars = status.chars();
                change(
                    &format!("{status}.txt"),
                    chars.next().unwrap(),
                    chars.next().unwrap(),
                )
            })
            .to_vec();
        let tree = RepositoryTree {
            entries: changes
                .iter()
                .map(|change| FileTreeEntry {
                    path: change.path.clone(),
                    kind: FileTreeKind::File,
                    index_status: change.index_status,
                    worktree_status: change.worktree_status,
                })
                .collect(),
            ..Default::default()
        };
        let (window, harness) = harness(cx, tree, changes, tree_mode);
        for appearance in [
            super::super::theme::Appearance::Light,
            super::super::theme::Appearance::Dark,
        ] {
            cx.update_window(window.into(), |_, window, cx| {
                appearance.apply(window, cx);
                window.render_frame(cx);
                for row in usize::from(!tree_mode)..7 + usize::from(!tree_mode) {
                    let file = window.find(("file-row", row));
                    assert_eq!(file.description(), Some("冲突，单击解决"));
                    assert!(window.find(("file-name", row)).visible());
                    assert!(window.try_find(("file-stage", row)).is_none());
                }
                window.click(("file-row", usize::from(!tree_mode)), cx);
            })
            .expect("render conflict rows in both native themes");
        }
        assert_eq!(requests(cx, &harness).len(), 2);
        assert!(
            requests(cx, &harness)
                .iter()
                .all(|request| matches!(request, Request::File { staged: false, .. }))
        );
    }
}

#[gpui_kit::test]
fn labeled_submodule_branch_buttons_are_independent_of_row_navigation(cx: &mut TestAppContext) {
    for tree_mode in [false, true] {
        let tree = RepositoryTree {
            entries: vec![
                entry("engine", FileTreeKind::Submodule),
                entry("missing", FileTreeKind::Submodule),
            ],
            submodules: vec![module("engine", true), module("missing", false)],
            ..Default::default()
        };
        let (window, harness) = harness(
            cx,
            tree,
            vec![change("engine", ' ', 'M'), change("missing", ' ', 'M')],
            tree_mode,
        );
        let first = usize::from(!tree_mode);
        cx.update_window(window.into(), |_, window, cx| {
            let initialized = window.find(("file-module-branch", first));
            let unavailable = window.find(("file-module-branch", first + 1));
            assert_eq!(initialized.label(), Some("分支 ▾"));
            // Enabled native buttons may omit the accessibility disabled flag;
            // the real click and recorded branch request below prove operation.
            assert_eq!(unavailable.disabled(), Some(true));
            window.click(("file-module-branch", first), cx);
            window.click(("file-module-branch", first + 1), cx);
        })
        .expect("click the labeled branch dropdowns");
        assert_eq!(
            requests(cx, &harness),
            [Request::Branches(repository_root().join("engine"))]
        );
        cx.read(|cx| assert!(harness.read(cx).list.read(cx).selected.is_none()));
    }
}

#[gpui_kit::test]
fn submodule_pointer_gestures_preview_gitlinks_and_enter_only_initialized_changes(
    cx: &mut TestAppContext,
) {
    let initialized = "modules/子 模块";
    let uninitialized = "modules/未初始化";
    let tree = RepositoryTree {
        submodules: vec![module(initialized, true), module(uninitialized, false)],
        ..RepositoryTree::default()
    };
    let (window, harness) = harness(
        cx,
        tree,
        vec![
            change(initialized, ' ', 'M'),
            change(uninitialized, ' ', 'M'),
        ],
        false,
    );
    cx.update_window(window.into(), |_, window, cx| {
        click_row(window, 1, 1, cx);
        click_row(window, 1, 2, cx);
        click_row(window, 2, 2, cx);
    })
    .expect("click and double-click both submodule states");
    assert_eq!(
        requests(cx, &harness),
        [
            Request::File {
                path: initialized.into(),
                staged: false,
            },
            Request::File {
                path: initialized.into(),
                staged: false,
            },
            Request::Submodule(repository_root().join(initialized)),
            Request::File {
                path: uninitialized.into(),
                staged: false,
            },
            Request::File {
                path: uninitialized.into(),
                staged: false,
            },
        ]
    );
    cx.read(|cx| assert_eq!(harness.read(cx).list.read(cx).root, repository_root()));
}

#[gpui_kit::test]
fn staged_and_worktree_rows_open_the_selected_side_and_group_click_only_collapses(
    cx: &mut TestAppContext,
) {
    let path = "src/共同 修改.rs";
    let (window, harness) = harness(
        cx,
        RepositoryTree::default(),
        vec![change(path, 'M', 'M')],
        false,
    );
    cx.update_window(window.into(), |_, window, cx| {
        click_row(window, 1, 1, cx);
        click_row(window, 5, 1, cx);
        click_row(window, 4, 1, cx);
    })
    .expect("open worktree and staged sides, then collapse the staged group");
    settle(cx, window, &harness);
    assert_eq!(
        requests(cx, &harness),
        [
            Request::File {
                path: path.into(),
                staged: false,
            },
            Request::File {
                path: path.into(),
                staged: true,
            },
        ]
    );
    cx.read(|cx| {
        let list = harness.read(cx).list.read(cx);
        assert!(list.collapsed.contains("已暂存"));
        assert!(
            !list
                .rows
                .iter()
                .any(|row| matches!(row, Row::File { staged: true, .. }))
        );
    });
    cx.update_window(window.into(), |_, window, cx| {
        click_row(window, 4, 1, cx);
    })
    .expect("expand the staged group");
    settle(cx, window, &harness);
    cx.update_window(window.into(), |_, window, cx| {
        click_row(window, 5, 1, cx);
    })
    .expect("open the restored staged row");
    assert_eq!(
        requests(cx, &harness).last(),
        Some(&Request::File {
            path: path.into(),
            staged: true
        })
    );
}

#[gpui_kit::test]
fn filtering_reveals_a_collapsed_chinese_path_and_matches_ascii_case_insensitively(
    cx: &mut TestAppContext,
) {
    let tree = RepositoryTree {
        entries: vec![
            entry("src", FileTreeKind::Directory),
            entry("src/main.rs", FileTreeKind::File),
            entry("资源", FileTreeKind::Directory),
            entry("资源/说明.md", FileTreeKind::File),
            entry("README.md", FileTreeKind::File),
        ],
        ..RepositoryTree::default()
    };
    let (window, harness) = harness(cx, tree, Vec::new(), true);
    cx.update_window(window.into(), |_, window, cx| {
        click_row(window, 2, 1, cx);
    })
    .expect("collapse the Chinese directory");
    settle(cx, window, &harness);
    cx.read(|cx| {
        let list = harness.read(cx).list.read(cx);
        assert!(list.collapsed.contains("资源"));
        assert!(
            !list
                .rows
                .iter()
                .any(|row| matches!(row, Row::File { path, .. } if path == "资源/说明.md"))
        );
    });
    assert!(requests(cx, &harness).is_empty());

    cx.update_window(window.into(), |_, window, cx| {
        window.click("file-filter", cx);
        window.input("说明", cx);
    })
    .expect("type the Chinese filename filter");
    settle(cx, window, &harness);
    cx.update_window(window.into(), |_, window, cx| {
        click_row(window, 1, 1, cx);
        window.click("file-filter", cx);
        window.press("ctrl-a", cx);
        window.input("MAIN.RS", cx);
    })
    .expect("open the Chinese match and type a case-insensitive source filter");
    settle(cx, window, &harness);
    cx.update_window(window.into(), |_, window, cx| {
        click_row(window, 1, 1, cx);
    })
    .expect("open the source match");
    assert_eq!(
        requests(cx, &harness),
        [
            Request::File {
                path: "资源/说明.md".into(),
                staged: false
            },
            Request::File {
                path: "src/main.rs".into(),
                staged: false
            },
        ]
    );
}

#[gpui_kit::test]
fn filtering_submodule_paths_highlights_original_unicode_and_keeps_double_click_navigation(
    cx: &mut TestAppContext,
) {
    let path = "modules/İstanbul子模块";
    let tree = RepositoryTree {
        entries: vec![
            entry("modules", FileTreeKind::Directory),
            entry(path, FileTreeKind::Submodule),
        ],
        submodules: vec![module(path, true)],
        ..RepositoryTree::default()
    };
    let (window, harness) = harness(cx, tree, vec![change(path, ' ', 'M')], true);
    for (query, expected) in [("MODULES\\İ", "modules/İ"), ("子模块", "子模块")] {
        cx.update_window(window.into(), |_, window, cx| {
            window.click("file-filter", cx);
            window.press("ctrl-a", cx);
            window.input(query, cx);
        })
        .unwrap();
        settle(cx, window, &harness);
        cx.read(|cx| {
            let list = harness.read(cx).list.read(cx);
            assert_eq!(list.rows.len(), 2);
            let Row::File {
                path: actual,
                search_matches: Some(matches),
                ..
            } = &list.rows[1]
            else {
                panic!("filtered submodule must retain its highlighted relative path");
            };
            assert_eq!(actual, path);
            assert_eq!(
                matches
                    .iter()
                    .map(|range| &actual[range.clone()])
                    .collect::<Vec<_>>(),
                [expected]
            );
        });
    }
    cx.update_window(window.into(), |_, window, cx| {
        click_row(window, 1, 2, cx);
    })
    .unwrap();
    assert_eq!(
        requests(cx, &harness),
        [
            Request::File {
                path: path.into(),
                staged: false
            },
            Request::Submodule(repository_root().join(path)),
        ]
    );
}
