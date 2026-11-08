// Included by headless_tests.rs. Every selection below goes through the real
// registered-submodule navigation, metadata read, file-tree read and branch read.

fn scope_fixture(cx: &mut TestAppContext) -> Fixture {
    let fixture = Fixture::new(cx);
    git(
        &fixture.root,
        &[
            "remote",
            "add",
            "upstream",
            "https://example.invalid/parent",
        ],
    );
    git(
        &fixture.child,
        &["remote", "add", "origin", "https://example.invalid/child"],
    );
    git(
        &fixture.root,
        &[
            "update-ref",
            "refs/remotes/upstream/parent-remote",
            "parent-feature",
        ],
    );
    git(
        &fixture.child,
        &[
            "update-ref",
            "refs/remotes/origin/child-remote",
            "child-feature",
        ],
    );
    // A name available in both scopes catches stale actions that would otherwise
    // merely fail because the selected branch does not exist in the other repo.
    git(
        &fixture.root,
        &["branch", "shared-feature", "parent-feature"],
    );
    git(
        &fixture.child,
        &["branch", "shared-feature", "child-feature"],
    );
    cx.update(|cx| {
        fixture.workspace.update(cx, |view, cx| {
            view.show_parent_changes(cx);
            view.service.invalidate(&fixture.root);
            view.service.invalidate(&fixture.child);
            view.details = Some(view.service.details(&fixture.root).unwrap());
            view.expanded_remotes
                .extend(["origin".into(), "upstream".into()]);
            cx.notify();
        });
    });
    cx.run_until_parked();
    fixture
}

fn enter_scope_child(fixture: &Fixture, cx: &mut TestAppContext) {
    cx.update(|cx| {
        fixture.workspace.update(cx, |view, cx| {
            assert!(view.submodule_changes.is_none());
            view.select_submodule(fixture.child.clone(), cx);
            assert!(view.submodule_loading);
        });
    });
    cx.run_until_parked();
    cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        assert!(!view.submodule_loading, "{:?}", view.notice);
        assert!(view.submodule_task.is_none());
        assert!(view.branch_context_task.is_none());
        assert!(gitviews_service::paths_equal(
            &view
                .changes_repository()
                .expect("registered child navigation succeeded")
                .root,
            &fixture.child,
        ));
        assert!(
            view.branch_picker_details().is_some(),
            "sidebar branches load without opening the picker"
        );
    });
}

fn scope_branch_actions(
    fixture: &Fixture,
    light: bool,
    cx: &TestAppContext,
) -> Vec<(String, super::super::WorkbenchAction)> {
    cx.read(|cx| {
        let mut actions = std::collections::HashMap::new();
        fixture.workspace.read(cx).make_tree(light, &mut actions);
        let mut branches = actions
            .into_iter()
            .filter(|(_, action)| {
                matches!(action, super::super::WorkbenchAction::CheckoutBranch { .. })
            })
            .collect::<Vec<_>>();
        branches.sort_by(|left, right| left.0.cmp(&right.0));
        branches
    })
}

fn scope_branch_action(
    fixture: &Fixture,
    name: &str,
    remote: bool,
    cx: &TestAppContext,
) -> (String, super::super::WorkbenchAction) {
    scope_branch_actions(fixture, false, cx)
        .into_iter()
        .find(|(_, action)| {
            matches!(action, super::super::WorkbenchAction::CheckoutBranch {
            name: candidate, remote: candidate_remote, ..
        } if candidate == name && *candidate_remote == remote)
        })
        .unwrap_or_else(|| panic!("rendered sidebar is missing {name}, remote={remote}"))
}

fn assert_scope_sidebar(fixture: &Fixture, child: bool, cx: &TestAppContext) {
    let expected_root = if child { &fixture.child } else { &fixture.root };
    let expected_local = if child {
        vec![
            "child-feature",
            "child-feature-old",
            "main",
            "shared-feature",
        ]
    } else {
        vec!["main", "parent-feature", "shared-feature"]
    };
    let expected_remote = if child {
        "origin/child-remote"
    } else {
        "upstream/parent-remote"
    };
    for light in [false, true] {
        let branches = scope_branch_actions(fixture, light, cx);
        let mut local_names = Vec::new();
        let mut remote_names = Vec::new();
        for (_, action) in branches {
            if let super::super::WorkbenchAction::CheckoutBranch { name, remote, root } = action {
                assert!(gitviews_service::paths_equal(
                    root.as_deref().unwrap(),
                    expected_root
                ));
                if remote {
                    remote_names.push(name);
                } else {
                    local_names.push(name);
                }
            }
        }
        local_names.sort();
        assert_eq!(local_names, expected_local, "light={light}, child={child}");
        assert_eq!(
            remote_names,
            [expected_remote],
            "light={light}, child={child}"
        );
    }
}

#[gpui_kit::test]
fn sidebar_local_and_remote_branches_follow_real_child_navigation_and_return(
    cx: &mut TestAppContext,
) {
    let fixture = scope_fixture(cx);
    assert_scope_sidebar(&fixture, false, cx);
    enter_scope_child(&fixture, cx);
    assert_scope_sidebar(&fixture, true, cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("parent-changes").visible());
        window.click("parent-changes", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_scope_sidebar(&fixture, false, cx);
    cx.read(|cx| assert!(fixture.workspace.read(cx).submodule_changes.is_none()));
}

#[gpui_kit::test]
fn rendered_child_sidebar_double_click_switches_only_child_local_branch(cx: &mut TestAppContext) {
    let fixture = scope_fixture(cx);
    enter_scope_child(&fixture, cx);
    let (id, _) = scope_branch_action(&fixture, "child-feature", false, cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window
                .find(gpui_kit::SharedString::from(id.clone()))
                .visible()
        );
        window.double_click(gpui_kit::SharedString::from(id), cx);
        assert!(fixture.workspace.read(cx).mutation_task.is_some());
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        git(&fixture.root, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
    assert_eq!(
        git(&fixture.child, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "child-feature"
    );
    cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        assert!(view.mutation_task.is_none());
        assert_eq!(
            view.repository.as_ref().unwrap().head,
            Head::Branch("main".into())
        );
        assert_eq!(
            view.changes_repository().unwrap().head,
            Head::Branch("child-feature".into())
        );
        assert!(
            view.branch_picker_details()
                .unwrap()
                .branches
                .iter()
                .any(|branch| { branch.name == "child-feature" && branch.is_current })
        );
    });
}

#[gpui_kit::test]
fn rendered_child_remote_branch_checkout_creates_child_tracking_branch(cx: &mut TestAppContext) {
    let fixture = scope_fixture(cx);
    enter_scope_child(&fixture, cx);
    let (id, _) = scope_branch_action(&fixture, "origin/child-remote", true, cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window
                .find(gpui_kit::SharedString::from(id.clone()))
                .visible()
        );
        window.double_click(gpui_kit::SharedString::from(id), cx);
        assert!(fixture.workspace.read(cx).mutation_task.is_some());
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        git(&fixture.root, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
    assert_eq!(
        git(&fixture.child, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "child-remote"
    );
    assert_eq!(
        git(
            &fixture.child,
            &["rev-parse", "--abbrev-ref", "@{upstream}"]
        )
        .trim(),
        "origin/child-remote"
    );
    assert_eq!(
        git(&fixture.child, &["rev-parse", "HEAD"]).trim(),
        git(&fixture.child, &["rev-parse", "child-feature"]).trim()
    );
    cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        assert!(view.mutation_task.is_none());
        assert_eq!(
            view.changes_repository().unwrap().head,
            Head::Branch("child-remote".into())
        );
    });
}

#[gpui_kit::test]
fn stale_sidebar_action_never_switches_a_same_named_branch_in_another_scope(
    cx: &mut TestAppContext,
) {
    let fixture = scope_fixture(cx);
    let (_, parent_action) = scope_branch_action(&fixture, "shared-feature", false, cx);
    enter_scope_child(&fixture, cx);
    let (_, child_action) = scope_branch_action(&fixture, "shared-feature", false, cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        fixture.workspace.update(cx, |view, cx| {
            view.handle_workbench(&parent_action, window, cx);
            assert!(view.mutation_task.is_none());
            assert!(view.notice.as_deref().unwrap().contains("当前仓库已变化"));
            view.show_parent_changes(cx);
            view.handle_workbench(&child_action, window, cx);
            assert!(view.mutation_task.is_none());
            assert!(view.notice.as_deref().unwrap().contains("当前仓库已变化"));
        });
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        git(&fixture.root, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
    assert_eq!(
        git(&fixture.child, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
}

#[gpui_kit::test]
fn child_sidebar_copy_commit_uses_child_oid_for_a_shared_branch_name(cx: &mut TestAppContext) {
    let fixture = scope_fixture(cx);
    let child_id = git(&fixture.child, &["rev-parse", "shared-feature"])
        .trim()
        .to_owned();
    let parent_id = git(&fixture.root, &["rev-parse", "shared-feature"])
        .trim()
        .to_owned();
    assert_ne!(child_id, parent_id);
    enter_scope_child(&fixture, cx);
    let (id, _) = scope_branch_action(&fixture, "shared-feature", false, cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.right_click(gpui_kit::SharedString::from(id), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("popup-menu").visible());
        // Name, full ref, commit ID: choose the third actual context-menu item.
        window.press("down", cx);
        window.press("down", cx);
        window.press("down", cx);
        window.press("enter", cx);
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(child_id)
        );
        let view = fixture.workspace.read(cx);
        assert!(view.mutation_task.is_none());
        assert_eq!(
            view.repository.as_ref().unwrap().head,
            Head::Branch("main".into())
        );
        assert_eq!(
            view.changes_repository().unwrap().head,
            Head::Branch("main".into())
        );
    })
    .unwrap();
}
