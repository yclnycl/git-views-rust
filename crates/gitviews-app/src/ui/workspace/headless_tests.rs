//! Exercises production Workspace actions, real GPUI input and temporary TOML files.

use super::{Workspace, WorkspaceOptions};
use crate::ui::headless;
use gitviews_service::{AppSettings, SettingsStore, ThemePreference};
use gpui_kit::{
    AppContext, Entity, TestAppContext, WindowHandle,
    base::Root,
    component::{ActiveTheme, ThemeMode},
    point, px, size,
    test::{TestQueryExt as _, TestWindowExt},
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

fn replace_input(
    window: &mut gpui_kit::Window,
    id: &'static str,
    value: &str,
    cx: &mut gpui_kit::App,
) {
    window.click(id, cx);
    window.press("ctrl-a", cx);
    window.input(value, cx);
    assert_eq!(window.find(id).value(), Some(value));
}

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    directory: PathBuf,
    window: WindowHandle<Root>,
    workspace: Entity<Workspace>,
    store: SettingsStore,
}

impl Fixture {
    fn new(cx: &mut TestAppContext) -> Self {
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/headless-tests")
            .join(format!(
                "workspace-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&directory).unwrap();
        let store = SettingsStore::new(directory.join("中文 配置/config.toml"));
        let settings = store.load_or_create().unwrap();
        let (window, workspace) =
            headless::open_window(cx, size(px(1680.), px(1000.)), |window, cx| {
                cx.new(|cx| {
                    Workspace::new(
                        WorkspaceOptions {
                            initial_paths: Vec::new(),
                            state_file: directory.join("repositories.json"),
                            preview: true,
                            settings,
                            settings_store: store.clone(),
                            settings_error: None,
                        },
                        window,
                        cx,
                    )
                })
            });
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        Self {
            directory,
            window,
            workspace,
            store,
        }
    }

    fn open_settings(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.press("ctrl-,", cx);
            assert!(window.find("settings-save").visible());
        })
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let expected =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/headless-tests");
        if self.directory.parent() == Some(expected.as_path()) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

#[gpui_kit::test]
fn search_toolbar_has_one_prompt_and_one_shortcut_in_both_themes(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.workspace.read_with(cx, |workspace, _| {
        for light in [true, false] {
            let mut tree = workspace.make_tree(light, &mut std::collections::HashMap::new());
            let search = super::presentation::find_name_mut(&mut tree, "搜索命令或文件").unwrap();
            let labels = search
                .children
                .iter()
                .filter_map(|child| child.content.as_deref())
                .collect::<Vec<_>>();
            assert_eq!(
                labels
                    .iter()
                    .filter(|label| **label == "搜索文件或内容…")
                    .count(),
                1
            );
            assert_eq!(labels.iter().filter(|label| **label == "Ctrl P").count(), 1);
            assert_eq!(
                labels.len(),
                2,
                "the icon must not receive another text prompt"
            );
        }
    });
}

#[gpui_kit::test]
fn headless_settings_save_persists_and_applies_to_the_workspace(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.open_settings(cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click(("settings-theme", 1_usize), cx);
        window.click("settings-font-size", cx);
        window.press("ctrl-a", cx);
        window.input("14.5", cx);
        assert_eq!(window.find("settings-font-size").value(), Some("14.5"));
        window.click("settings-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("settings-save").is_none());
        let workspace = fixture.workspace.read(cx);
        assert!(!workspace.settings_saving);
        assert_eq!(workspace.settings.appearance.theme, ThemePreference::Light);
        assert_eq!(workspace.settings.appearance.editor_font_size, 14.5);
        assert_eq!(cx.theme().mode, ThemeMode::Light);
        assert_eq!(cx.global::<crate::ui::PreviewPreferences>().font_size, 14.5);
        assert!(workspace.focus.is_focused(window));
    })
    .unwrap();
    let saved = fixture.store.load().unwrap();
    assert_eq!(saved.appearance.theme, ThemePreference::Light);
    assert_eq!(saved.appearance.editor_font_size, 14.5);
    assert!(!fixture.directory.join("repositories.json").exists());
    assert_ne!(saved, AppSettings::default());
}

#[gpui_kit::test]
fn invalid_settings_stay_visible_without_writing_or_applying(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    let previous = fs::read(fixture.store.path()).unwrap();
    fixture.open_settings(cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        replace_input(window, "settings-font-size", "25", cx);
    })
    .unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("settings-save", cx);
        let error = window.find("settings-error-scroll");
        assert!(error.visible());
        assert!(error.label().unwrap().contains("editor_font_size"));
        assert!(window.find("settings-save").visible());
        assert_eq!(fixture.workspace.read(cx).settings, AppSettings::default());
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("settings-error-scroll").visible());
    })
    .unwrap();
    assert_eq!(fs::read(fixture.store.path()).unwrap(), previous);
}

#[gpui_kit::test]
fn cancel_discards_the_draft_and_modal_shortcuts_do_not_reach_the_workspace(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    let previous = fs::read(fixture.store.path()).unwrap();
    fixture.open_settings(cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        replace_input(window, "settings-font-size", "18", cx);
        window.click(("settings-theme", 1_usize), cx);
        for key in ["ctrl-h", "ctrl-p", "ctrl-w", "ctrl-r"] {
            window.press(key, cx);
        }
        let workspace = fixture.workspace.read(cx);
        assert!(!workspace.history);
        assert!(!workspace.file_search.open);
        assert_eq!(workspace.settings, AppSettings::default());
        window.press("escape", cx);
        assert!(window.try_find("settings-save").is_none());
        assert!(fixture.workspace.read(cx).focus.is_focused(window));
        window.click("settings-toolbar", cx);
        assert_eq!(window.find("settings-font-size").value(), Some("12.5"));
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(fs::read(fixture.store.path()).unwrap(), previous);
}

#[gpui_kit::test]
fn reload_reads_external_toml_and_immediately_updates_the_ui(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.open_settings(cx);
    let mut external = AppSettings::default();
    external.appearance.theme = ThemePreference::Light;
    external.appearance.editor_font_size = 17.5;
    external.refresh.scan_interval_secs = 30;
    fixture.store.save(&external).unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        replace_input(window, "settings-font-size", "20", cx);
        window.click("settings-reload", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("settings-font-size").value(), Some("17.5"));
        assert_eq!(window.find("settings-scan-interval").value(), Some("30"));
        assert_eq!(fixture.workspace.read(cx).settings, external);
        assert_eq!(cx.theme().mode, ThemeMode::Light);
        assert!(window.try_find("settings-error-scroll").is_none());
        window.press("escape", cx);
        assert!(window.try_find("settings-save").is_none());
    })
    .unwrap();
    assert_eq!(fixture.store.load().unwrap(), external);
}

#[gpui_kit::test]
fn malformed_external_toml_is_preserved_until_repaired_and_reloaded(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.open_settings(cx);
    let malformed = b"[appearance\ntheme='light'";
    fs::write(fixture.store.path(), malformed).unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("settings-reload", cx)
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window
                .find("settings-error-scroll")
                .label()
                .unwrap()
                .contains("TOML")
        );
        assert_eq!(fixture.workspace.read(cx).settings, AppSettings::default());
        replace_input(window, "settings-font-size", "16", cx);
        window.click("settings-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("settings-error-scroll").visible());
        assert_eq!(window.find("settings-font-size").value(), Some("16"));
        assert_ne!(window.find("settings-save").disabled(), Some(true));
        assert_eq!(fixture.workspace.read(cx).settings, AppSettings::default());
    })
    .unwrap();
    assert_eq!(fs::read(fixture.store.path()).unwrap(), malformed);
    let mut repaired = AppSettings::default();
    repaired.appearance.editor_font_size = 15.;
    fs::write(fixture.store.path(), repaired.to_toml().unwrap()).unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("settings-reload", cx)
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("settings-error-scroll").is_none());
        assert_eq!(window.find("settings-font-size").value(), Some("15"));
        assert_eq!(fixture.workspace.read(cx).settings, repaired);
    })
    .unwrap();
}

#[gpui_kit::test(seeds(0, 1, 17))]
fn cancelling_reload_cannot_reopen_the_dialog_or_create_missing_configuration(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    fixture.open_settings(cx);
    fs::remove_file(fixture.store.path()).unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("settings-reload", cx);
        window.click("settings-cancel", cx);
        assert!(window.try_find("settings-save").is_none());
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("settings-save").is_none());
        assert!(fixture.workspace.read(cx).focus.is_focused(window));
        assert_eq!(fixture.workspace.read(cx).settings, AppSettings::default());
    })
    .unwrap();
    assert!(!fixture.store.path().exists());
}

#[gpui_kit::test]
fn both_column_dividers_resize_the_workspace_and_survive_a_rerender(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        let before = (0_usize..3)
            .map(|index| window.find(("workbench-column", index)).bounds())
            .collect::<Vec<_>>();
        let first_divider = point(before[0].right(), before[0].center().y);
        window.drag(first_divider, first_divider + point(px(50.), px(0.)), cx);
        let first = window.find(("workbench-column", 0_usize)).bounds();
        assert!(first.size.width > before[0].size.width + px(40.));
        let second = window.find(("workbench-column", 1_usize)).bounds();
        let second_divider = point(second.right(), second.center().y);
        window.drag(second_divider, second_divider + point(px(60.), px(0.)), cx);
        let resized = (0_usize..3)
            .map(|index| window.find(("workbench-column", index)).bounds().size.width)
            .collect::<Vec<_>>();
        assert!(resized[1] > second.size.width + px(50.));
        assert!(resized[2] >= px(600.));
        window.click("settings-toolbar", cx);
        window.press("escape", cx);
        for (index, width) in resized.into_iter().enumerate() {
            assert!(
                (window.find(("workbench-column", index)).bounds().size.width - width).abs()
                    < px(1.)
            );
        }
    })
    .unwrap();
}

fn install_copy_sidebar(fixture: &Fixture, cx: &mut TestAppContext) -> PathBuf {
    let root = fixture.directory.join("main");
    let worktree = fixture.directory.join("另一 工作树");
    cx.update_window(fixture.window.into(), |_, window, cx| {
        fixture.workspace.update(cx, |view, cx| {
            view.preview = false;
            view.repository = Some(gitviews_service::RepositorySnapshot {
                root: root.clone(),
                name: "主仓库".into(),
                head: gitviews_service::Head::Branch("main".into()),
                changes: Vec::new(),
                recent_commits: Vec::new(),
            });
            view.details = Some(gitviews_service::RepositoryDetails {
                worktrees: vec![
                    gitviews_service::WorktreeInfo {
                        path: root.clone(),
                        branch: Some("main".into()),
                        is_main: true,
                        ..Default::default()
                    },
                    gitviews_service::WorktreeInfo {
                        path: worktree.clone(),
                        branch: Some("feature/中文".into()),
                        ..Default::default()
                    },
                ],
                branches: vec![gitviews_service::BranchInfo {
                    name: "feature/中文".into(),
                    commit_id: "1234567890abcdef".into(),
                    subject: String::new(),
                    is_current: false,
                    upstream: None,
                    is_remote: false,
                }],
            });
            cx.notify();
        });
        window.render_frame(cx);
    })
    .unwrap();
    worktree
}

#[gpui_kit::test]
fn sidebar_right_click_copies_branch_name_without_checkout(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    // The branch sidebar belongs to advanced mode; all other settings tests keep the default.
    cx.update_window(fixture.window.into(), |_, window, cx| {
        fixture.workspace.update(cx, |view, cx| {
            let mut settings = view.settings;
            settings.read_only = false;
            view.apply_settings(settings, window, cx);
        });
    })
    .unwrap();
    install_copy_sidebar(&fixture, cx);
    let (request_id, head) = cx
        .update_window(fixture.window.into(), |_, window, cx| {
            let request_id = fixture.workspace.read(cx).request_id;
            let head = fixture
                .workspace
                .read(cx)
                .repository
                .as_ref()
                .unwrap()
                .head
                .clone();
            let row = window
                .find_by_label("实际本地分支")
                .path()
                .last()
                .unwrap()
                .clone();
            window.right_click(row, cx);
            (request_id, head)
        })
        .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("popup-menu").visible());
        window.press("down", cx);
        window.press("enter", cx);
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("feature/中文".into())
        );
        let view = fixture.workspace.read(cx);
        assert_eq!(view.request_id, request_id);
        assert_eq!(view.repository.as_ref().unwrap().head, head);
        assert!(view.mutation_task.is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn sidebar_right_click_copies_worktree_path_without_switching(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    let worktree = install_copy_sidebar(&fixture, cx);
    let (request_id, selected_root) = cx
        .update_window(fixture.window.into(), |_, window, cx| {
            let request_id = fixture.workspace.read(cx).request_id;
            let selected_root = fixture
                .workspace
                .read(cx)
                .repository
                .as_ref()
                .unwrap()
                .root
                .clone();
            let row = window.find_all_by_label("实际工作树")[1]
                .path()
                .last()
                .unwrap()
                .clone();
            window.right_click(row, cx);
            (request_id, selected_root)
        })
        .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("popup-menu").visible());
        window.press("down", cx);
        window.press("down", cx);
        window.press("enter", cx);
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some(crate::ui::paths::display_path(&worktree))
        );
        let view = fixture.workspace.read(cx);
        assert_eq!(view.request_id, request_id);
        assert_eq!(view.repository.as_ref().unwrap().root, selected_root);
        assert!(!view.loading);
        assert!(view.load_task.is_none());
    })
    .unwrap();
}
