// Included by headless_tests.rs so these native pointer tests reuse its isolated
// parent/child repository fixture rather than bypassing the rendered toolbar.

fn dropdown_control_id(
    fixture: &Fixture,
    name: &str,
    light: bool,
    cx: &TestAppContext,
) -> gpui_kit::SharedString {
    fn find<'a>(node: &'a crate::ui::design_renderer::DesignNode, name: &str) -> Option<&'a str> {
        if node.name == name {
            Some(node.id.as_str())
        } else {
            node.children.iter().find_map(|child| find(child, name))
        }
    }
    cx.read(|cx| {
        let tree = fixture
            .workspace
            .read(cx)
            .make_tree(light, &mut std::collections::HashMap::new());
        find(&tree, name)
            .expect("toolbar branch control")
            .to_owned()
            .into()
    })
}

fn assert_toolbar_branch_pointer_toggle(cx: &mut TestAppContext, light: bool, child: bool) {
    let fixture = Fixture::new(cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        if light {
            crate::ui::theme::Appearance::Light.apply(window, cx);
        }
        fixture.workspace.update(cx, |view, cx| {
            if !child {
                view.submodule_changes = None;
            }
            view.notice = None;
            cx.notify();
        });
        window.render_frame(cx);
    })
    .unwrap();
    for name in ["分支名称", "当前分支图标", "分支切换"] {
        let id = dropdown_control_id(&fixture, name, light, cx);
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click(id.clone(), cx);
            assert_eq!(
                fixture.workspace.read(cx).menu,
                Some("分支"),
                "{name}: a single pointer click must open the branch picker; light={light}, child={child}"
            );
        }).unwrap();
        cx.run_until_parked();
        cx.update_window(fixture.window.into(), |_, window, cx| {
            window.render_frame(cx);
            let panel = window.find("branch-picker");
            assert!(
                panel.visible(),
                "{name}: the branch menu must be inside the viewport; bounds={:?}",
                panel.bounds()
            );
            assert_eq!(
                panel.bounds().origin,
                gpui_kit::point(px(244.), px(91.)),
                "{name}: the branch menu must be anchored below its toolbar trigger"
            );
            let view = fixture.workspace.read(cx);
            let expected = if child {
                "child-feature"
            } else {
                "parent-feature"
            };
            assert!(
                view.branch_picker_details()
                    .unwrap()
                    .branches
                    .iter()
                    .any(|branch| branch.name == expected)
            );
            let search = window.find("branch-picker-search");
            assert!(
                search.visible(),
                "{name}: the branch search must be reachable; bounds={:?}",
                search.bounds()
            );
            assert!(search.bounds().origin.y >= panel.bounds().origin.y);
            window.click("branch-picker-search", cx);
            window.input("feature", cx);
            assert_eq!(window.find("branch-picker-search").value(), Some("feature"));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(fixture.window.into(), |_, window, cx| {
            assert_eq!(fixture.workspace.read(cx).branch_query, "feature");
            window.click(id.clone(), cx);
            assert_eq!(
                fixture.workspace.read(cx).menu,
                None,
                "{name}: clicking the open picker's trigger must close it exactly once; light={light}, child={child}"
            );
        }).unwrap();
        cx.run_until_parked();
    }
}

#[gpui_kit::test]
fn toolbar_branch_pointer_controls_toggle_in_dark_parent_scope(cx: &mut TestAppContext) {
    assert_toolbar_branch_pointer_toggle(cx, false, false);
}

#[gpui_kit::test]
fn toolbar_branch_pointer_controls_toggle_in_light_parent_scope(cx: &mut TestAppContext) {
    assert_toolbar_branch_pointer_toggle(cx, true, false);
}

#[gpui_kit::test]
fn toolbar_branch_pointer_controls_toggle_in_dark_child_scope(cx: &mut TestAppContext) {
    assert_toolbar_branch_pointer_toggle(cx, false, true);
}

#[gpui_kit::test]
fn toolbar_branch_pointer_controls_toggle_in_light_child_scope(cx: &mut TestAppContext) {
    assert_toolbar_branch_pointer_toggle(cx, true, true);
}

#[gpui_kit::test]
fn reselecting_active_child_replaces_queued_branch_metadata_read(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    cx.update(|cx| {
        fixture.workspace.update(cx, |view, cx| {
            // Queue a real details read but reselect the same child before its
            // completion can publish through the UI executor.
            view.branch_context.as_mut().unwrap().details = None;
            view.refresh_branch_context(cx);
            assert!(view.branch_context_task.is_some());
            let previous_scope_id = view.submodule_request_id;
            view.select_submodule(fixture.child.clone(), cx);
            assert_ne!(view.submodule_request_id, previous_scope_id);
            assert!(view.branch_context_task.is_some());
        });
    });
    cx.run_until_parked();
    cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        assert!(view.branch_context_task.is_none());
        let context = view.branch_context.as_ref().unwrap();
        assert_eq!(context.scope_id, view.submodule_request_id);
        let branches = &view.branch_picker_details().unwrap().branches;
        assert!(branches.iter().any(|branch| branch.name == "child-feature"));
        assert!(
            !branches
                .iter()
                .any(|branch| branch.name == "parent-feature")
        );
        assert!(gitviews_service::paths_equal(
            &view.changes_repository().unwrap().root,
            &fixture.child
        ));
    });
}
