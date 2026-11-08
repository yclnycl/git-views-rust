//! Native index/commit operations dispatched by real production Workspace UI.
use super::super::{Field, Workspace, WorkspaceOptions};
use gitviews_service::{AppSettings, SettingsStore, display_path, paths_equal};
use gpui_kit::{
    App, AppContext as _, Entity, InputEvent as _, Modifiers, MouseButton, MouseDownEvent,
    MouseUpEvent, TestAppContext, Window, WindowHandle,
    base::Root,
    px, size,
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
    window: WindowHandle<Root>,
    workspace: Entity<Workspace>,
}
impl Fixture {
    fn new(cx: &mut TestAppContext, unborn: bool) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/headless-write-tests");
        fs::create_dir_all(&base).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let directory = base.join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let root = directory.join("中文 仓库");
        git(
            &directory,
            &["init", "--initial-branch=main", &display_path(&root)],
        );
        configure(&root);
        fs::write(root.join("note.txt"), base_text()).unwrap();
        if !unborn {
            git(&root, &["add", "note.txt"]);
            git(&root, &["commit", "-m", "fixture base"]);
        }
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
        let fixture = Self {
            base,
            directory,
            root,
            window,
            workspace,
        };
        fixture.reload(cx);
        fixture
    }

    fn reload(&self, cx: &mut TestAppContext) {
        cx.update(|cx| {
            self.workspace.update(cx, |view, cx| {
                view.service.invalidate(&self.root);
                let snapshot = view.service.open(&self.root).unwrap();
                view.details = Some(view.service.details(&self.root).unwrap());
                view.tree = Some(Arc::new(view.service.tree(&self.root).unwrap()));
                view.requested_path = Some(snapshot.root.clone());
                view.repository = Some(snapshot);
                view.preview = false;
                view.loading = false;
                view.tree_mode = false;
                view.file_list
                    .update(cx, |list, cx| list.set_tree_mode(false, cx));
                view.sync_file_list(cx);
                cx.notify();
            })
        });
        self.settle(cx);
    }

    fn settle(&self, cx: &mut TestAppContext) {
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        cx.update_window(self.window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    fn click(&self, id: &'static str, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find(id).visible(), "{id} must be visible");
            window.click(id, cx);
        })
        .unwrap();
        self.settle(cx);
    }

    fn click_label(&self, label: &str, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.find_by_label(label).visible(),
                "{label} must be visible"
            );
            let position = window.find_by_label(label).bounds().center();
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
            window.render_frame(cx);
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
        self.settle(cx);
    }

    fn enter_commit(&self, summary: &str, description: &str, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            fill(window, "commit-summary", summary, cx);
            let position = window.find_by_label("提交详细描述").bounds().center();
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
            window.render_frame(cx);
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
            window.press("ctrl-a", cx);
            window.input(description, cx);
            assert_eq!(
                window.find_by_label("提交详细描述").value(),
                Some(description)
            );
        })
        .unwrap();
    }

    fn select_diff(&self, staged: bool, cx: &mut TestAppContext) {
        cx.update(|cx| {
            self.workspace.update(cx, |view, cx| {
                view.select_file("note.txt".into(), staged, cx)
            })
        });
        self.settle(cx);
        cx.read(|cx| {
            let view = self.workspace.read(cx);
            assert!(
                view.diff.is_some(),
                "preview should load: {:?}",
                view.notice
            );
            assert_eq!(view.selected_staged, staged);
        });
    }

    fn assert_mutation_done(&self, cx: &TestAppContext) {
        cx.read(|cx| {
            let view = self.workspace.read(cx);
            assert!(view.mutation_task.is_none(), "write must finish");
            assert!(
                !view.mutation_refresh,
                "post-write scan must finish: {:?}",
                view.notice
            );
        });
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.directory.parent() == Some(self.base.as_path()) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

fn base_text() -> String {
    (1..=30).map(|line| format!("line {line}\n")).collect()
}
fn changed_text() -> String {
    base_text()
        .replace("line 2\n", "first change\n")
        .replace("line 28\n", "last change\n")
}
fn fill(window: &mut Window, id: &'static str, text: &str, cx: &mut App) {
    window.click(id, cx);
    window.press("ctrl-a", cx);
    window.input(text, cx);
    assert_eq!(window.find(id).value(), Some(text));
}
fn configure(root: &Path) {
    for (key, value) in [
        ("core.autocrlf", "false"),
        ("user.name", "Write Fixture"),
        ("user.email", "fixture@example.invalid"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", ""),
    ] {
        git(root, &["config", key, value]);
    }
}
fn git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.hooksPath=", "-c", "init.templateDir=", "-C"])
        .arg(display_path(root))
        .args(args)
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Write Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Write Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CONFIG_COUNT");
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
fn file_stage_and_unstage_buttons_do_not_open_preview(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, false);
    fs::write(fixture.root.join("note.txt"), "local worktree change\n").unwrap();
    fixture.reload(cx);
    fixture.click_label("暂存 note.txt", cx);
    fixture.assert_mutation_done(cx);
    assert_eq!(
        git(&fixture.root, &["show", ":note.txt"]),
        "local worktree change\n"
    );
    cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        assert!(
            view.opened_files.is_empty(),
            "index button must stop row click propagation"
        );
        assert!(view.selected_file.is_none());
    });
    fixture.click_label("取消暂存 note.txt", cx);
    fixture.assert_mutation_done(cx);
    assert_eq!(git(&fixture.root, &["show", ":note.txt"]), base_text());
    assert_eq!(
        fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
        "local worktree change\n"
    );
    cx.read(|cx| assert!(fixture.workspace.read(cx).opened_files.is_empty()));
}

#[gpui_kit::test]
fn commit_form_submits_only_index_preserves_later_edits_and_clears_draft(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, false);
    fs::write(fixture.root.join("note.txt"), "staged version\n").unwrap();
    fixture.reload(cx);
    fixture.click("stage-all", cx);
    fs::write(fixture.root.join("note.txt"), "later unstaged version\n").unwrap();
    fixture.enter_commit("提交暂存内容", "详情第一行\n详情第二行", cx);
    fixture.click("commit-staged", cx);
    fixture.assert_mutation_done(cx);
    assert_eq!(
        git(&fixture.root, &["show", "HEAD:note.txt"]),
        "staged version\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
        "later unstaged version\n"
    );
    assert_eq!(
        git(&fixture.root, &["log", "-1", "--format=%B"]).trim_end(),
        "提交暂存内容\n\n详情第一行\n详情第二行"
    );
    cx.update_window(fixture.window.into(), |_, window, cx| {
        assert_eq!(window.find("commit-summary").value(), Some(""));
        assert_eq!(window.find_by_label("提交详细描述").value(), Some(""));
        assert!(fixture.workspace.read(cx).summary.is_empty());
        assert!(fixture.workspace.read(cx).description.is_empty());
    })
    .unwrap();
}

#[gpui_kit::test]
fn unborn_repository_can_stage_and_create_first_commit_through_form(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, true);
    fixture.click("stage-all", cx);
    fixture.enter_commit("首次提交", "新仓库", cx);
    fixture.click("commit-staged", cx);
    fixture.assert_mutation_done(cx);
    assert_eq!(
        git(&fixture.root, &["rev-list", "--count", "HEAD"]).trim(),
        "1"
    );
    assert_eq!(git(&fixture.root, &["show", "HEAD:note.txt"]), base_text());
    assert_eq!(
        git(&fixture.root, &["log", "-1", "--format=%B"]).trim_end(),
        "首次提交\n\n新仓库"
    );
    cx.read(|cx| assert!(fixture.workspace.read(cx).summary.is_empty()));
}

#[gpui_kit::test]
fn hunk_button_stages_and_unstages_only_selected_block_of_two(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx, false);
    fs::write(fixture.root.join("note.txt"), changed_text()).unwrap();
    fixture.reload(cx);
    fixture.select_diff(false, cx);
    cx.read(|cx| {
        assert_eq!(
            fixture
                .workspace
                .read(cx)
                .diff
                .as_ref()
                .unwrap()
                .patch
                .match_indices("\n@@ ")
                .count(),
            2
        )
    });
    fixture.click("stage-selected-hunk", cx);
    fixture.assert_mutation_done(cx);
    assert_eq!(
        git(&fixture.root, &["show", ":note.txt"]),
        base_text().replace("line 2\n", "first change\n")
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
        changed_text()
    );
    fixture.click("stage-all", cx);
    fixture.select_diff(true, cx);
    cx.read(|cx| {
        assert_eq!(
            fixture
                .workspace
                .read(cx)
                .diff
                .as_ref()
                .unwrap()
                .patch
                .match_indices("\n@@ ")
                .count(),
            2
        )
    });
    fixture.click("stage-selected-hunk", cx);
    fixture.assert_mutation_done(cx);
    assert_eq!(
        git(&fixture.root, &["show", ":note.txt"]),
        base_text().replace("line 28\n", "last change\n")
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
        changed_text()
    );
}

#[gpui_kit::test]
fn child_commit_form_targets_active_submodule_without_moving_parent_head_or_index(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx, false);
    let child = fixture.root.join("modules/子仓库");
    git(
        &fixture.directory,
        &["init", "--initial-branch=main", &display_path(&child)],
    );
    configure(&child);
    fs::write(child.join("child.txt"), "child initial\n").unwrap();
    git(&child, &["add", "child.txt"]);
    git(&child, &["commit", "-m", "child initial"]);
    let child_initial = git(&child, &["rev-parse", "HEAD"]);
    fs::write(
        fixture.root.join(".gitmodules"),
        "[submodule \"child\"]\n\tpath = modules/子仓库\n\turl = https://example.invalid/child\n",
    )
    .unwrap();
    git(&fixture.root, &["add", ".gitmodules"]);
    git(
        &fixture.root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            child_initial.trim(),
            "modules/子仓库",
        ],
    );
    git(&fixture.root, &["commit", "-m", "register child"]);
    fs::write(child.join("child.txt"), "child staged\n").unwrap();
    fixture.reload(cx);
    let parent_head = git(&fixture.root, &["rev-parse", "HEAD"]);
    let parent_index = fs::read(fixture.root.join(".git/index")).unwrap();
    cx.update(|cx| {
        fixture
            .workspace
            .update(cx, |view, cx| view.select_submodule(child.clone(), cx))
    });
    fixture.settle(cx);
    cx.read(|cx| {
        assert!(paths_equal(
            &fixture
                .workspace
                .read(cx)
                .changes_repository()
                .unwrap()
                .root,
            &child
        ))
    });
    fixture.click("stage-all", cx);
    fixture.enter_commit("仅提交子模块", "子模块范围", cx);
    fixture.click("commit-staged", cx);
    fixture.assert_mutation_done(cx);
    assert_eq!(git(&fixture.root, &["rev-parse", "HEAD"]), parent_head);
    assert_eq!(
        fs::read(fixture.root.join(".git/index")).unwrap(),
        parent_index
    );
    assert_ne!(git(&child, &["rev-parse", "HEAD"]), child_initial);
    assert_eq!(git(&child, &["show", "HEAD:child.txt"]), "child staged\n");
    assert_eq!(
        git(&child, &["log", "-1", "--format=%B"]).trim_end(),
        "仅提交子模块\n\n子模块范围"
    );
}

#[gpui_kit::test]
fn returning_to_parent_during_child_commit_refreshes_gitlink_and_preserves_parent_draft(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx, false);
    let child = fixture.root.join("modules/后台子仓库");
    git(
        &fixture.directory,
        &["init", "--initial-branch=main", &display_path(&child)],
    );
    configure(&child);
    fs::write(child.join("child.txt"), "child initial\n").unwrap();
    git(&child, &["add", "child.txt"]);
    git(&child, &["commit", "-m", "child initial"]);
    let child_initial = git(&child, &["rev-parse", "HEAD"]);
    fs::write(fixture.root.join(".gitmodules"),
        "[submodule \"child\"]\n\tpath = modules/后台子仓库\n\turl = https://example.invalid/child\n").unwrap();
    git(&fixture.root, &["add", ".gitmodules"]);
    git(
        &fixture.root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            child_initial.trim(),
            "modules/后台子仓库",
        ],
    );
    git(
        &fixture.root,
        &["commit", "-m", "register background child"],
    );
    fs::write(child.join("child.txt"), "background child commit\n").unwrap();
    git(&child, &["add", "child.txt"]);
    fixture.reload(cx);
    let parent_head = git(&fixture.root, &["rev-parse", "HEAD"]);
    cx.update(|cx| {
        fixture
            .workspace
            .update(cx, |view, cx| view.select_submodule(child.clone(), cx))
    });
    fixture.settle(cx);
    fixture.enter_commit("后台子模块提交", "属于子模块的提交内容", cx);
    let scans_before = cx.read(|cx| fixture.workspace.read(cx).scan_count);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click("commit-staged", cx);
        fixture.workspace.update(cx, |view, cx| {
            assert!(
                view.mutation_task.is_some(),
                "real native write must still be pending publication"
            );
            view.show_parent_changes(cx);
            // The active scope has its own draft by the time the background
            // completion arrives; it must never be cleared by a child commit.
            view.summary = "父仓库草稿".into();
            view.description = "父仓库尚未提交的说明".into();
            view.inputs[Field::Summary.index()]
                .update(cx, |input, cx| input.set_value("父仓库草稿", window, cx));
            view.commit_description.update(cx, |input, cx| {
                input.set_value("父仓库尚未提交的说明", window, cx)
            });
            cx.notify();
        });
    })
    .unwrap();
    fixture.settle(cx);
    fixture.assert_mutation_done(cx);
    let completed_child_head = git(&child, &["rev-parse", "HEAD"]);
    assert_ne!(completed_child_head, child_initial);
    assert_eq!(git(&fixture.root, &["rev-parse", "HEAD"]), parent_head);
    assert_eq!(
        git(&child, &["show", "HEAD:child.txt"]),
        "background child commit\n"
    );
    cx.update_window(fixture.window.into(), |_, window, cx| {
        let view = fixture.workspace.read(cx);
        assert!(view.submodule_changes.is_none());
        assert!(
            view.scan_count > scans_before,
            "old-scope completion must refresh the visible parent"
        );
        let module = view
            .tree
            .as_ref()
            .unwrap()
            .submodules
            .iter()
            .find(|module| module.path == "modules/后台子仓库")
            .unwrap();
        assert_eq!(
            module.workdir_id.as_deref(),
            Some(completed_child_head.trim())
        );
        assert!(
            view.repository
                .as_ref()
                .unwrap()
                .changes
                .iter()
                .any(|change| change.path == "modules/后台子仓库" && change.worktree_status != ' '),
            "parent must refresh its changed gitlink: {:?}",
            view.repository.as_ref().unwrap().changes
        );
        assert_eq!(view.summary, "父仓库草稿");
        assert_eq!(view.description, "父仓库尚未提交的说明");
        assert!(
            !view
                .notice
                .as_deref()
                .unwrap_or_default()
                .contains("正在处理")
        );
        assert!(
            view.notice
                .as_deref()
                .unwrap_or_default()
                .contains("已提交"),
            "{:?}",
            view.notice
        );
        assert_eq!(window.find("commit-summary").value(), Some("父仓库草稿"));
        assert_eq!(
            window.find_by_label("提交详细描述").value(),
            Some("父仓库尚未提交的说明")
        );
    })
    .unwrap();
}
