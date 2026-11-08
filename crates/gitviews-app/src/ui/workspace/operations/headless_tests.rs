//! Real Workspace shortcuts, child scope and asynchronous publication guards.
use super::super::{Workspace, WorkspaceOptions, submodules::SubmoduleChanges};
use crate::ui::history_tools_dialog::HistoryToolsDialogEvent;
use gitviews_service::{
    AppSettings, HistoryGraphPage, HistoryToolsRequest, HistoryToolsResult, SettingsStore,
    display_path,
};
use gpui_kit::{
    AppContext, Entity, TestAppContext, WindowHandle, base::Root, px, size, test::TestWindowExt,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
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
    fn new(cx: &mut TestAppContext) -> Self {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/headless-operation-tests");
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
        let child = root.join("modules/子仓库");
        git(
            &directory,
            &["init", "--initial-branch=main", child.to_str().unwrap()],
        );
        for (repo, message) in [(&child, "child history"), (&root, "parent history")] {
            git(repo, &["config", "core.autocrlf", "false"]);
            fs::write(repo.join("note.txt"), format!("{message}\n")).unwrap();
            git(repo, &["add", "note.txt"]);
            git(repo, &["commit", "-m", message]);
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
        cx.update_window(window.into(), |_, window, cx| {
            workspace.update(cx, |view, cx| {
                let parent = view.service.open(&root).unwrap();
                view.details = Some(view.service.details(&root).unwrap());
                view.tree = Some(Arc::new(view.service.tree(&root).unwrap()));
                let repository = view.service.open(&child).unwrap();
                view.submodule_changes = Some(SubmoduleChanges {
                    parent_root: parent.root.clone(),
                    target: repository.root.clone(),
                    tree: Arc::new(view.service.tree(&child).unwrap()),
                    repository,
                    watcher: None,
                    watcher_needs_restart: false,
                });
                view.repository = Some(parent);
                view.preview = false;
                view.loading = false;
                view.submodule_loading = false;
                view.requested_path = Some(root.clone());
                view.selected_file = Some("note.txt".into());
                view.focus.focus(window, cx);
                view.sync_file_list(cx);
                cx.notify();
            })
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        Self {
            base,
            directory,
            root,
            child,
            window,
            workspace,
        }
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
        .args(["-c", "core.hooksPath=", "-C"])
        .arg(display_path(root))
        .args(args.iter().map(|arg| {
            if arg.starts_with(r"\\?\") {
                display_path(Path::new(arg))
            } else {
                (*arg).to_owned()
            }
        }))
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
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
fn history_shortcut_uses_active_child_and_prefills_selected_file(cx: &mut TestAppContext) {
    let f = Fixture::new(cx);
    cx.update_window(f.window.into(), |_, window, cx| {
        window.press("ctrl-alt-h", cx)
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(f.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("history-tools-root").label(),
            Some(display_path(&f.child).as_str())
        );
        assert!(
            window
                .find(("history-tools-commit", 0usize))
                .label()
                .unwrap()
                .contains(git(&f.child, &["rev-parse", "HEAD"]).trim())
        );
        window.click("history-tools-mode-file", cx);
        assert_eq!(window.find("history-tools-path").value(), Some("note.txt"));
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        git(&f.root, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "main"
    );
}

#[gpui_kit::test]
fn history_close_cancels_token_and_invalidates_queued_results(cx: &mut TestAppContext) {
    let f = Fixture::new(cx);
    cx.update_window(f.window.into(), |_, window, cx| {
        window.press("ctrl-alt-h", cx)
    })
    .unwrap();
    let token = Arc::new(AtomicBool::new(false));
    let observed = token.clone();
    cx.update_window(f.window.into(), |_, window, cx| {
        f.workspace.update(cx, |view, _| {
            view.operations_read_cancel = Some(token);
        });
        window.render_frame(cx);
        window.click("history-tools-close", cx);
    })
    .unwrap();
    assert!(observed.load(Ordering::Relaxed));
    cx.run_until_parked();
    cx.read(|cx| assert!(!f.workspace.read(cx).history_tools_dialog.read(cx).is_open()));
    cx.update_window(f.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("history-tools-root").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn history_result_from_old_child_scope_cannot_replace_new_scope(cx: &mut TestAppContext) {
    let f = Fixture::new(cx);
    cx.update_window(f.window.into(), |_, window, cx| {
        window.press("ctrl-alt-h", cx)
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(f.window.into(), |_, window, cx| {
        f.workspace.update(cx, |view, cx| {
            view.history_tools_dialog.update(cx, |dialog, cx| {
                dialog.set_result(HistoryToolsResult::Commits(HistoryGraphPage::default()), cx);
                dialog.set_busy(true, cx);
            });
            view.handle_history_tools(
                &HistoryToolsDialogEvent::Request(HistoryToolsRequest::Commits {
                    anchor: None,
                    offset: 0,
                    limit: 100,
                    path: None,
                }),
                window,
                cx,
            );
            view.submodule_request_id = view.submodule_request_id.wrapping_add(1);
            view.submodule_changes = None;
            cx.notify();
        })
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(f.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find(("history-tools-commit", 0usize)).is_none());
        assert_eq!(window.find("history-tools-load").label(), Some("读取中…"));
    })
    .unwrap();
}

#[gpui_kit::test]
fn branch_manager_shortcut_creates_only_in_active_child(cx: &mut TestAppContext) {
    let f = Fixture::new(cx);
    let parent_head = git(&f.root, &["rev-parse", "HEAD"]);
    let parent_index = fs::read(f.root.join(".git/index")).unwrap();
    cx.update_window(f.window.into(), |_, window, cx| {
        window.press("ctrl-alt-b", cx)
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(f.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("branch-root-path").label(),
            Some(display_path(&f.child).as_str())
        );
        window.click("branch-name", cx);
        window.input("child-created", cx);
        window.click("branch-apply", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        git(&f.child, &["rev-parse", "child-created"]).trim(),
        git(&f.child, &["rev-parse", "HEAD"]).trim()
    );
    assert_eq!(git(&f.root, &["rev-parse", "HEAD"]), parent_head);
    assert_eq!(fs::read(f.root.join(".git/index")).unwrap(), parent_index);
    assert!(!git(&f.root, &["branch", "--list", "child-created"]).contains("child-created"));
}

#[gpui_kit::test]
fn legacy_history_shortcut_reads_child_and_exit_preserves_child_scope(cx: &mut TestAppContext) {
    let f = Fixture::new(cx);
    let child_head = git(&f.child, &["rev-parse", "HEAD"]);
    cx.update_window(f.window.into(), |_, window, cx| window.press("ctrl-h", cx))
        .unwrap();
    cx.run_until_parked();
    cx.read(|cx| {
        let view = f.workspace.read(cx);
        assert!(view.history);
        assert_eq!(view.history_commits.first().unwrap().id, child_head.trim());
        assert!(
            view.history_commits
                .iter()
                .all(|commit| commit.subject == "child history")
        );
    });
    cx.update_window(f.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("exit-history", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.read(|cx| {
        let view = f.workspace.read(cx);
        assert!(!view.history);
        assert!(gitviews_service::paths_equal(
            &view.changes_repository().unwrap().root,
            &f.child
        ));
    });
}

#[gpui_kit::test]
fn stash_inspect_opens_read_only_child_preview_and_includes_untracked_snapshot(
    cx: &mut TestAppContext,
) {
    let f = Fixture::new(cx);
    fs::write(f.child.join("note.txt"), "staged snapshot\n").unwrap();
    git(&f.child, &["add", "note.txt"]);
    fs::write(f.child.join("note.txt"), "working snapshot\n").unwrap();
    fs::write(f.child.join("draft.txt"), "untracked snapshot\n").unwrap();
    git(
        &f.child,
        &[
            "stash",
            "push",
            "--include-untracked",
            "-m",
            "inspect fixture",
        ],
    );
    let stash = git(&f.child, &["rev-parse", "refs/stash"]);
    let child_head = git(&f.child, &["rev-parse", "HEAD"]);
    let parent_head = git(&f.root, &["rev-parse", "HEAD"]);
    let index = fs::read(f.child.join(".git/index")).unwrap();
    cx.update_window(f.window.into(), |_, window, cx| {
        window.press("ctrl-alt-s", cx)
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(f.window.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(("stash-entry", 0usize), cx);
        window.click("stash-inspect", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(f.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("history-tools-root").label(),
            Some(display_path(&f.child).as_str())
        );
        assert_eq!(
            window.find("history-tools-right").value(),
            Some(stash.trim())
        );
        window.click("history-stash-untracked", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(f.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window
                .find(("history-tools-compare-file", 0usize))
                .label()
                .unwrap()
                .contains("draft.txt")
        );
        window.click(("history-tools-compare-file", 0usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(f.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window
                .find(("history-tools-diff-line", 1usize))
                .label()
                .unwrap()
                .contains("新增 untracked snapshot")
        );
    })
    .unwrap();
    assert_eq!(git(&f.child, &["rev-parse", "refs/stash"]), stash);
    assert_eq!(git(&f.child, &["rev-parse", "HEAD"]), child_head);
    assert_eq!(git(&f.root, &["rev-parse", "HEAD"]), parent_head);
    assert_eq!(fs::read(f.child.join(".git/index")).unwrap(), index);
    assert!(!f.child.join("draft.txt").exists());
}
