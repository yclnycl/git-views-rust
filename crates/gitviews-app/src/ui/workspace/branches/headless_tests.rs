//! Branch commands through the actual Workspace with isolated local repositories.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use gitviews_service::{AppSettings, Head, SettingsStore};
use gpui_kit::{
    AppContext, Entity, TestAppContext, WindowHandle,
    base::Root,
    px, size,
    test::{TestQueryExt as _, TestWindowExt},
};

use super::super::{Workspace, WorkspaceOptions, submodules::SubmoduleChanges};

static NEXT: AtomicU64 = AtomicU64::new(0);

include!("click_tests.rs");
include!("scope_tests.rs");

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
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/headless-branch-tests");
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
        git(&root, &["config", "core.autocrlf", "false"]);
        let child = root.join("modules/子仓库");
        git(
            &directory,
            &["init", "--initial-branch=main", child.to_str().unwrap()],
        );
        git(&child, &["config", "core.autocrlf", "false"]);
        fs::write(child.join("note.txt"), "child main\n").unwrap();
        git(&child, &["add", "note.txt"]);
        git(&child, &["commit", "-m", "child main"]);
        git(&child, &["switch", "-c", "child-feature"]);
        fs::write(child.join("note.txt"), "child feature\n").unwrap();
        git(&child, &["commit", "-am", "child feature"]);
        git(&child, &["branch", "child-feature-old"]);
        git(&child, &["switch", "main"]);
        fs::write(root.join("note.txt"), "parent main\n").unwrap();
        fs::write(root.join(".gitmodules"), "[submodule \"child\"]\n\tpath = modules/子仓库\n\turl = https://example.invalid/child\n").unwrap();
        let child_head = git(&child, &["rev-parse", "HEAD"]);
        git(&root, &["add", "note.txt", ".gitmodules"]);
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
        git(&root, &["commit", "-m", "parent main"]);
        git(&root, &["switch", "-c", "parent-feature"]);
        fs::write(root.join("note.txt"), "parent feature\n").unwrap();
        git(&root, &["commit", "-am", "parent feature"]);
        git(&root, &["switch", "main"]);

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
                view.requested_path = Some(parent.root.clone());
                view.repository = Some(parent);
                view.preview = false;
                view.loading = false;
                view.tree_mode = true;
                view.file_list
                    .update(cx, |list, cx| list.set_tree_mode(true, cx));
                view.sync_file_list(cx);
                view.refresh_branch_context(cx);
            })
        });
        cx.run_until_parked();
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
        .args(["-c", "core.hooksPath="])
        .arg("-C")
        .arg(gitviews_service::display_path(root))
        .args(args.iter().map(|argument| {
            if argument.starts_with(r"\\?\") {
                gitviews_service::display_path(Path::new(argument))
            } else {
                (*argument).to_owned()
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
fn active_child_picker_loads_child_branches_and_enter_switches_exact_match(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        fixture
            .workspace
            .update(cx, |view, cx| view.toggle_branch_menu(window, cx));
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        fixture.workspace.update(cx, |view, _| {
            let branches = &view.branch_picker_details().unwrap().branches;
            assert!(branches.iter().any(|branch| branch.name == "child-feature"));
            assert!(
                !branches
                    .iter()
                    .any(|branch| branch.name == "parent-feature")
            );
        });
        window.render_frame(cx);
        window.input("child-feature", cx);
        window.press("enter", cx);
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
            view.submodule_changes.as_ref().unwrap().repository.head,
            Head::Branch("child-feature".into())
        );
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find_by_label("文件 note.txt").visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn sidebar_branch_action_switches_child_and_keeps_parent_head_unchanged(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    cx.update(|cx| {
        fixture.workspace.update(cx, |view, cx| {
            view.checkout_branch("child-feature".into(), false, cx);
            assert!(view.mutation_task.is_some());
        })
    });
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
        assert!(view.submodule_changes.is_some());
        assert_eq!(
            view.repository.as_ref().unwrap().head,
            Head::Branch("main".into())
        );
    });
}

#[gpui_kit::test]
fn pending_child_metadata_never_dispatches_checkout_against_the_parent(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    cx.update(|cx| {
        fixture.workspace.update(cx, |view, cx| {
            view.submodule_loading = true;
            view.checkout_active_branch("child-feature".into(), false, cx);
            assert!(view.mutation_task.is_none());
            assert!(view.notice.as_deref().unwrap().contains("读取完成"));
        })
    });
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
fn returning_to_parent_while_child_checkout_finishes_releases_the_mutation_gate(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    cx.update(|cx| {
        fixture.workspace.update(cx, |view, cx| {
            view.checkout_active_branch("child-feature".into(), false, cx);
            assert!(view.mutation_task.is_some());
            view.show_parent_changes(cx);
        })
    });
    cx.run_until_parked();
    assert_eq!(
        git(&fixture.child, &["symbolic-ref", "--short", "HEAD"]).trim(),
        "child-feature"
    );
    cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        assert!(view.mutation_task.is_none());
        assert!(!view.mutation_refresh);
        assert!(view.submodule_changes.is_none());
        assert_eq!(
            view.repository.as_ref().unwrap().head,
            Head::Branch("main".into())
        );
    });
}
