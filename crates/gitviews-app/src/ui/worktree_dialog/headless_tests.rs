//! The actual native controls exercised through GPUI's simulated platform.

use std::{cell::RefCell, path::PathBuf, rc::Rc};

use gpui_kit::{
    AppContext, Context, Entity, FocusHandle, IntoElement, ParentElement, Render, Styled,
    Subscription, TestAppContext, Window, WindowHandle, base::Root, component::button::Button, div,
    px, size, test::TestWindowExt,
};

use super::{RepositoryDetails, WorktreeAction, WorktreeDialog, WorktreeDialogEvent, WorktreeInfo};

#[derive(Clone, Debug, Eq, PartialEq)]
enum Request {
    Apply(WorktreeAction),
    Reload,
    Close,
}

struct Harness {
    dialog: Entity<WorktreeDialog>,
    outside_focus: FocusHandle,
    requests: Rc<RefCell<Vec<Request>>>,
    _subscription: Subscription,
}

impl Harness {
    fn new(details: RepositoryDetails, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let outside_focus = cx.focus_handle();
        outside_focus.focus(window, cx);
        let dialog = cx.new(|cx| {
            let mut dialog = WorktreeDialog::new(cx);
            dialog.show(path("current"), details, cx);
            dialog
        });
        let requests = Rc::new(RefCell::new(Vec::new()));
        let recorded = Rc::clone(&requests);
        let subscription = cx.subscribe(&dialog, move |_, _, event, _| {
            recorded.borrow_mut().push(match event {
                WorktreeDialogEvent::Apply(action) => Request::Apply(action.clone()),
                WorktreeDialogEvent::Reload => Request::Reload,
                WorktreeDialogEvent::Close => Request::Close,
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
                Button::new("outside-worktrees")
                    .label("Outside")
                    .track_focus(&self.outside_focus),
            )
            .child(self.dialog.clone())
    }
}

fn path(name: &str) -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("C:/headless-worktrees").join(name)
    } else {
        PathBuf::from("/headless-worktrees").join(name)
    }
}

fn details() -> RepositoryDetails {
    RepositoryDetails {
        worktrees: ["main", "current", "other"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| WorktreeInfo {
                path: path(name),
                branch: Some(name.into()),
                is_main: index == 0,
                ..WorktreeInfo::default()
            })
            .collect(),
        ..RepositoryDetails::default()
    }
}

fn harness(
    cx: &mut TestAppContext,
    details: RepositoryDetails,
) -> (WindowHandle<Root>, Entity<Harness>) {
    let (window, view) =
        crate::ui::headless::open_window(cx, size(px(1150.), px(950.)), move |window, cx| {
            cx.new(|cx| Harness::new(details, window, cx))
        });
    cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
        .expect("draw worktree manager");
    (window, view)
}

fn requests(cx: &TestAppContext, harness: &Entity<Harness>) -> Vec<Request> {
    cx.read(|cx| harness.read(cx).requests.borrow().clone())
}

fn input(window: &mut Window, id: &'static str, value: &str, cx: &mut gpui_kit::App) {
    window.click(id, cx);
    window.press("ctrl-a", cx);
    window.input(value, cx);
    assert_eq!(window.find(id).value(), Some(value));
}

#[cfg(windows)]
#[gpui_kit::test]
fn verbatim_worktree_paths_are_readable_and_share_the_current_repository_identity(
    cx: &mut TestAppContext,
) {
    let mut details = details();
    for worktree in &mut details.worktrees {
        worktree.path = PathBuf::from(format!(
            r"\\?\C:\headless-worktrees\{}",
            worktree.branch.as_deref().expect("fixture branch")
        ));
    }
    let stored_other = details.worktrees[2].path.clone();
    let (handle, harness) = harness(cx, details.clone());
    cx.update_window(handle.into(), |_, window, cx| {
        let label = window
            .find(("worktree-select", 1usize))
            .label()
            .expect("current worktree label")
            .to_owned();
        assert!(label.contains("当前"));
        assert!(!label.contains(r"\\?\"));
        assert_eq!(window.find("worktree-mode-remove").disabled(), Some(true));
        assert_eq!(window.find("worktree-mode-move").disabled(), Some(true));
        harness.read(cx).dialog.clone().update(cx, |dialog, cx| {
            dialog.show(
                PathBuf::from(r"\\?\C:\headless-worktrees\current"),
                details,
                cx,
            );
        });
        window.render_frame(cx);
        assert_eq!(
            window.find("worktree-root-path").label(),
            Some(r"C:\headless-worktrees\current")
        );
        window.click(("worktree-select", 2usize), cx);
        window.click("worktree-mode-remove", cx);
        assert_eq!(
            window.find("worktree-selected-path").label(),
            Some(r"C:\headless-worktrees\other")
        );
        window.click("worktree-apply", cx);
        let warning = window
            .find("worktree-confirmation")
            .label()
            .expect("readable removal confirmation")
            .to_owned();
        assert!(warning.contains(r"C:\headless-worktrees\other"));
        assert!(!warning.contains(r"\\?\"));
        window.click("worktree-confirm-apply", cx);
    })
    .expect("normalize only display and identity, retaining the original action path");
    assert_eq!(
        requests(cx, &harness),
        [Request::Apply(WorktreeAction::Remove {
            path: stored_other,
            force: false,
        })]
    );
}

#[gpui_kit::test]
fn create_validates_absolute_path_and_emits_chinese_path_and_explicit_branch(
    cx: &mut TestAppContext,
) {
    let (handle, harness) = harness(cx, details());
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("worktree-mode-create", cx);
        window.click("worktree-apply", cx);
        assert!(window.try_find("worktree-error").is_some());
        input(window, "worktree-create-path", "relative/path", cx);
        input(window, "worktree-branch", "feature/中文", cx);
    })
    .expect("fill relative path");
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("worktree-apply", cx);
        assert!(window.try_find("worktree-error").is_some());
    })
    .expect("reject relative path");
    assert!(requests(cx, &harness).is_empty());
    let destination = path("中文工作树");
    cx.update_window(handle.into(), |_, window, cx| {
        input(
            window,
            "worktree-create-path",
            &destination.to_string_lossy(),
            cx,
        );
    })
    .expect("type absolute Chinese path");
    cx.update_window(handle.into(), |_, window, cx| {
        window.press("enter", cx);
    })
    .expect("submit with Enter");
    assert_eq!(
        requests(cx, &harness),
        [Request::Apply(WorktreeAction::Create {
            path: destination,
            branch: "feature/中文".into(),
            create_branch: true,
        })]
    );
}

#[gpui_kit::test]
fn busy_blocks_duplicate_actions_close_and_escape_and_failure_keeps_draft(cx: &mut TestAppContext) {
    let (handle, harness) = harness(cx, details());
    let destination = path("new-worktree");
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("worktree-mode-create", cx);
        window.click("worktree-branch-existing", cx);
        input(
            window,
            "worktree-create-path",
            &destination.to_string_lossy(),
            cx,
        );
        input(window, "worktree-branch", "feature/existing", cx);
    })
    .expect("type existing branch draft");
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("worktree-apply", cx);
        for id in [
            "worktree-apply",
            "worktree-close",
            "worktree-reload",
            "worktree-mode-remove",
        ] {
            assert_eq!(window.find(id).disabled(), Some(true));
            window.click(id, cx);
        }
        window.click(("worktree-select", 0usize), cx);
        window.press("escape", cx);
        assert!(window.find("worktree-close").visible());
    })
    .expect("busy modal rejects duplicate actions and closure");
    let expected = Request::Apply(WorktreeAction::Create {
        path: destination.clone(),
        branch: "feature/existing".into(),
        create_branch: false,
    });
    assert_eq!(
        requests(cx, &harness).as_slice(),
        std::slice::from_ref(&expected)
    );
    cx.update_window(handle.into(), |_, window, cx| {
        harness.read(cx).dialog.clone().update(cx, |dialog, cx| {
            dialog.set_error(Some("Branch is already checked out".into()), cx);
        });
        window.render_frame(cx);
        assert_eq!(
            window.find("worktree-create-path").value(),
            Some(destination.to_string_lossy().as_ref())
        );
        assert_eq!(
            window.find("worktree-branch").value(),
            Some("feature/existing")
        );
        assert!(window.find("worktree-error").visible());
        window.click("worktree-apply", cx);
    })
    .expect("retry retains all edited values");
    assert_eq!(requests(cx, &harness), [expected.clone(), expected]);
}

#[gpui_kit::test]
fn main_and_current_are_protected_and_remove_needs_explicit_second_confirmation(
    cx: &mut TestAppContext,
) {
    let (handle, harness) = harness(cx, details());
    cx.update_window(handle.into(), |_, window, cx| {
        for index in [0usize, 1usize] {
            window.click(("worktree-select", index), cx);
            for id in [
                "worktree-mode-remove",
                "worktree-mode-move",
                "worktree-mode-prune",
            ] {
                assert_eq!(window.find(id).disabled(), Some(true));
            }
        }
        window.click(("worktree-select", 2usize), cx);
        assert!(window.try_find("worktree-force-remove").is_none());
        window.click("worktree-mode-remove", cx);
        assert_eq!(window.find("worktree-force-remove").checked(), Some(false));
        window.click("worktree-apply", cx);
        assert!(window.find("worktree-confirmation").visible());
        assert_eq!(
            window.find("worktree-selected-path").label(),
            Some(gitviews_service::display_path(&path("other")).as_str())
        );
        assert_eq!(window.find("worktree-apply").disabled(), Some(true));
        window.press("enter", cx);
    })
    .expect("review exact removal path");
    assert!(requests(cx, &harness).is_empty());
    cx.update_window(handle.into(), |_, window, cx| {
        window.press("escape", cx);
        assert!(window.try_find("worktree-confirmation").is_none());
        assert!(window.find("worktree-close").visible());
        window.click("worktree-apply", cx);
        window.click("worktree-confirm-apply", cx);
        window.click("worktree-confirm-apply", cx);
    })
    .expect("explicitly confirm once");
    assert_eq!(
        requests(cx, &harness),
        [Request::Apply(WorktreeAction::Remove {
            path: path("other"),
            force: false,
        })]
    );
}

#[gpui_kit::test]
fn force_remove_is_an_explicit_choice_and_changed_choice_requires_new_confirmation(
    cx: &mut TestAppContext,
) {
    let (handle, harness) = harness(cx, details());
    cx.update_window(handle.into(), |_, window, cx| {
        window.click(("worktree-select", 2usize), cx);
        window.click("worktree-mode-remove", cx);
        harness.read(cx).dialog.clone().update(cx, |dialog, cx| {
            dialog.set_error(Some("工作树包含未提交文件".into()), cx);
        });
        window.render_frame(cx);
        assert_eq!(window.find("worktree-force-remove").checked(), Some(false));
        window.click("worktree-force-remove", cx);
        assert_eq!(window.find("worktree-force-remove").checked(), Some(true));
        assert!(window.try_find("worktree-error").is_none());
        window.click("worktree-apply", cx);
        let warning = window.find("worktree-confirmation");
        let warning = warning.label().expect("force removal warning");
        assert!(warning.contains(gitviews_service::display_path(&path("other")).as_str()));
        assert!(warning.contains("子模块"));
        assert!(warning.contains("永久丢弃"));
        assert_eq!(
            window.find("worktree-confirm-apply").label(),
            Some("确认丢弃改动并强制移除")
        );
        window.press("enter", cx);
    })
    .expect("review explicit force removal and data loss warning");
    assert!(requests(cx, &harness).is_empty());
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("worktree-force-remove", cx);
        assert_eq!(window.find("worktree-force-remove").checked(), Some(false));
        assert!(window.try_find("worktree-confirmation").is_none());
        window.click("worktree-force-remove", cx);
        assert!(window.try_find("worktree-confirmation").is_none());
        window.click("worktree-apply", cx);
    })
    .expect("changing force choice invalidates the old confirmation");
    assert!(requests(cx, &harness).is_empty());
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("worktree-confirm-apply", cx);
        assert_eq!(window.find("worktree-force-remove").disabled(), Some(true));
        window.click("worktree-confirm-apply", cx);
    })
    .expect("only the second explicit confirmation dispatches force removal");
    assert_eq!(
        requests(cx, &harness),
        [Request::Apply(WorktreeAction::Remove {
            path: path("other"),
            force: true,
        })]
    );
}

#[gpui_kit::test]
fn force_remove_never_carries_to_another_action_target_or_dialog_open(cx: &mut TestAppContext) {
    let worktrees = details();
    let (handle, harness) = harness(cx, worktrees.clone());
    cx.update_window(handle.into(), |_, window, cx| {
        window.click(("worktree-select", 2usize), cx);
        window.click("worktree-mode-remove", cx);
        window.click("worktree-force-remove", cx);
        window.click("worktree-mode-move", cx);
        assert!(window.try_find("worktree-force-remove").is_none());
        window.click("worktree-mode-remove", cx);
        assert_eq!(window.find("worktree-force-remove").checked(), Some(false));
        window.click("worktree-force-remove", cx);
        window.click(("worktree-select", 1usize), cx);
        assert!(window.try_find("worktree-force-remove").is_none());
        window.click(("worktree-select", 2usize), cx);
        window.click("worktree-mode-remove", cx);
        assert_eq!(window.find("worktree-force-remove").checked(), Some(false));
        window.click("worktree-force-remove", cx);
        harness.read(cx).dialog.clone().update(cx, |dialog, cx| {
            dialog.replace_details(worktrees.clone(), cx);
        });
        window.render_frame(cx);
        window.click("worktree-mode-remove", cx);
        assert_eq!(window.find("worktree-force-remove").checked(), Some(false));
        window.click("worktree-force-remove", cx);
        window.press("escape", cx);
        harness.read(cx).dialog.clone().update(cx, |dialog, cx| {
            dialog.show(path("current"), worktrees, cx);
        });
        window.render_frame(cx);
        window.click(("worktree-select", 2usize), cx);
        window.click("worktree-mode-remove", cx);
        assert_eq!(window.find("worktree-force-remove").checked(), Some(false));
    })
    .expect("force removal choice resets after mode, target, success refresh, and reopen");
    assert_eq!(requests(cx, &harness), [Request::Close]);
}

#[gpui_kit::test]
fn locked_worktree_cannot_be_removed_moved_or_pruned_but_can_be_unlocked(cx: &mut TestAppContext) {
    let mut details = details();
    details.worktrees[2].locked = true;
    details.worktrees[2].prunable = true;
    let (handle, harness) = harness(cx, details);
    cx.update_window(handle.into(), |_, window, cx| {
        window.click(("worktree-select", 2usize), cx);
        for id in [
            "worktree-mode-remove",
            "worktree-mode-move",
            "worktree-mode-prune",
            "worktree-mode-lock",
        ] {
            assert_eq!(window.find(id).disabled(), Some(true));
        }
        window.click("worktree-mode-unlock", cx);
        window.click("worktree-apply", cx);
    })
    .expect("unlock the protected registration");
    assert_eq!(
        requests(cx, &harness),
        [Request::Apply(WorktreeAction::Unlock {
            path: path("other")
        })]
    );
}

#[gpui_kit::test]
fn prune_only_targets_the_selected_missing_registration_and_requires_confirmation(
    cx: &mut TestAppContext,
) {
    let mut details = details();
    details.worktrees[2].prunable = true;
    let (handle, harness) = harness(cx, details);
    cx.update_window(handle.into(), |_, window, cx| {
        window.click(("worktree-select", 2usize), cx);
        window.click("worktree-mode-prune", cx);
        window.click("worktree-apply", cx);
        assert!(window.find("worktree-confirmation").visible());
    })
    .expect("review prune confirmation");
    assert!(requests(cx, &harness).is_empty());
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("worktree-confirm-apply", cx);
    })
    .expect("confirm selected registration prune");
    assert_eq!(
        requests(cx, &harness),
        [Request::Apply(WorktreeAction::Prune {
            path: path("other")
        })]
    );
}

#[gpui_kit::test]
fn focus_stays_inside_modal_and_escape_restores_original_focus(cx: &mut TestAppContext) {
    let (handle, harness) = harness(cx, details());
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("worktree-mode-create", cx);
        for key in ["tab", "shift-tab"] {
            for _ in 0..34 {
                window.press(key, cx);
                assert!(
                    harness
                        .read(cx)
                        .dialog
                        .read(cx)
                        .focus
                        .contains_focused(window, cx)
                );
                assert_eq!(window.find("outside-worktrees").focused(), Some(false));
            }
        }
        window.press("escape", cx);
        assert!(window.try_find("worktree-close").is_none());
        assert!(harness.read(cx).outside_focus.is_focused(window));
    })
    .expect("focus trap and close");
    assert_eq!(requests(cx, &harness), [Request::Close]);
}

#[gpui_kit::test]
fn reload_is_single_flight_and_replaces_stale_selection(cx: &mut TestAppContext) {
    let (handle, harness) = harness(cx, details());
    cx.update_window(handle.into(), |_, window, cx| {
        window.click(("worktree-select", 2usize), cx);
        window.click("worktree-reload", cx);
        window.click("worktree-reload", cx);
        window.press("escape", cx);
    })
    .expect("request one background reload");
    assert_eq!(requests(cx, &harness), [Request::Reload]);
    cx.update_window(handle.into(), |_, window, cx| {
        let mut updated = details();
        updated.worktrees.pop();
        harness
            .read(cx)
            .dialog
            .clone()
            .update(cx, |dialog, cx| dialog.replace_details(updated, cx));
        window.render_frame(cx);
        assert!(window.try_find(("worktree-select", 2usize)).is_none());
        assert_eq!(window.find("worktree-mode-remove").disabled(), Some(true));
        window.press("escape", cx);
        assert!(harness.read(cx).outside_focus.is_focused(window));
    })
    .expect("refresh removes stale row and permits closing");
    assert_eq!(requests(cx, &harness), [Request::Reload, Request::Close]);
}

#[gpui_kit::test]
fn move_repair_and_lock_preserve_the_exact_selected_path_and_form_values(cx: &mut TestAppContext) {
    let (handle, harness) = harness(cx, details());
    let destination = path("移动目标");
    cx.update_window(handle.into(), |_, window, cx| {
        window.click(("worktree-select", 2usize), cx);
        window.click("worktree-mode-move", cx);
        input(
            window,
            "worktree-destination",
            &destination.to_string_lossy(),
            cx,
        );
    })
    .expect("type move destination");
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("worktree-apply", cx);
    })
    .expect("request selected worktree move");
    assert_eq!(
        requests(cx, &harness),
        [Request::Apply(WorktreeAction::Move {
            path: path("other"),
            destination: destination.clone(),
        })]
    );
    cx.update_window(handle.into(), |_, window, cx| {
        harness.read(cx).dialog.clone().update(cx, |dialog, cx| {
            dialog.set_error(Some("Move refused".into()), cx)
        });
        window.render_frame(cx);
        window.click("worktree-mode-repair", cx);
        input(
            window,
            "worktree-destination",
            &destination.to_string_lossy(),
            cx,
        );
    })
    .expect("type repair destination after move failure");
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("worktree-apply", cx);
    })
    .expect("request registration repair");
    cx.update_window(handle.into(), |_, window, cx| {
        harness.read(cx).dialog.clone().update(cx, |dialog, cx| {
            dialog.set_error(Some("Repair refused".into()), cx)
        });
        window.render_frame(cx);
        window.click("worktree-mode-lock", cx);
        input(window, "worktree-lock-reason", "移动磁盘暂时离线", cx);
    })
    .expect("type lock reason");
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("worktree-apply", cx);
    })
    .expect("request registration lock");
    assert_eq!(
        requests(cx, &harness),
        [
            Request::Apply(WorktreeAction::Move {
                path: path("other"),
                destination: destination.clone()
            }),
            Request::Apply(WorktreeAction::Repair {
                path: path("other"),
                destination
            }),
            Request::Apply(WorktreeAction::Lock {
                path: path("other"),
                reason: "移动磁盘暂时离线".into()
            }),
        ]
    );
}

#[gpui_kit::test]
fn background_refresh_keeps_creation_draft_but_never_moves_an_action_to_a_fallback_target(
    cx: &mut TestAppContext,
) {
    let (handle, harness) = harness(cx, details());
    let destination = path("保留输入");
    cx.update_window(handle.into(), |_, window, cx| {
        window.click("worktree-mode-create", cx);
        input(
            window,
            "worktree-create-path",
            &destination.to_string_lossy(),
            cx,
        );
        input(window, "worktree-branch", "feature/preserve", cx);
    })
    .expect("type creation draft before metadata completes");
    cx.update_window(handle.into(), |_, window, cx| {
        harness
            .read(cx)
            .dialog
            .clone()
            .update(cx, |dialog, cx| dialog.refresh_details(details(), cx));
        window.render_frame(cx);
        assert_eq!(
            window.find("worktree-create-path").value(),
            Some(destination.to_string_lossy().as_ref())
        );
        assert_eq!(
            window.find("worktree-branch").value(),
            Some("feature/preserve")
        );
        window.click(("worktree-select", 2usize), cx);
        window.click("worktree-mode-remove", cx);
        window.click("worktree-apply", cx);
        assert!(window.find("worktree-confirmation").visible());
        let mut updated = details();
        updated.worktrees.pop();
        harness
            .read(cx)
            .dialog
            .clone()
            .update(cx, |dialog, cx| dialog.refresh_details(updated, cx));
        window.render_frame(cx);
        assert!(window.try_find("worktree-confirmation").is_none());
        assert!(window.try_find("worktree-selected-path").is_none());
        assert_eq!(window.find("worktree-apply").disabled(), Some(true));
    })
    .expect("never transfer a removed target's action to the current worktree");
    assert!(requests(cx, &harness).is_empty());
}
