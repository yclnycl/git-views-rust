//! Exercise the default mode through production startup, UI actions and real repositories.
use super::super::*;
use crate::ui::headless;
use core::prelude::v1::test;
use gpui_kit::{base::Root, test::TestWindowExt as _};
use std::{
    fs,
    path::Path,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    directory: PathBuf,
    root: PathBuf,
    window: WindowHandle<Root>,
    workspace: Entity<Workspace>,
    store: SettingsStore,
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

impl Fixture {
    fn new(cx: &mut TestAppContext) -> Self {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/headless-read-only-tests");
        fs::create_dir_all(&base).unwrap();
        let directory = fs::canonicalize(base).unwrap().join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let root = directory.join("AI 代码");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--initial-branch=main"]);
        git(&root, &["config", "user.name", "Readonly Test"]);
        git(&root, &["config", "user.email", "readonly@example.invalid"]);
        fs::write(root.join("note.txt"), "before\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "base"]);
        git(&root, &["branch", "other"]);
        fs::write(root.join("note.txt"), "AI change\n").unwrap();
        let original_index = fs::read(root.join(".git/index")).unwrap();
        let store = SettingsStore::new(directory.join("config.toml"));
        let (window, workspace) =
            headless::open_window(cx, size(px(1680.), px(1100.)), |window, cx| {
                cx.new(|cx| {
                    Workspace::new(
                        WorkspaceOptions {
                            initial_paths: vec![root.clone()],
                            state_file: directory.join("repositories.json"),
                            preview: false,
                            settings: AppSettings::default(),
                            settings_store: store.clone(),
                            settings_error: None,
                        },
                        window,
                        cx,
                    )
                })
            });
        let fixture = Self {
            directory,
            root,
            window,
            workspace,
            store,
        };
        fixture.settle(cx);
        assert_eq!(
            fs::read(fixture.root.join(".git/index")).unwrap(),
            original_index
        );
        fixture
    }

    fn settle(&self, cx: &mut TestAppContext) {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        cx.update_window(self.window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    fn toggle(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.click("read-only-mode-toolbar", cx)
        })
        .unwrap();
        self.settle(cx);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/headless-read-only-tests");
        if fs::canonicalize(base).ok().as_deref() == self.directory.parent() {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

#[gpui_kit::test]
fn default_startup_opens_repository_without_git_setup_and_keeps_reading_tools(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    fixture.workspace.read_with(cx, |view, cx| {
        assert!(view.settings.read_only);
        assert!(view.git_engine_ready && view.catalog_ready);
        assert!(!view.modal_open(cx));
        assert!(paths_equal(
            &view.repository.as_ref().unwrap().root,
            &fixture.root
        ));
        for light in [true, false] {
            let mut actions = HashMap::new();
            let mut tree = view.make_tree(light, &mut actions);
            for name in [
                "提交变更区域",
                "获取远端",
                "拉取远端",
                "推送提交",
                "管理工作树",
                "新增工作树",
                "本地分支分区",
                "远程分支分区",
                "储藏入口分区",
                "分支切换",
            ] {
                assert!(
                    presentation::find_name_mut(&mut tree, name).is_none(),
                    "{name}"
                );
            }
            for name in [
                "切换只读模式",
                "查看提交历史",
                "搜索命令或文件",
                "刷新文件变更",
            ] {
                assert!(
                    presentation::find_name_mut(&mut tree, name).is_some(),
                    "{name}"
                );
            }
        }
    });
    assert!(!fixture.directory.join("git-engine.toml").exists());
    cx.update_window(fixture.window.into(), |_, window, cx| {
        assert!(window.try_find("native-commit-form").is_none());
        assert!(window.try_find(("file-stage", 1usize)).is_none());
        fixture.workspace.update(cx, |view, cx| {
            view.select_file("note.txt".into(), false, cx)
        });
    })
    .unwrap();
    fixture.settle(cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        assert!(window.try_find("stage-selected-hunk").is_none());
        fixture.workspace.update(cx, |view, cx| {
            assert!(view.diff.is_some());
            view.open_file_history(fixture.root.clone(), "note.txt".into(), window, cx);
        });
        window.render_frame(cx);
        assert!(window.find("history-tools-mode-file").visible());
        assert!(window.try_find("history-tools-mode-recovery").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn read_only_blocks_shortcuts_and_direct_mutations_without_touching_git(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    let head = git(&fixture.root, &["rev-parse", "HEAD"]);
    let index = fs::read(fixture.root.join(".git/index")).unwrap();
    let config = fs::read(fixture.root.join(".git/config")).unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        for key in [
            "ctrl-enter",
            "ctrl-alt-b",
            "ctrl-alt-s",
            "ctrl-alt-m",
            "ctrl-alt-w",
            "ctrl-alt-,",
            "ctrl-n",
            "ctrl-shift-o",
        ] {
            window.press(key, cx);
        }
        fixture.workspace.update(cx, |view, cx| {
            view.run_write(WriteAction::StageAll, window, cx);
            view.checkout_branch("other".into(), false, cx);
            view.start_remote(RemoteAction::Fetch, cx);
            view.open_submodule_branch_menu(fixture.root.clone(), window, cx);
            view.run_local_operation(
                writes::LocalOperation::History(HistoryToolsRequest::Restore {
                    name: "forbidden".into(),
                    commit_id: head.trim().into(),
                    expected_head: Some(head.trim().into()),
                }),
                window,
                cx,
            );
            assert!(view.mutation_task.is_none());
            assert!(view.remote_job.is_none());
            assert!(!view.modal_open(cx));
            assert!(view.notice.as_ref().unwrap().contains("只读"));
        });
    })
    .unwrap();
    fixture.settle(cx);
    assert_eq!(git(&fixture.root, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(&fixture.root, &["branch", "--show-current"]).trim(),
        "main"
    );
    assert_eq!(fs::read(fixture.root.join(".git/index")).unwrap(), index);
    assert_eq!(fs::read(fixture.root.join(".git/config")).unwrap(), config);
    assert_eq!(
        fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
        "AI change\n"
    );
    assert!(git(&fixture.root, &["branch", "--list", "forbidden"]).is_empty());
}

#[gpui_kit::test]
fn mode_toggle_persists_restores_advanced_tools_and_waits_for_operations(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.toggle(cx);
    assert!(!fixture.store.load().unwrap().read_only);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        assert!(window.find("git-engine-save").visible());
        window.click("git-engine-builtin", cx);
        window.click("git-engine-save", cx);
    })
    .unwrap();
    fixture.settle(cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        assert!(window.find("native-commit-form").visible());
        fixture
            .workspace
            .update(cx, |view, _| view.worktree_busy = true);
        window.click("read-only-mode-toolbar", cx);
    })
    .unwrap();
    fixture.settle(cx);
    assert!(!fixture.store.load().unwrap().read_only);
    cx.update(|cx| {
        fixture
            .workspace
            .update(cx, |view, _| view.worktree_busy = false)
    });
    fixture.toggle(cx);
    assert!(fixture.store.load().unwrap().read_only);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        assert!(window.try_find("native-commit-form").is_none());
        window.press("ctrl-,", cx);
        window.click("settings-read-only", cx);
        window.click("settings-cancel", cx);
    })
    .unwrap();
    assert!(fixture.store.load().unwrap().read_only);
    fixture.toggle(cx);
    cx.update_window(fixture.window.into(), |_, window, _cx| {
        assert!(window.try_find("git-engine-save").is_none());
        assert!(window.find("native-commit-form").visible());
    })
    .unwrap();
    fixture.workspace.read_with(cx, |view, _| {
        assert!(paths_equal(
            &view.repository.as_ref().unwrap().root,
            &fixture.root
        ));
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.press("ctrl-,", cx);
        window.click("settings-read-only", cx);
        window.click("settings-save", cx);
    })
    .unwrap();
    fixture.settle(cx);
    assert!(fixture.store.load().unwrap().read_only);
}

#[gpui_kit::test]
fn failed_mode_save_keeps_read_only_enabled(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fs::write(fixture.store.path(), "invalid = [").unwrap();
    fixture.toggle(cx);
    fixture.workspace.read_with(cx, |view, cx| {
        assert!(view.settings.read_only);
        assert!(!view.settings_saving);
        assert!(view.settings_error.is_some());
        assert!(!view.modal_open(cx));
    });
    assert_eq!(
        fs::read_to_string(fixture.store.path()).unwrap(),
        "invalid = ["
    );
}
