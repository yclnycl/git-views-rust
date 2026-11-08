//! File-history entry points use the clicked row and retain its repository scope.
use super::super::{Workspace, WorkspaceOptions, submodules::SubmoduleChanges};
use gitviews_service::{AppSettings, SettingsStore, display_path, paths_equal};
use gpui_kit::{
    App, AppContext as _, Entity, InputEvent as _, Modifiers, MouseButton, MouseDownEvent,
    MouseUpEvent, TestAppContext, Window, WindowHandle,
    base::Root,
    point, px, size,
    test::{TestEventExt as _, TestQueryExt as _, TestWindowExt as _},
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    base: PathBuf,
    directory: PathBuf,
    root: PathBuf,
    child: PathBuf,
    window: WindowHandle<Root>,
    workspace: Entity<Workspace>,
}

impl Fixture {
    fn new(cx: &mut TestAppContext, tree_mode: bool) -> Self {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/headless-file-history-tests");
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
            &["init", "--initial-branch=main", &display_path(&root)],
        );
        let child = root.join("modules/子仓库");
        git(
            &directory,
            &["init", "--initial-branch=main", &display_path(&child)],
        );
        for (repo, scope) in [(&root, "parent"), (&child, "child")] {
            git(repo, &["config", "core.autocrlf", "false"]);
            for (path, message) in [
                ("a.txt", "a created"),
                ("b.txt", "b created"),
                ("deleted.txt", "deleted created"),
                ("b.txt", "b updated"),
                ("leading.txt", "plain name created"),
                (" leading.txt", "leading space name created"),
            ] {
                fs::write(repo.join(path), format!("{scope} {message}\n")).unwrap();
                git(repo, &["add", path]);
                git(repo, &["commit", "-m", &format!("{scope} {message}")]);
            }
        }
        fs::write(
            root.join(".gitmodules"),
            "[submodule \"child\"]\npath = modules/子仓库\nurl = https://example.invalid/unused\n",
        )
        .unwrap();
        git(&root, &["add", ".gitmodules"]);
        let child_head = git(&child, &["rev-parse", "HEAD"]);
        git(
            &root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                child_head.trim(),
                "modules/子仓库",
            ],
        );
        git(&root, &["commit", "-m", "register child"]);
        // Keep both unstaged and staged data. Reading history must not refresh
        // the real index, consume the staged change or touch either working tree.
        for repo in [&root, &child] {
            fs::write(repo.join("a.txt"), "unstaged A\n").unwrap();
            fs::write(repo.join("b.txt"), "staged B\n").unwrap();
            git(repo, &["add", "b.txt"]);
            fs::remove_file(repo.join("deleted.txt")).unwrap();
            fs::write(repo.join("新 文件.txt"), "untracked\n").unwrap();
        }
        let (window, workspace) =
            crate::ui::headless::open_window(cx, size(px(1680.), px(1100.)), |window, cx| {
                cx.new(|cx| {
                    Workspace::new(
                        WorkspaceOptions {
                            initial_paths: Vec::new(),
                            state_file: directory.join("repositories.json"),
                            preview: true,
                            settings: AppSettings::default(),
                            settings_store: SettingsStore::new(directory.join("config.toml")),
                            settings_error: None,
                        },
                        window,
                        cx,
                    )
                })
            });
        let fixture = Self {
            base,
            directory,
            root,
            child,
            window,
            workspace,
        };
        cx.update_window(window.into(), |_, window, cx| {
            fixture.workspace.update(cx, |view, cx| {
                let parent = view.service.open(&fixture.root).unwrap();
                view.details = Some(view.service.details(&fixture.root).unwrap());
                view.tree = Some(Arc::new(view.service.tree(&fixture.root).unwrap()));
                view.requested_path = Some(parent.root.clone());
                view.repository = Some(parent);
                view.preview = false;
                view.loading = false;
                view.tree_mode = tree_mode;
                view.file_list
                    .update(cx, |list, cx| list.set_tree_mode(tree_mode, cx));
                view.sync_file_list(cx);
                view.focus.focus(window, cx);
                cx.notify();
            });
        })
        .unwrap();
        fixture.settle(cx);
        fixture
    }

    fn settle(&self, cx: &mut TestAppContext) {
        cx.run_until_parked();
        cx.update_window(self.window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    fn select_a(&self, cx: &mut TestAppContext) {
        self.click_label("文件 a.txt", cx);
        cx.read(|cx| {
            assert_eq!(
                self.workspace.read(cx).selected_file.as_deref(),
                Some("a.txt")
            );
        });
    }

    fn click_label(&self, label: &str, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.render_frame(cx);
            let bounds = window.find_by_label(label).bounds();
            assert!(window.find_by_label(label).visible(), "{label} visible");
            pointer(window, bounds.center(), MouseButton::Left, cx);
        })
        .unwrap();
        self.settle(cx);
    }

    fn open_menu(&self, path: &str, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.render_frame(cx);
            let bounds = window.find_by_label(&format!("文件 {path}")).bounds();
            pointer(window, bounds.center(), MouseButton::Right, cx);
        })
        .unwrap();
        self.settle(cx);
        cx.update_window(self.window.into(), |_, window, _| {
            assert!(window.find("popup-menu").visible());
            assert!(window.find_by_label("查看文件历史").visible());
        })
        .unwrap();
    }

    fn open_history(&self, path: &str, cx: &mut TestAppContext) {
        self.open_menu(path, cx);
        self.click_label("查看文件历史", cx);
    }

    fn assert_history(&self, root: &Path, path: &str, cx: &mut TestAppContext) {
        let expected = git(root, &["log", "--format=%H", "--", path])
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        cx.update_window(self.window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find("history-tools-root").label(),
                Some(display_path(root).as_str())
            );
            assert_eq!(window.find("history-tools-path").value(), Some(path));
            let mut actual = Vec::new();
            for index in 0usize..100 {
                let Some(row) = window.try_find(("history-tools-commit", index)) else {
                    break;
                };
                let label = row.label().unwrap();
                actual.push(
                    label
                        .strip_prefix("提交 ")
                        .unwrap()
                        .split('，')
                        .next()
                        .unwrap()
                        .to_owned(),
                );
            }
            assert_eq!(actual, expected, "file-history paths are filtered");
            assert!(window.try_find("history-tools-error").is_none());
        })
        .unwrap();
    }

    fn close_history(&self, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.click("history-tools-close", cx);
        })
        .unwrap();
        self.settle(cx);
    }

    fn enter_child(&self, cx: &mut TestAppContext) {
        cx.update(|cx| {
            self.workspace.update(cx, |view, cx| {
                view.select_submodule(self.child.clone(), cx);
            });
        });
        self.settle(cx);
        cx.read(|cx| {
            let view = self.workspace.read(cx);
            assert!(!view.submodule_loading, "{:?}", view.notice);
            assert!(paths_equal(
                &view.changes_repository().unwrap().root,
                &self.child
            ));
        });
    }

    fn settle_preview(&self, cx: &mut TestAppContext) {
        // The production preview request coalesces pointer navigation for 25ms.
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(50));
        self.settle(cx);
        cx.read(|cx| {
            let view = self.workspace.read(cx);
            assert!(!view.diff_loading, "{:?}", view.notice);
            // request_preview retains its completed Task handle. Readiness is
            // the accepted payload for this exact key, not handle disposal.
            let key = view.active_preview_key().unwrap();
            assert_eq!(view.source_file.as_ref().unwrap().path, key.path);
            assert_eq!(view.diff.as_ref().unwrap().path, key.path);
            assert_eq!(view.preview_pane.read(cx).source().unwrap().path, key.path);
            assert_eq!(
                view.preview_pane.read(cx).file_diff().unwrap().path,
                key.path
            );
        });
    }

    fn open_preview_menu(&self, source: bool, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.render_frame(cx);
            let context = window.find(if source {
                "preview-source-context"
            } else {
                "preview-diff-context"
            });
            assert!(context.visible());
            let position =
                context.bounds().origin + point(px(115.), if source { px(12.) } else { px(52.) });
            pointer(window, position, MouseButton::Right, cx);
        })
        .unwrap();
        self.settle(cx);
        cx.update_window(self.window.into(), |_, window, _| {
            assert!(window.find("popup-menu").visible());
            let history = window.find_by_label("查看文件历史");
            assert!(history.visible());
            assert_ne!(history.disabled(), Some(true));
        })
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.directory.parent() == Some(&self.base) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

fn pointer(
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

fn git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "core.hooksPath=",
            "-c",
            "init.templateDir=",
            "-c",
            "commit.gpgsign=false",
            "-C",
        ])
        .arg(display_path(root))
        .args(args)
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "File history fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "File history fixture")
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

#[derive(Debug, PartialEq, Eq)]
struct RepositoryState {
    head: String,
    index: Vec<u8>,
    worktree: Vec<(String, Option<Vec<u8>>)>,
}

fn repository_state(root: &Path) -> RepositoryState {
    RepositoryState {
        head: git(root, &["rev-parse", "HEAD"]),
        index: fs::read(root.join(".git/index")).unwrap(),
        worktree: [
            "a.txt",
            "b.txt",
            "deleted.txt",
            "新 文件.txt",
            "leading.txt",
            " leading.txt",
        ]
        .into_iter()
        .map(|path| (path.to_owned(), fs::read(root.join(path)).ok()))
        .collect(),
    }
}

#[gpui_kit::test]
fn tree_context_history_reads_clicked_file_without_changing_selection_or_data(
    cx: &mut TestAppContext,
) {
    let f = Fixture::new(cx, true);
    f.select_a(cx);
    let parent = repository_state(&f.root);
    let child = repository_state(&f.child);
    f.open_history("b.txt", cx);
    f.assert_history(&f.root, "b.txt", cx);
    cx.read(|cx| {
        assert_eq!(f.workspace.read(cx).selected_file.as_deref(), Some("a.txt"));
    });
    assert_eq!(repository_state(&f.root), parent);
    assert_eq!(repository_state(&f.child), child);
}

#[gpui_kit::test]
fn status_group_context_history_reads_clicked_staged_file_and_preserves_index(
    cx: &mut TestAppContext,
) {
    let f = Fixture::new(cx, false);
    f.select_a(cx);
    let parent = repository_state(&f.root);
    let child = repository_state(&f.child);
    f.open_history("b.txt", cx);
    f.assert_history(&f.root, "b.txt", cx);
    cx.read(|cx| {
        let view = f.workspace.read(cx);
        assert_eq!(view.selected_file.as_deref(), Some("a.txt"));
        assert!(!view.selected_staged);
    });
    assert_eq!(repository_state(&f.root), parent);
    assert_eq!(repository_state(&f.child), child);
}

#[gpui_kit::test]
fn child_same_relative_path_history_uses_child_commits(cx: &mut TestAppContext) {
    let f = Fixture::new(cx, true);
    f.enter_child(cx);
    f.select_a(cx);
    let parent = repository_state(&f.root);
    let child = repository_state(&f.child);
    f.open_history("b.txt", cx);
    f.assert_history(&f.child, "b.txt", cx);
    assert_ne!(
        git(&f.root, &["log", "--format=%H", "--", "b.txt"]),
        git(&f.child, &["log", "--format=%H", "--", "b.txt"])
    );
    cx.read(|cx| {
        assert_eq!(f.workspace.read(cx).selected_file.as_deref(), Some("a.txt"));
    });
    assert_eq!(repository_state(&f.root), parent);
    assert_eq!(repository_state(&f.child), child);
}

#[gpui_kit::test]
fn context_history_preserves_leading_filename_space_instead_of_reading_the_trimmed_name(
    cx: &mut TestAppContext,
) {
    let f = Fixture::new(cx, true);
    f.select_a(cx);
    let parent = repository_state(&f.root);
    let child = repository_state(&f.child);
    let path = " leading.txt";
    assert_ne!(
        git(&f.root, &["log", "--format=%H", "--", path]),
        git(&f.root, &["log", "--format=%H", "--", path.trim()])
    );
    f.open_history(path, cx);
    // assert_history checks the raw input value and actual rendered commit IDs;
    // there is also a different tracked file whose name is the trimmed path.
    f.assert_history(&f.root, path, cx);
    cx.read(|cx| {
        assert_eq!(f.workspace.read(cx).selected_file.as_deref(), Some("a.txt"));
    });
    assert_eq!(repository_state(&f.root), parent);
    assert_eq!(repository_state(&f.child), child);
}

#[gpui_kit::test]
fn deleted_history_is_available_and_untracked_history_is_empty(cx: &mut TestAppContext) {
    let f = Fixture::new(cx, false);
    f.select_a(cx);
    let before = repository_state(&f.root);
    f.open_history("deleted.txt", cx);
    f.assert_history(&f.root, "deleted.txt", cx);
    f.close_history(cx);
    f.open_history("新 文件.txt", cx);
    f.assert_history(&f.root, "新 文件.txt", cx);
    cx.update_window(f.window.into(), |_, window, _| {
        assert!(window.try_find(("history-tools-commit", 0usize)).is_none());
    })
    .unwrap();
    cx.read(|cx| {
        assert_eq!(f.workspace.read(cx).selected_file.as_deref(), Some("a.txt"));
    });
    assert_eq!(repository_state(&f.root), before);
}

#[gpui_kit::test]
fn file_menu_from_previous_scope_cannot_open_history_for_new_scope(cx: &mut TestAppContext) {
    let f = Fixture::new(cx, false);
    // Matching row identities make the popup survive the root replacement. The
    // callback must still reject its captured old root epoch rather than read
    // the same relative filename from the newly active child.
    cx.update(|cx| {
        f.workspace
            .read(cx)
            .file_list
            .clone()
            .update(cx, |list, cx| {
                list.set_query("b.txt".into(), cx);
            });
    });
    f.settle(cx);
    f.open_menu("b.txt", cx);
    let parent = repository_state(&f.root);
    let child = repository_state(&f.child);
    cx.update(|cx| {
        f.workspace.update(cx, |view, cx| {
            let repository = view.service.open(&f.child).unwrap();
            view.submodule_request_id = view.submodule_request_id.wrapping_add(1);
            view.submodule_changes = Some(SubmoduleChanges {
                parent_root: view.repository.as_ref().unwrap().root.clone(),
                target: repository.root.clone(),
                tree: Arc::new(view.service.tree(&f.child).unwrap()),
                repository,
                watcher: None,
                watcher_needs_restart: false,
            });
            view.sync_file_list(cx);
            cx.notify();
        });
    });
    f.settle(cx);
    cx.update_window(f.window.into(), |_, window, cx| {
        if let Some(item) = window.try_find_by_label("查看文件历史") {
            pointer(window, item.bounds().center(), MouseButton::Left, cx);
        } else {
            // A menu dismissed when the old row disappears is also safe. Send
            // the pending keyboard activation to catch a retained old focus.
            window.press("enter", cx);
        }
    })
    .unwrap();
    f.settle(cx);
    cx.read(|cx| {
        let view = f.workspace.read(cx);
        assert!(!view.history_tools_dialog.read(cx).is_open());
        assert!(paths_equal(
            &view.changes_repository().unwrap().root,
            &f.child
        ));
    });
    assert_eq!(repository_state(&f.root), parent);
    assert_eq!(repository_state(&f.child), child);
}

#[gpui_kit::test]
fn child_source_preview_history_uses_the_tab_identity_instead_of_another_selected_file(
    cx: &mut TestAppContext,
) {
    let f = Fixture::new(cx, true);
    f.enter_child(cx);
    f.click_label("文件 b.txt", cx);
    f.settle_preview(cx);
    cx.update(|cx| {
        f.workspace.update(cx, |view, cx| {
            // Selection and preview are separate UI state. Deliberately leave
            // A selected while the retained B tab is rendered, so falling back
            // to selected_file instead of the immutable PreviewKey is visible.
            view.selected_file = Some("a.txt".into());
            view.file_list
                .update(cx, |list, cx| list.select("a.txt".into(), false, cx));
            view.source = true;
            view.sync_preview(cx);
            let key = view.active_preview_key().unwrap();
            assert_eq!(key.path, "b.txt");
            assert!(paths_equal(&key.root, &f.child));
            assert!(key.staged);
            cx.notify();
        });
    });
    f.settle(cx);
    let parent = repository_state(&f.root);
    let child = repository_state(&f.child);
    f.open_preview_menu(true, cx);
    f.click_label("查看文件历史", cx);
    f.assert_history(&f.child, "b.txt", cx);
    cx.read(|cx| {
        assert_eq!(f.workspace.read(cx).selected_file.as_deref(), Some("a.txt"));
    });
    assert_eq!(repository_state(&f.root), parent);
    assert_eq!(repository_state(&f.child), child);
}

#[gpui_kit::test]
fn pending_preview_history_click_is_rejected_after_another_tab_becomes_active(
    cx: &mut TestAppContext,
) {
    let f = Fixture::new(cx, true);
    f.select_a(cx);
    f.settle_preview(cx);
    f.click_label("文件 b.txt", cx);
    f.settle_preview(cx);
    f.open_preview_menu(false, cx);
    let parent = repository_state(&f.root);
    let child = repository_state(&f.child);
    cx.update_window(f.window.into(), |_, window, cx| {
        let position = window.find_by_label("查看文件历史").bounds().center();
        window.pointer_move(position, None, cx);
        window.dispatch_event(
            MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
                first_mouse: false,
            }
            .to_platform_input(),
            cx,
        );
        // A tab switch can arrive between a menu press and its release. Keep
        // the previous frame's popup callback queued so the identity guard,
        // rather than simply removing the popup element, rejects old B's event.
        f.workspace.update(cx, |view, cx| {
            assert_eq!(view.active_preview_key().unwrap().path, "b.txt");
            view.activate_tab(0, cx);
            assert_eq!(view.active_preview_key().unwrap().path, "a.txt");
        });
        window.dispatch_event(
            MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Modifiers::default(),
                click_count: 1,
            }
            .to_platform_input(),
            cx,
        );
        window.render_frame(cx);
    })
    .unwrap();
    f.settle(cx);
    cx.read(|cx| {
        let view = f.workspace.read(cx);
        assert!(!view.history_tools_dialog.read(cx).is_open());
        assert_eq!(view.active_preview_key().unwrap().path, "a.txt");
    });
    // A fresh menu from A must still work after rebinding the subscription.
    f.open_preview_menu(false, cx);
    f.click_label("查看文件历史", cx);
    f.assert_history(&f.root, "a.txt", cx);
    assert_eq!(repository_state(&f.root), parent);
    assert_eq!(repository_state(&f.child), child);
}
