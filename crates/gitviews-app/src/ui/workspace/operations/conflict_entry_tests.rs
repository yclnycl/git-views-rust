//! Real mouse entry into unmerged rows must load the exact repository and sides.
use super::super::{Workspace, WorkspaceOptions};
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
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
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
    fn new(cx: &mut TestAppContext, tree_mode: bool) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/headless-conflict-entry-tests");
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
            for side in ["base", "ours", "theirs"] {
                fs::write(
                    repo.join(format!("{side}.txt")),
                    format!("{scope} {side}\n"),
                )
                .unwrap();
            }
            fs::write(repo.join("dd.txt"), format!("{scope} base\n")).unwrap();
            fs::write(repo.join("uu.txt"), format!("{scope} base\n")).unwrap();
            git(
                repo,
                &[
                    "add",
                    "--",
                    "base.txt",
                    "ours.txt",
                    "theirs.txt",
                    "dd.txt",
                    "uu.txt",
                ],
            );
            git(repo, &["commit", "-m", &format!("{scope} base")]);
        }
        fs::write(
            root.join(".gitmodules"),
            "[submodule \"child\"]\npath = modules/子仓库\nurl = https://example.invalid/unused\n",
        )
        .unwrap();
        git(&root, &["add", "--", ".gitmodules"]);
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
        for (repo, scope) in [(&root, "parent"), (&child, "child")] {
            create_unmerged_index(repo, scope);
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
            child,
            window,
            workspace,
        };
        cx.update_window(window.into(), |_, window, cx| {
            fixture.workspace.update(cx, |view, cx| {
                let repository = view.service.open(&fixture.root).unwrap();
                view.details = Some(view.service.details(&fixture.root).unwrap());
                view.tree = Some(Arc::new(view.service.tree(&fixture.root).unwrap()));
                view.requested_path = Some(repository.root.clone());
                view.repository = Some(repository);
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

    fn enter_child(&self, cx: &mut TestAppContext) {
        self.workspace
            .update(cx, |view, cx| view.select_submodule(self.child.clone(), cx));
        self.settle(cx);
        cx.read(|cx| {
            assert!(paths_equal(
                &self.workspace.read(cx).changes_repository().unwrap().root,
                &self.child
            ))
        });
    }

    fn click_conflict(&self, path: &str, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.render_frame(cx);
            for path in ["aa.txt", "dd.txt", "uu.txt"] {
                assert!(
                    window.try_find_by_label(&format!("暂存 {path}")).is_none(),
                    "unmerged {path} must not have an ordinary stage button"
                );
                assert!(
                    window
                        .try_find_by_label(&format!("取消暂存 {path}"))
                        .is_none()
                );
            }
            let row = window.find_by_label(&format!("文件 {path}"));
            assert!(row.visible());
            // Aim at filename text, away from optional buttons at the row end.
            let position = row.bounds().origin + gpui_kit::point(px(80.), px(16.));
            pointer(window, position, cx);
        })
        .unwrap();
        self.settle(cx);
    }

    fn assert_versions(&self, root: &Path, path: &str, scope: &str, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                window.find("conflict-repository-path").label(),
                Some(display_path(root).as_str())
            );
            for id in ["conflict-side-0", "conflict-side-1", "conflict-side-2"] {
                assert!(window.find(id).visible());
            }
            assert!(window.find("conflict-result").visible());
            let view = self.workspace.read(cx);
            let dialog = view.conflict_dialog.read(cx);
            assert!(dialog.is_open());
            assert_eq!(dialog.selected_path(), Some(path));
            let versions = dialog
                .versions()
                .expect("exact clicked file versions loaded");
            assert_eq!(versions.path, path);
            assert_eq!(
                versions.base,
                (path != "aa.txt").then(|| format!("{scope} base\n"))
            );
            assert_eq!(
                versions.ours,
                (path != "dd.txt").then(|| format!("{scope} ours\n"))
            );
            assert_eq!(
                versions.theirs,
                (path != "dd.txt").then(|| format!("{scope} theirs\n"))
            );
            assert_eq!(
                versions.result,
                if path == "dd.txt" {
                    String::new()
                } else {
                    format!("{scope} {path} unresolved\n")
                }
            );
            assert!(
                view.mutation_task.is_none(),
                "opening an unmerged row must not launch a write"
            );
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

fn command(root: &Path, args: &[&str]) -> Command {
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
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Conflict entry fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Conflict entry fixture")
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
    command
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = command(root, args).output().unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn create_unmerged_index(root: &Path, scope: &str) {
    let base = git(root, &["rev-parse", "HEAD:base.txt"]);
    let ours = git(root, &["rev-parse", "HEAD:ours.txt"]);
    let theirs = git(root, &["rev-parse", "HEAD:theirs.txt"]);
    let mut entries = String::new();
    for path in ["aa.txt", "dd.txt", "uu.txt"] {
        entries.push_str(&format!("0 {}\t{path}\n", "0".repeat(40)));
        for (stage, hash) in [(1, base.trim()), (2, ours.trim()), (3, theirs.trim())] {
            if path == "aa.txt" && stage == 1 || path == "dd.txt" && stage != 1 {
                continue;
            }
            entries.push_str(&format!("100644 {hash} {stage}\t{path}\n"));
        }
        if path == "dd.txt" {
            fs::remove_file(root.join(path)).unwrap();
        } else {
            fs::write(root.join(path), format!("{scope} {path} unresolved\n")).unwrap();
        }
    }
    let mut child = command(root, &["update-index", "--index-info"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(entries.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn pointer(window: &mut Window, position: gpui_kit::Point<gpui_kit::Pixels>, cx: &mut App) {
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
}

#[derive(Debug, Eq, PartialEq)]
struct Snapshot {
    head: String,
    index: Vec<u8>,
    files: Vec<Option<Vec<u8>>>,
}

fn snapshot(root: &Path) -> Snapshot {
    Snapshot {
        head: git(root, &["rev-parse", "HEAD"]),
        index: fs::read(root.join(".git/index")).unwrap(),
        files: [
            "aa.txt",
            "dd.txt",
            "uu.txt",
            "base.txt",
            "ours.txt",
            "theirs.txt",
        ]
        .map(|path| fs::read(root.join(path)).ok())
        .to_vec(),
    }
}

#[gpui_kit::test]
fn parent_add_add_row_opens_three_way_directly_without_staging(cx: &mut TestAppContext) {
    let f = Fixture::new(cx, true);
    let before = (snapshot(&f.root), snapshot(&f.child));
    f.click_conflict("aa.txt", cx);
    f.assert_versions(&f.root, "aa.txt", "parent", cx);
    assert_eq!((snapshot(&f.root), snapshot(&f.child)), before);
}

#[gpui_kit::test]
fn grouped_child_delete_delete_row_opens_deleted_sides_without_staging(cx: &mut TestAppContext) {
    let f = Fixture::new(cx, false);
    f.enter_child(cx);
    let before = (snapshot(&f.root), snapshot(&f.child));
    f.click_conflict("dd.txt", cx);
    f.assert_versions(&f.child, "dd.txt", "child", cx);
    assert_eq!((snapshot(&f.root), snapshot(&f.child)), before);
}

#[gpui_kit::test]
fn child_same_named_unmerged_row_loads_child_versions_only(cx: &mut TestAppContext) {
    let f = Fixture::new(cx, true);
    f.enter_child(cx);
    let before = (snapshot(&f.root), snapshot(&f.child));
    f.click_conflict("uu.txt", cx);
    f.assert_versions(&f.child, "uu.txt", "child", cx);
    assert_eq!((snapshot(&f.root), snapshot(&f.child)), before);
}
