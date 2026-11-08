//! Real menu interactions and isolated repositories catch accidental parent writes.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use gitviews_service::{AppSettings, Head, SettingsStore, paths_equal};
use gpui_kit::{
    AppContext, Entity, TestAppContext, WindowHandle, base::Root, px, size, test::TestWindowExt,
};

use super::super::{Workspace, WorkspaceOptions, submodules::SubmoduleChanges};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    base: PathBuf,
    directory: PathBuf,
    root: PathBuf,
    first: PathBuf,
    second: PathBuf,
    window: WindowHandle<Root>,
    workspace: Entity<Workspace>,
}

impl Fixture {
    fn new(cx: &mut TestAppContext) -> Self {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/headless-module-branch-tests");
        fs::create_dir_all(&base).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let directory = base.join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let root = directory.join("主 仓库");
        git(
            &directory,
            &["init", "--initial-branch=main", root.to_str().unwrap()],
        );
        let first = root.join("modules/甲");
        let second = root.join("modules/乙");
        for (child, content) in [(&first, "first"), (&second, "second")] {
            git(
                &directory,
                &["init", "--initial-branch=main", child.to_str().unwrap()],
            );
            git(child, &["config", "core.autocrlf", "false"]);
            fs::write(child.join("note.txt"), format!("{content} main\n")).unwrap();
            git(child, &["add", "note.txt"]);
            git(child, &["commit", "-m", "main"]);
            git(child, &["switch", "-c", "shared"]);
            fs::write(child.join("note.txt"), format!("{content} feature\n")).unwrap();
            git(child, &["commit", "-am", "feature"]);
            git(
                child,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://example.invalid/submodule",
                ],
            );
            git(
                child,
                &["update-ref", "refs/remotes/origin/remote-shared", "shared"],
            );
            git(child, &["switch", "main"]);
        }
        fs::write(root.join("note.txt"), "parent main\n").unwrap();
        fs::write(root.join(".gitmodules"), "[submodule \"first\"]\npath = modules/甲\nurl = https://example.invalid/first\n[submodule \"second\"]\npath = modules/乙\nurl = https://example.invalid/second\n").unwrap();
        git(&root, &["add", "note.txt", ".gitmodules"]);
        for (path, child) in [("modules/甲", &first), ("modules/乙", &second)] {
            let id = git(child, &["rev-parse", "HEAD"]);
            git(
                &root,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    "160000",
                    id.trim(),
                    path,
                ],
            );
        }
        git(&root, &["commit", "-m", "parent"]);
        git(&root, &["branch", "shared"]);
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
                            settings_store: SettingsStore::new(directory.join("config.toml")),
                            settings_error: None,
                        },
                        window,
                        cx,
                    )
                })
            });
        cx.update(|cx| {
            workspace.update(cx, |view, cx| {
                let repository = view.service.open(&root).unwrap();
                view.details = Some(view.service.details(&root).unwrap());
                view.tree = Some(Arc::new(view.service.tree(&root).unwrap()));
                view.requested_path = Some(repository.root.clone());
                view.repository = Some(repository);
                view.preview = false;
                view.loading = false;
                view.tree_mode = true;
                view.file_list
                    .update(cx, |list, cx| list.set_tree_mode(true, cx));
                view.sync_file_list(cx);
                cx.notify();
            })
        });
        cx.run_until_parked();
        Self {
            base,
            directory,
            root,
            first,
            second,
            window,
            workspace,
        }
    }

    fn open_menu(&self, root: &Path, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            self.workspace.update(cx, |view, cx| {
                view.open_submodule_branch_menu(root.to_path_buf(), window, cx)
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.read(|cx| {
            let view = self.workspace.read(cx);
            let menu = view
                .submodule_branch_menu
                .as_ref()
                .expect("registered child menu");
            assert!(menu.read_task.is_none());
            assert!(menu.error.is_none(), "{:?}", menu.error);
            assert!(paths_equal(&menu.repository.as_ref().unwrap().root, root));
        });
    }

    fn select_other_child(&self, cx: &mut TestAppContext) {
        cx.update(|cx| {
            self.workspace.update(cx, |view, cx| {
                let repository = view.service.open(&self.second).unwrap();
                view.submodule_changes = Some(SubmoduleChanges {
                    parent_root: view.repository.as_ref().unwrap().root.clone(),
                    target: repository.root.clone(),
                    tree: Arc::new(view.service.tree(&self.second).unwrap()),
                    repository,
                    watcher: None,
                    watcher_needs_restart: false,
                });
                view.sync_file_list(cx);
                cx.notify();
            })
        });
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.directory.parent() == Some(&self.base) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.hooksPath="])
        .arg("-C")
        .arg(gitviews_service::display_path(root))
        .args(args.iter().map(|arg| {
            if arg.starts_with(r"\\?\") {
                gitviews_service::display_path(Path::new(arg))
            } else {
                (*arg).to_owned()
            }
        }))
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "GitViews fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "GitViews fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[gpui_kit::test]
fn module_row_dropdown_switches_exact_child_and_preserves_other_scope(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.select_other_child(cx);
    let button_id: gpui_kit::SharedString = cx
        .read(|cx| {
            let mut actions = std::collections::HashMap::new();
            fixture.workspace.read(cx).make_tree(false, &mut actions);
            actions
                .into_iter()
                .find_map(|(id, action)| {
                    matches!(action, super::super::WorkbenchAction::SubmoduleBranches(root)
                if paths_equal(&root, &fixture.first))
                    .then_some(id)
                })
                .expect("the sidebar must render the first module's branch control")
        })
        .into();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(button_id.clone(), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("submodule-branch-picker").visible());
        window.click("submodule-branch-search", cx);
        window.input("shared", cx);
        window.press("enter", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        git(&fixture.first, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "shared"
    );
    assert_eq!(
        git(&fixture.second, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
    assert_eq!(
        git(&fixture.root, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
    assert_eq!(
        fs::read_to_string(fixture.first.join("note.txt")).unwrap(),
        "first feature\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
        "parent main\n"
    );
    cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        assert!(view.mutation_task.is_none(), "{:?}", view.notice);
        assert!(paths_equal(
            &view.changes_repository().unwrap().root,
            &fixture.second
        ));
        assert_eq!(
            view.repository.as_ref().unwrap().head,
            Head::Branch("main".into())
        );
    });
}

#[gpui_kit::test]
fn module_remote_dropdown_creates_child_tracking_branch_without_fetching(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.open_menu(&fixture.first, cx);
    let row = cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        super::branches(view.submodule_branch_menu.as_ref().unwrap())
            .iter()
            .position(|branch| branch.name == "origin/remote-shared" && branch.is_remote)
            .unwrap()
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(("submodule-branch-row", row), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        git(&fixture.first, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "remote-shared"
    );
    assert_eq!(
        git(&fixture.first, &["config", "branch.remote-shared.remote"]).trim(),
        "origin"
    );
    assert_eq!(
        git(&fixture.first, &["config", "branch.remote-shared.merge"]).trim(),
        "refs/heads/remote-shared"
    );
    assert_eq!(
        git(&fixture.root, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
    cx.read(|cx| assert!(fixture.workspace.read(cx).submodule_changes.is_none()));
}

#[gpui_kit::test]
fn closing_or_replacing_module_menu_discards_late_results_and_stale_actions(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    let first_id = cx
        .update_window(fixture.window.into(), |_, window, cx| {
            fixture.workspace.update(cx, |view, cx| {
                view.open_submodule_branch_menu(fixture.first.clone(), window, cx);
                let previous = view.submodule_branch_menu.as_ref().unwrap();
                let first_id = previous.id;
                let cancelled = Arc::clone(&previous.cancel);
                view.close_submodule_branch_menu(cx);
                assert!(cancelled.load(Ordering::Acquire));
                view.open_submodule_branch_menu(fixture.second.clone(), window, cx);
                first_id
            })
        })
        .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        fixture.workspace.update(cx, |view, cx| {
            let current = view.submodule_branch_menu.as_ref().unwrap();
            assert_ne!(current.id, first_id);
            assert!(paths_equal(
                &current.repository.as_ref().unwrap().root,
                &fixture.second
            ));
            view.checkout_submodule_menu_branch(first_id, "shared".into(), false, cx);
            assert!(view.mutation_task.is_none());
            assert!(view.submodule_branch_menu.is_some());
            view.close_submodule_branch_menu(cx);
        })
    });
    cx.run_until_parked();
    cx.read(|cx| assert!(fixture.workspace.read(cx).submodule_branch_menu.is_none()));
    assert_eq!(
        git(&fixture.first, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
    assert_eq!(
        git(&fixture.second, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
}

#[gpui_kit::test]
fn module_deinitialized_after_menu_read_never_falls_back_to_parent(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.open_menu(&fixture.first, cx);
    fs::rename(
        fixture.first.join(".git"),
        fixture.first.join(".git-hidden"),
    )
    .unwrap();
    cx.update(|cx| {
        fixture.workspace.update(cx, |view, cx| {
            let id = view.submodule_branch_menu.as_ref().unwrap().id;
            view.checkout_submodule_menu_branch(id, "shared".into(), false, cx);
        })
    });
    cx.run_until_parked();
    assert_eq!(
        git(&fixture.root, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
        "parent main\n"
    );
    cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        assert!(view.mutation_task.is_none());
        assert!(
            view.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("身份验证失败")),
            "{:?}",
            view.notice
        );
    });
}

#[gpui_kit::test]
fn opening_another_dialog_cancels_module_menu_instead_of_hiding_it(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.open_menu(&fixture.first, cx);
    let cancelled = cx.read(|cx| {
        Arc::clone(
            &fixture
                .workspace
                .read(cx)
                .submodule_branch_menu
                .as_ref()
                .unwrap()
                .cancel,
        )
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        fixture
            .workspace
            .update(cx, |view, cx| view.open_branch_manager(window, cx));
        window.render_frame(cx);
        let view = fixture.workspace.read(cx);
        assert!(view.branch_dialog.read(cx).is_open());
        assert!(view.submodule_branch_menu.is_none());
        assert!(cancelled.load(Ordering::Acquire));
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        fixture.workspace.update(cx, |view, cx| {
            view.branch_dialog.update(cx, |dialog, cx| dialog.clear(cx));
            cx.notify();
        });
        window.render_frame(cx);
        assert!(fixture.workspace.read(cx).submodule_branch_menu.is_none());
    })
    .unwrap();
    assert_eq!(
        git(&fixture.root, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
    assert_eq!(
        git(&fixture.first, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
}

#[gpui_kit::test]
fn file_tree_branch_button_opens_menu_without_previewing_or_entering_gitlink(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    cx.update(|cx| {
        fixture.workspace.update(cx, |view, cx| {
            view.select_file("note.txt".into(), false, cx);
            // A path filter gives the target module a stable row after its
            // ancestor directory while retaining a different selected preview.
            view.file_query = "modules/甲".into();
            view.file_list
                .update(cx, |list, cx| list.set_query("modules/甲".into(), cx));
            cx.notify();
        });
    });
    cx.run_until_parked();
    let previous = cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        assert_eq!(view.selected_file.as_deref(), Some("note.txt"));
        (
            view.submodule_request_id,
            view.diff_request_id,
            view.opened_files.len(),
            view.active_preview_key(),
        )
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        let button = window.find(("file-module-branch", 1usize));
        assert!(
            button.visible(),
            "the file-tree branch control must be reachable"
        );
        window.click(("file-module-branch", 1usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        let view = fixture.workspace.read(cx);
        assert!(view.submodule_branch_menu.is_some(), "{:?}", view.notice);
        let menu = view.submodule_branch_menu.as_ref().unwrap();
        assert!(menu.error.is_none(), "{:?}", menu.error);
        assert!(paths_equal(
            &menu.repository.as_ref().unwrap().root,
            &fixture.first
        ));
        assert_eq!(view.selected_file.as_deref(), Some("note.txt"));
        assert_eq!(view.opened_files.len(), previous.2);
        assert_eq!(view.active_preview_key(), previous.3);
        assert!(view.submodule_changes.is_none());
        assert!(!view.submodule_loading);
        assert_eq!(view.submodule_request_id, previous.0);
        assert_eq!(view.diff_request_id, previous.1);
        assert!(window.find("submodule-branch-picker").visible());
        window.click("submodule-branch-close", cx);
    })
    .unwrap();
    assert_eq!(
        git(&fixture.root, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
    assert_eq!(
        git(&fixture.first, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
}
