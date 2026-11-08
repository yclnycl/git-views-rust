//! Production Workspace -> native dialog -> background Service -> real local Git.
//! TestPlatform renders simulated windows only; no desktop application is opened.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use gitviews_service::{AppSettings, Head, SettingsStore};
use gpui_kit::{
    App, AppContext, Entity, TestAppContext, Window, WindowHandle, base::Root, px, size,
    test::TestWindowExt,
};

use super::super::{Workspace, WorkspaceOptions};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    base: PathBuf,
    directory: PathBuf,
    root: PathBuf,
    linked: Option<PathBuf>,
    window: WindowHandle<Root>,
    workspace: Entity<Workspace>,
}

impl Fixture {
    fn new(cx: &mut TestAppContext, with_linked: bool) -> Self {
        // The project checkout is itself a Git repository. Keep fixture
        // worktrees outside it so production nested-repository checks apply.
        let base = std::env::temp_dir().join("gitviews-headless-worktree-tests");
        fs::create_dir_all(&base).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = base.join(format!(
            "{}-{stamp}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let root = directory.join("主 仓库");
        git(
            &directory,
            &["init", "--initial-branch=main", root.to_str().unwrap()],
        );
        git(&root, &["config", "core.autocrlf", "false"]);
        fs::write(root.join("note.txt"), "main fixture\n").unwrap();
        fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        git(&root, &["add", "note.txt", ".gitignore"]);
        git(&root, &["commit", "-m", "headless worktree fixture"]);
        git(&root, &["branch", "feature"]);
        let linked = with_linked.then(|| {
            let path = directory.join("关联 中文工作树");
            git(
                &root,
                &["worktree", "add", path.to_str().unwrap(), "feature"],
            );
            path
        });
        let store = SettingsStore::new(directory.join("config.toml"));
        let (window, workspace) =
            crate::ui::headless::open_window(cx, size(px(1680.), px(1100.)), |window, cx| {
                cx.new(|cx| {
                    Workspace::new(
                        WorkspaceOptions {
                            initial_paths: Vec::new(),
                            state_file: directory.join("repositories.json"),
                            preview: true,
                            settings: AppSettings {
                                read_only: false,
                                ..AppSettings::default()
                            },
                            settings_store: store,
                            settings_error: None,
                        },
                        window,
                        cx,
                    )
                })
            });
        // Install fixture data after constructing the real Workspace. Preview
        // startup skips catalog restore and the persistent filesystem watcher.
        cx.update(|cx| {
            workspace.update(cx, |view, cx| {
                let snapshot = view.service.open(&root).unwrap();
                view.details = Some(view.service.details(&root).unwrap());
                view.tree = Some(Arc::new(view.service.tree(&root).unwrap()));
                view.catalog
                    .register(snapshot.root.clone(), snapshot.name.clone())
                    .unwrap();
                view.catalog.set_active(&snapshot.root);
                view.repository = Some(snapshot);
                view.catalog_ready = true;
                view.preview = false;
                view.loading = false;
                view.sync_file_list(cx);
                cx.notify();
            })
        });
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        Self {
            base,
            directory,
            root,
            linked,
            window,
            workspace,
        }
    }

    fn open_manager(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.press("ctrl-alt-w", cx);
            assert!(window.find("worktree-close").visible());
        })
        .unwrap();
        self.settle(cx);
    }

    fn settle(&self, cx: &mut TestAppContext) {
        cx.run_until_parked();
        cx.update_window(self.window.into(), |_, window, cx| {
            window.render_frame(cx);
            let view = self.workspace.read(cx);
            assert!(!view.worktree_busy);
            assert!(view.worktree_task.is_none());
            assert!(view.scan_task.is_none());
            assert!(view.watch_task.is_none());
            assert!(view.watcher.is_none());
        })
        .unwrap();
    }

    fn create_draft(&self, path: &Path, branch: &str, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.click("worktree-mode-create", cx);
            input(window, "worktree-create-path", path.to_str().unwrap(), cx);
            input(window, "worktree-branch", branch, cx);
        })
        .unwrap();
    }

    fn assert_main(&self, cx: &TestAppContext, worktrees: usize) {
        cx.read(|cx| {
            let view = self.workspace.read(cx);
            let repository = view.repository.as_ref().unwrap();
            assert_eq!(repository.root, fs::canonicalize(&self.root).unwrap());
            assert_eq!(repository.head, Head::Branch("main".into()));
            assert_eq!(view.catalog.active.as_ref(), Some(&repository.root));
            assert_eq!(view.details.as_ref().unwrap().worktrees.len(), worktrees);
            assert!(view.submodule_changes.is_none());
        });
        assert_eq!(
            git(&self.root, &["symbolic-ref", "--short", "HEAD"]).trim(),
            "main"
        );
        assert_eq!(
            fs::read_to_string(self.root.join("note.txt")).unwrap(),
            "main fixture\n"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(directory) = fs::canonicalize(&self.directory)
            && directory.parent() == Some(self.base.as_path())
            && directory != self.base
        {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

#[gpui_kit::test]
fn headless_git_settings_load_save_and_modal_isolation(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, false);
    git(
        &fixture.root,
        &[
            "remote",
            "add",
            "origin",
            "git@example.invalid:team/repo.git",
        ],
    );
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.press("ctrl-alt-,", cx);
        assert!(window.find("git-settings-save").visible());
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("git-settings-fetch-url").value(),
            Some("git@example.invalid:team/repo.git")
        );
        let view = fixture.workspace.read(cx);
        assert_eq!(
            view.ssh_discovery_id, 0,
            "opening an SSH repository must not discover keys"
        );
        assert!(view.ssh_discovery_task.is_none());
        let request_id = view.request_id;
        for key in ["ctrl-o", "ctrl-,", "ctrl-alt-w", "ctrl-r", "ctrl-p"] {
            window.press(key, cx);
        }
        assert_eq!(fixture.workspace.read(cx).request_id, request_id);
        assert!(window.try_find("settings-save").is_none());
        assert!(window.try_find("worktree-close").is_none());
        window.click("git-settings-user-name", cx);
        window.press("ctrl-a", cx);
        window.input("新的提交身份", cx);
    })
    .unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("git-settings-save", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("git-settings-save").is_none());
        let view = fixture.workspace.read(cx);
        assert!(!view.git_settings_saving);
        assert!(view.focus.is_focused(window));
        let settings = view.service.git_settings(&fixture.root).unwrap();
        assert_eq!(settings.user_name, "新的提交身份");
        assert_eq!(settings.remotes[0].url, "git@example.invalid:team/repo.git");
    })
    .unwrap();
}

#[gpui_kit::test]
fn headless_git_settings_application_entry_and_cancel_preserve_repository_config(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx, false);
    git(
        &fixture.root,
        &[
            "remote",
            "add",
            "origin",
            "https://example.invalid/team/repo.git",
        ],
    );
    let before = cx.read(|cx| {
        fixture
            .workspace
            .read(cx)
            .service
            .git_settings(&fixture.root)
            .unwrap()
    });
    let app_config = fixture.directory.join("config.toml");
    let app_config_before = fs::read(&app_config).ok();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.press("ctrl-,", cx);
        input(window, "settings-font-size", "18", cx);
    })
    .unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("settings-git", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("settings-save").is_none());
        assert!(fixture.workspace.read(cx).git_settings_return_to_app);
        window.click("git-settings-user-name", cx);
        window.press("ctrl-a", cx);
        window.input("不保存", cx);
    })
    .unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.press("escape", cx);
        assert!(window.try_find("git-settings-save").is_none());
    })
    .unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("settings-save").visible());
        assert_eq!(window.find("settings-font-size").value(), Some("18"));
        assert!(!fixture.workspace.read(cx).git_settings_return_to_app);
        window.click("settings-cancel", cx);
        assert!(window.try_find("settings-save").is_none());
        assert_eq!(
            fixture
                .workspace
                .read(cx)
                .service
                .git_settings(&fixture.root)
                .unwrap(),
            before
        );
    })
    .unwrap();
    assert_eq!(fs::read(app_config).ok(), app_config_before);
}

#[gpui_kit::test]
fn headless_navigation_cancel_discards_late_worktree_results(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, true);
    let linked = fixture.linked.clone().unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        fixture
            .workspace
            .update(cx, |view, cx| view.load(linked, cx));
        window.render_frame(cx);
        assert!(window.find("navigation-progress").visible());
        window.click("cancel-navigation", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        let view = fixture.workspace.read(cx);
        assert!(!view.loading);
        assert!(!view.navigation_loading());
        assert!(view.repository.is_none());
        assert!(view.load_task.is_none());
        assert!(window.try_find("navigation-progress").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn headless_navigation_new_worktree_wins_over_superseded_read(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, true);
    cx.update(|cx| {
        fixture.workspace.update(cx, |view, cx| {
            view.load(fixture.linked.clone().unwrap(), cx);
            view.load(fixture.root.clone(), cx);
        })
    });
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        fixture.workspace.update(cx, |view, cx| {
            assert!(!view.loading);
            assert!(!view.navigation_loading());
            assert_eq!(view.repository.as_ref().unwrap().root, fixture.root);
            assert_eq!(view.requested_path.as_ref(), Some(&fixture.root));
            assert!(view.details.as_ref().unwrap().worktrees.len() >= 2);
            assert!(
                view.tree
                    .as_ref()
                    .unwrap()
                    .entries
                    .iter()
                    .any(|entry| entry.path == "note.txt")
            );
            view.watch_task = None;
            view.watcher = None;
            cx.notify();
        });
    })
    .unwrap();
}

#[gpui_kit::test]
fn headless_navigation_removing_loading_repository_stops_progress_and_late_reads(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx, false);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        fixture.workspace.update(cx, |view, cx| {
            view.repository = None;
            view.load(fixture.root.clone(), cx);
        });
        window.render_frame(cx);
        assert!(window.find("navigation-progress").visible());
        fixture
            .workspace
            .update(cx, |view, cx| view.remove_repository(&fixture.root, cx));
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        let view = fixture.workspace.read(cx);
        assert!(!view.navigation_loading());
        assert!(!view.loading);
        assert!(view.repository.is_none());
        assert!(view.requested_path.is_none());
        assert!(view.catalog.repositories.is_empty());
        assert!(window.try_find("navigation-progress").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn headless_navigation_submodule_stages_keep_the_main_repository(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, false);
    let source = fixture.directory.join("child-source");
    git(
        &fixture.directory,
        &["init", "--initial-branch=main", source.to_str().unwrap()],
    );
    fs::write(source.join("child.txt"), "child fixture\n").unwrap();
    git(&source, &["add", "child.txt"]);
    git(&source, &["commit", "-m", "child fixture"]);
    git(
        &fixture.root,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            source.to_str().unwrap(),
            "child",
        ],
    );
    let child = fs::canonicalize(fixture.root.join("child")).unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        fixture.workspace.update(cx, |view, cx| {
            view.tree = Some(Arc::new(view.service.tree(&fixture.root).unwrap()));
            view.select_submodule(child.clone(), cx);
        });
        window.render_frame(cx);
        assert!(window.find("navigation-progress").visible());
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        let view = fixture.workspace.read(cx);
        assert!(!view.submodule_loading);
        assert!(!view.navigation_loading());
        assert_eq!(view.repository.as_ref().unwrap().root, fixture.root);
        let scope = view.submodule_changes.as_ref().unwrap();
        assert_eq!(scope.repository.root, child);
        assert!(
            scope
                .tree
                .entries
                .iter()
                .any(|entry| entry.path == "child.txt")
        );
        assert!(window.try_find("navigation-progress").is_none());
    })
    .unwrap();
}

fn git(root: &Path, arguments: &[&str]) -> String {
    // Rust canonicalizes Windows paths to \\?\F:\..., while Git for Windows
    // rejects that spelling as a command argument. Keep canonical fixture
    // ownership paths intact and normalize only the child process inputs.
    let arguments = arguments
        .iter()
        .map(|value| cli_path(value))
        .collect::<Vec<_>>();
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("core.hooksPath=")
        .arg("-c")
        .arg("user.name=GitViews Headless")
        .arg("-c")
        .arg("user.email=headless@example.invalid")
        .args(&arguments)
        .current_dir(cli_path(root.to_str().unwrap()))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command
        .output()
        .expect("Git CLI is used only for owned test fixtures");
    assert!(
        output.status.success(),
        "fixture Git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn cli_path(value: &str) -> &str {
    value
        .strip_prefix("\\\\?\\")
        .or_else(|| value.strip_prefix("//?/"))
        .filter(|value| value.as_bytes().get(1) == Some(&b':'))
        .unwrap_or(value)
}

fn input(window: &mut Window, id: &'static str, value: &str, cx: &mut App) {
    window.click(id, cx);
    window.press("ctrl-a", cx);
    window.input(value, cx);
    assert_eq!(window.find(id).value(), Some(value));
}

#[gpui_kit::test]
fn headless_worktree_create_runs_native_service_and_keeps_main_repository_selected(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx, false);
    fixture.open_manager(cx);
    let destination = fixture.directory.join("新建 中文工作树");
    fixture.create_draft(&destination, "topic/界面创建", cx);
    // Input Change events settle before the separate submit frame.
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("worktree-apply", cx);
        window.press("escape", cx);
        window.press("ctrl-,", cx);
        assert!(window.find("worktree-close").visible());
        assert!(window.try_find("settings-save").is_none());
    })
    .unwrap();
    fixture.settle(cx);
    fixture.assert_main(cx, 2);
    assert!(destination.join("note.txt").is_file());
    assert_eq!(
        git(&destination, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "topic/界面创建"
    );
    cx.update_window(fixture.window.into(), |_, window, cx| {
        assert!(window.find(("worktree-select", 1usize)).visible());
        window.press("escape", cx);
        assert!(window.try_find("worktree-close").is_none());
        assert!(fixture.workspace.read(cx).focus.is_focused(window));
        window.press("ctrl-alt-w", cx);
    })
    .unwrap();
    fixture.settle(cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("worktree-close", cx);
        assert!(window.try_find("worktree-close").is_none());
        assert!(fixture.workspace.read(cx).focus.is_focused(window));
    })
    .unwrap();
}

#[gpui_kit::test]
fn headless_worktree_lock_unlock_and_confirmed_removal_mutate_only_the_selected_tree(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx, true);
    let linked = fixture.linked.as_ref().unwrap();
    fixture.open_manager(cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click(("worktree-select", 1usize), cx);
        window.click("worktree-mode-lock", cx);
        input(window, "worktree-lock-reason", "界面锁定测试", cx);
    })
    .unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("worktree-apply", cx)
    })
    .unwrap();
    fixture.settle(cx);
    fixture.assert_main(cx, 2);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        assert!(
            fixture
                .workspace
                .read(cx)
                .details
                .as_ref()
                .unwrap()
                .worktrees[1]
                .locked
        );
        assert_eq!(window.find("worktree-mode-remove").disabled(), Some(true));
        window.click("worktree-mode-unlock", cx);
        window.click("worktree-apply", cx);
    })
    .unwrap();
    fixture.settle(cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        assert!(
            !fixture
                .workspace
                .read(cx)
                .details
                .as_ref()
                .unwrap()
                .worktrees[1]
                .locked
        );
        window.click("worktree-mode-remove", cx);
        window.click("worktree-apply", cx);
        assert!(window.find("worktree-confirmation").visible());
        assert!(linked.exists());
        window.click("worktree-confirm-cancel", cx);
        assert!(window.try_find("worktree-confirmation").is_none());
        assert!(linked.exists());
        window.click("worktree-apply", cx);
        window.click("worktree-confirm-apply", cx);
    })
    .unwrap();
    fixture.settle(cx);
    fixture.assert_main(cx, 1);
    assert!(!linked.exists());
    assert!(
        !git(
            &fixture.root,
            &["show-ref", "--verify", "refs/heads/feature"]
        )
        .is_empty()
    );
    assert!(git(&fixture.root, &["status", "--porcelain"]).is_empty());
}

#[gpui_kit::test]
fn headless_worktree_service_error_keeps_draft_and_corrected_retry_succeeds(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx, false);
    let existing = fixture.directory.join("已存在目录");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join("precious.txt"), "preserve me").unwrap();
    fixture.open_manager(cx);
    fixture.create_draft(&existing, "retry-topic", cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("worktree-apply", cx)
    })
    .unwrap();
    fixture.settle(cx);
    fixture.assert_main(cx, 1);
    let corrected = fixture.directory.join("正确工作树目录");
    cx.update_window(fixture.window.into(), |_, window, cx| {
        assert!(window.find("worktree-error").visible());
        assert_eq!(
            window.find("worktree-create-path").value(),
            existing.to_str()
        );
        assert_eq!(window.find("worktree-branch").value(), Some("retry-topic"));
        assert_ne!(window.find("worktree-apply").disabled(), Some(true));
        input(
            window,
            "worktree-create-path",
            corrected.to_str().unwrap(),
            cx,
        );
    })
    .unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("worktree-apply", cx)
    })
    .unwrap();
    fixture.settle(cx);
    fixture.assert_main(cx, 2);
    assert_eq!(
        fs::read_to_string(existing.join("precious.txt")).unwrap(),
        "preserve me"
    );
    assert_eq!(
        git(&corrected, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "retry-topic"
    );
    cx.update_window(fixture.window.into(), |_, window, _| {
        assert!(window.try_find("worktree-error").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn headless_worktree_busy_modal_blocks_settings_repository_branch_and_workspace_shortcuts(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx, false);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.press("ctrl-alt-w", cx);
        assert!(fixture.workspace.read(cx).worktree_busy);
        let request_id = fixture.workspace.read(cx).request_id;
        for key in [
            "ctrl-o",
            "ctrl-,",
            "ctrl-h",
            "ctrl-p",
            "ctrl-r",
            "ctrl-alt-w",
            "escape",
        ] {
            window.press(key, cx);
        }
        assert_eq!(window.find("worktree-close").disabled(), Some(true));
        window.click("worktree-close", cx);
        assert!(window.find("worktree-close").visible());
        assert!(window.try_find("settings-save").is_none());
        assert!(window.try_find("branch-picker").is_none());
        fixture.workspace.update(cx, |view, cx| {
            // These same entry points also protect callbacks already queued
            // before the modal became visible.
            view.checkout_branch("feature".into(), false, cx);
            view.add_repositories(vec![fixture.directory.join("blocked-import")], cx);
            assert_eq!(view.request_id, request_id);
            assert_eq!(view.catalog.repositories.len(), 1);
            assert_eq!(view.catalog_import_count, 0);
            assert!(view.picker_task.is_none());
            assert!(view.mutation_task.is_none());
            assert!(!view.settings_dialog.read(cx).is_open());
            assert!(!view.history);
            assert!(!view.file_search.open);
            assert!(view.menu.is_none());
        });
    })
    .unwrap();
    fixture.settle(cx);
    fixture.assert_main(cx, 1);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.press("escape", cx);
        assert!(fixture.workspace.read(cx).focus.is_focused(window));
        window.press("ctrl-,", cx);
        assert!(window.find("settings-save").visible());
        window.press("escape", cx);
    })
    .unwrap();
}
