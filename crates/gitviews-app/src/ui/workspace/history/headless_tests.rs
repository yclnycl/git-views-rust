//! Native multi-reference graphs and filters through the real Workspace controls.
//! All Git writes live below target; browsing must preserve both fixture indexes.
use super::super::{Workspace, WorkspaceOptions, submodules::SubmoduleChanges};
use gitviews_service::{AppSettings, GraphReferenceKind, SettingsStore, display_path, paths_equal};
use gpui_kit::{
    AppContext as _, Entity, InputEvent as _, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent,
    TestAppContext, WindowHandle,
    base::Root,
    px, size,
    test::{TestEventExt as _, TestWindowExt as _},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
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
    fn new(cx: &mut TestAppContext) -> Self {
        Self::with_size(cx, size(px(1680.), px(1100.)))
    }

    fn with_size(cx: &mut TestAppContext, window_size: gpui_kit::Size<gpui_kit::Pixels>) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/headless-multiref-history-tests");
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
        create_graph(&root, "parent");
        let child = root.join("modules/子仓库");
        fs::create_dir_all(child.parent().unwrap()).unwrap();
        git(
            &directory,
            &["init", "--initial-branch=main", &display_path(&child)],
        );
        create_graph(&child, "child");
        fs::write(
            root.join(".gitmodules"),
            "[submodule \"child\"]\npath = modules/子仓库\nurl = https://example.invalid/unused\n",
        )
        .unwrap();
        git(&root, &["add", ".gitmodules"]);
        git(
            &root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                git(&child, &["rev-parse", "HEAD"]).trim(),
                "modules/子仓库",
            ],
        );
        git(&root, &["commit", "-m", "parent register child"]);
        for repo in [&root, &child] {
            fs::write(repo.join("base.txt"), "unstaged data must stay\n").unwrap();
            fs::write(repo.join("staged.txt"), "staged data must stay\n").unwrap();
            git(repo, &["add", "staged.txt"]);
            fs::write(repo.join("未跟踪 文件.txt"), "untracked must stay\n").unwrap();
        }
        let (window, workspace) =
            crate::ui::headless::open_window(cx, window_size, |window, cx| {
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
        fixture.set_scope(false, cx);
        fixture
    }

    fn set_scope(&self, child: bool, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            self.workspace.update(cx, |view, cx| {
                view.reset_history(cx);
                view.history = false;
                let repository = view.service.open(&self.root).unwrap();
                view.details = Some(view.service.details(&self.root).unwrap());
                view.tree = Some(Arc::new(view.service.tree(&self.root).unwrap()));
                view.requested_path = Some(repository.root.clone());
                view.submodule_changes = child.then(|| {
                    let child = view.service.open(&self.child).unwrap();
                    SubmoduleChanges {
                        parent_root: repository.root.clone(),
                        target: child.root.clone(),
                        tree: Arc::new(view.service.tree(&self.child).unwrap()),
                        repository: child,
                        watcher: None,
                        watcher_needs_restart: false,
                    }
                });
                view.repository = Some(repository);
                view.preview = false;
                view.loading = false;
                view.submodule_loading = false;
                view.focus.focus(window, cx);
                view.sync_file_list(cx);
                cx.notify();
            });
        })
        .unwrap();
        self.settle(cx);
    }

    fn settle(&self, cx: &mut TestAppContext) {
        cx.run_until_parked();
        cx.update_window(self.window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    fn open(&self, cx: &mut TestAppContext) {
        self.click("commit-history-toolbar", cx);
        cx.read(|cx| {
            let view = self.workspace.read(cx);
            assert!(view.history);
            assert!(!view.history_loading);
            assert!(
                view.history_state.page_error.is_none(),
                "{:?}",
                view.history_state.page_error
            );
            assert!(view.history_state.snapshot.is_some());
        });
    }

    fn click(&self, id: impl Into<gpui_kit::ElementId>, cx: &mut TestAppContext) {
        let id = id.into();
        cx.update_window(self.window.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        })
        .unwrap();
        self.settle(cx);
    }

    fn read_only_state(&self) -> Vec<RepositoryState> {
        [&self.root, &self.child]
            .into_iter()
            .map(|root| RepositoryState {
                head: git(root, &["rev-parse", "HEAD"]),
                branch: git(root, &["symbolic-ref", "HEAD"]),
                index: fs::read(root.join(".git/index")).unwrap(),
                status: git(root, &["status", "--porcelain=v1", "--untracked-files=all"]),
                unstaged: fs::read(root.join("base.txt")).unwrap(),
                staged: fs::read(root.join("staged.txt")).unwrap(),
                untracked: fs::read(root.join("未跟踪 文件.txt")).unwrap(),
            })
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct RepositoryState {
    head: String,
    branch: String,
    index: Vec<u8>,
    status: String,
    unstaged: Vec<u8>,
    staged: Vec<u8>,
    untracked: Vec<u8>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // canonicalized base + exact direct parent bounds recursive fixture cleanup.
        if self.directory.parent() == Some(&self.base) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

fn create_graph(root: &Path, scope: &str) {
    git(root, &["config", "core.autocrlf", "false"]);
    commit_file(root, "base.txt", &format!("{scope} base"), "Fixture");
    git(root, &["branch", "feature"]);
    commit_file(root, "main.txt", &format!("{scope} main"), "Bob");
    git(root, &["checkout", "feature"]);
    commit_file(
        root,
        "wanted.txt",
        &format!("{scope} needle"),
        "Alice Feature",
    );
    commit_file(root, "other.txt", &format!("{scope} context"), "Bob");
    git(root, &["checkout", "main"]);
    git(
        root,
        &[
            "merge",
            "--no-ff",
            "feature",
            "-m",
            &format!("{scope} merge"),
        ],
    );
    git(root, &["checkout", "-b", "local-only", "main~1"]);
    commit_file(root, "local.txt", &format!("{scope} local"), "Fixture");
    git(root, &["checkout", "-b", "temporary-remote", "main~1"]);
    commit_file(root, "remote.txt", &format!("{scope} remote"), "Fixture");
    let remote = git(root, &["rev-parse", "HEAD"]);
    git(
        root,
        &[
            "update-ref",
            "refs/remotes/origin/remote-only",
            remote.trim(),
        ],
    );
    git(root, &["checkout", "main"]);
    git(root, &["branch", "-D", "temporary-remote"]);
}

fn commit_file(root: &Path, path: &str, message: &str, author: &str) {
    fs::write(root.join(path), format!("{message}\n")).unwrap();
    git(root, &["add", path]);
    git_as(root, &["commit", "-m", message], author);
}

fn git(root: &Path, args: &[&str]) -> String {
    git_as(root, args, "Fixture")
}

fn git_as(root: &Path, args: &[&str], author: &str) -> String {
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
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_AUTHOR_NAME", author)
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
fn native_multibranch_history_opens_all_tips_and_applies_real_branch_multiselect(
    cx: &mut TestAppContext,
) {
    let f = Fixture::new(cx);
    let unchanged = f.read_only_state();
    f.open(cx);
    cx.update_window(f.window.into(), |_, window, cx| {
        window.render_frame(cx);
        let commit = window.find(("commit", 0usize));
        let source = window.find("history-source");
        let split = window.find("history-split");
        assert!(commit.visible() && source.visible() && split.visible());
        assert_eq!(commit.bounds().size.height, px(32.));
        assert!(source.bounds().origin.y > commit.bounds().bottom());
        assert!(source.bounds().size.height > px(10.));
    })
    .unwrap();
    f.click("history-toggle-details", cx);
    cx.update_window(f.window.into(), |_, window, _| {
        assert!(window.find(("commit", 0usize)).visible());
        assert!(
            window
                .try_find("history-source")
                .is_none_or(|control| !control.visible())
        );
        assert!(
            window
                .try_find("history-split")
                .is_none_or(|control| !control.visible())
        );
    })
    .unwrap();
    f.click("history-toggle-details", cx);
    cx.update_window(f.window.into(), |_, window, _| {
        assert!(window.find("history-source").visible());
        assert!(window.find("history-split").visible());
    })
    .unwrap();
    let expected = git(&f.root, &["rev-list", "--parents", "--all"])
        .lines()
        .map(|line| {
            let mut values = line.split_whitespace();
            (
                values.next().unwrap().to_owned(),
                values.map(str::to_owned).collect::<Vec<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let selected_refs = ["refs/heads/feature", "refs/remotes/origin/remote-only"];
    let (reference_indices, expected_tips) = cx.read(|cx| {
        let view = f.workspace.read(cx);
        let snapshot = view.history_state.snapshot.as_ref().unwrap();
        assert!(paths_equal(&snapshot.root, &f.root));
        assert!(
            snapshot
                .references
                .iter()
                .any(|reference| reference.kind == GraphReferenceKind::Head)
        );
        assert!(
            snapshot
                .references
                .iter()
                .any(|reference| reference.kind == GraphReferenceKind::RemoteBranch)
        );
        let actual = view
            .history_state
            .graph_rows
            .iter()
            .map(|row| (row.commit.id.clone(), row.commit.parents.clone()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(actual, expected);
        assert!(
            view.history_state
                .graph_rows
                .iter()
                .any(|row| row.commit.parents.len() == 2)
        );
        assert!(
            view.history_state
                .graph_rows
                .iter()
                .any(|row| row.lanes > 1)
        );
        let colors = view
            .history_state
            .graph_rows
            .iter()
            .flat_map(|row| {
                std::iter::once(row.color).chain(row.segments.iter().map(|edge| edge.color))
            })
            .collect::<BTreeSet<_>>();
        assert!(colors.len() > 1, "actual topology must use multiple colors");
        let references = snapshot
            .references
            .iter()
            .filter(|reference| reference.kind != GraphReferenceKind::Tag)
            .collect::<Vec<_>>();
        let indices = selected_refs.map(|name| {
            references
                .iter()
                .position(|reference| reference.full_name == name)
                .unwrap()
        });
        let tips = selected_refs
            .iter()
            .map(|name| {
                references
                    .iter()
                    .find(|reference| reference.full_name == *name)
                    .unwrap()
                    .target
                    .clone()
            })
            .collect::<BTreeSet<_>>();
        (indices, tips)
    });
    f.click("history-branches", cx);
    f.click("history-all-branches", cx);
    for index in reference_indices {
        f.click(("history-ref", index), cx);
    }
    cx.read(|cx| {
        let state = &f.workspace.read(cx).history_state;
        assert_eq!(
            state
                .draft_refs
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            selected_refs.into_iter().collect()
        );
        assert!(state.references.is_empty(), "draft must wait for Apply");
    });
    f.click("history-apply-branches", cx);
    cx.read(|cx| {
        let state = &f.workspace.read(cx).history_state;
        assert!(!state.ref_menu);
        assert_eq!(
            state
                .snapshot
                .as_ref()
                .unwrap()
                .tips
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            expected_tips
        );
        assert_eq!(
            state
                .references
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            selected_refs.into_iter().collect()
        );
    });
    f.click("history-branches", cx);
    f.click(("history-ref", reference_indices[0]), cx);
    f.click("history-close-branches", cx);
    f.click("reload-history", cx);
    cx.read(|cx| {
        let state = &f.workspace.read(cx).history_state;
        assert!(
            !state.ref_menu,
            "a completed reload must not reopen a canceled selector"
        );
        assert_eq!(
            state
                .references
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            selected_refs.into_iter().collect()
        );
    });
    assert_eq!(f.read_only_state(), unchanged);
}

#[gpui_kit::test]
fn native_child_history_filters_text_author_and_path_without_losing_graph_context(
    cx: &mut TestAppContext,
) {
    let f = Fixture::new(cx);
    f.set_scope(true, cx);
    let unchanged = f.read_only_state();
    f.open(cx);
    let initial_rows = cx.read(|cx| {
        let view = f.workspace.read(cx);
        assert!(paths_equal(
            &view.history_state.snapshot.as_ref().unwrap().root,
            &f.child
        ));
        assert!(
            view.history_state
                .graph_rows
                .iter()
                .all(|row| row.commit.subject.starts_with("child "))
        );
        view.history_state.graph_rows.len()
    });
    cx.update_window(f.window.into(), |_, window, cx| {
        f.workspace.update(cx, |view, cx| {
            view.history_state
                .input
                .as_ref()
                .unwrap()
                .update(cx, |input, cx| input.set_value("needle", window, cx));
            view.history_state
                .author_input
                .as_ref()
                .unwrap()
                .update(cx, |input, cx| input.set_value("Alice", window, cx));
            view.history_state
                .path_input
                .as_ref()
                .unwrap()
                .update(cx, |input, cx| input.set_value("wanted.txt", window, cx));
        });
        window.render_frame(cx);
    })
    .unwrap();
    f.settle(cx);
    cx.read(|cx| {
        let view = f.workspace.read(cx);
        assert_eq!(view.history_state.graph_rows.len(), initial_rows);
        assert_eq!(
            view.history_state.indices.len(),
            initial_rows,
            "typing prepares a query until Apply"
        );
    });
    f.click("history-apply-filter", cx);
    cx.read(|cx| {
        let view = f.workspace.read(cx);
        let state = &view.history_state;
        assert!(paths_equal(
            &state.snapshot.as_ref().unwrap().root,
            &f.child
        ));
        assert_eq!(
            state.graph_rows.len(),
            initial_rows,
            "connecting commits must remain present"
        );
        assert_eq!(state.indices.len(), 1);
        let matching = state
            .graph_rows
            .iter()
            .filter(|row| row.matches)
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].commit.subject, "child needle");
        assert_eq!(matching[0].commit.author, "Alice Feature");
        assert!(
            state
                .graph_rows
                .iter()
                .any(|row| !row.matches && row.commit.parents.len() == 2)
        );
        assert!(
            state
                .graph_rows
                .iter()
                .any(|row| !row.matches && !row.segments.is_empty())
        );
        assert!(
            state
                .graph_rows
                .iter()
                .all(|row| row.commit.subject.starts_with("child "))
        );
        assert!(state.page_error.is_none(), "{:?}", state.page_error);
    });
    assert_eq!(f.read_only_state(), unchanged);
}

#[gpui_kit::test]
fn pending_parent_history_clicks_cannot_change_a_new_child_history_session(
    cx: &mut TestAppContext,
) {
    let f = Fixture::new(cx);
    let unchanged = f.read_only_state();
    for action in ["checkbox", "apply", "commit"] {
        f.set_scope(false, cx);
        f.open(cx);
        let parent_commit = cx.read(|cx| {
            f.workspace.read(cx).history_state.graph_rows[0]
                .commit
                .id
                .clone()
        });
        if action != "commit" {
            f.click("history-branches", cx);
        }
        let feature = cx.read(|cx| {
            f.workspace
                .read(cx)
                .history_state
                .snapshot
                .as_ref()
                .unwrap()
                .references
                .iter()
                .filter(|reference| reference.kind != GraphReferenceKind::Tag)
                .position(|reference| reference.full_name == "refs/heads/feature")
                .unwrap()
        });
        if action == "apply" {
            f.click(("history-ref", feature), cx);
        }
        cx.update_window(f.window.into(), |_, window, cx| {
            let target: gpui_kit::ElementId = match action {
                "apply" => "history-apply-branches".into(),
                "commit" => ("commit", 0usize).into(),
                _ => ("history-ref", feature).into(),
            };
            let position = window.find(target).bounds().center();
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
            // Do not render between down/up: the previous parent popup's actual
            // click listener remains in the dispatch tree while the model moves
            // to a fresh child session. Merely removing the popup cannot pass.
            let child_state = f.workspace.update(cx, |view, cx| {
                view.reset_history(cx);
                view.history = false;
                let repository = view.service.open(&f.child).unwrap();
                view.submodule_changes = Some(SubmoduleChanges {
                    parent_root: view.repository.as_ref().unwrap().root.clone(),
                    target: repository.root.clone(),
                    tree: Arc::new(view.service.tree(&f.child).unwrap()),
                    repository,
                    watcher: None,
                    watcher_needs_restart: false,
                });
                view.toggle_history(cx);
                assert!(paths_equal(
                    view.history_state.root.as_ref().unwrap(),
                    &f.child
                ));
                assert!(view.history_state.references.is_empty());
                assert!(view.history_state.draft_refs.is_empty());
                (
                    view.history_request_id,
                    view.commit_request_id,
                    view.history_state.selected_id.clone(),
                )
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
            let view = f.workspace.read(cx);
            assert_eq!(
                view.history_request_id, child_state.0,
                "stale apply must not restart the child's read"
            );
            assert_eq!(
                view.commit_request_id, child_state.1,
                "stale parent row must not start a child details request"
            );
            assert_eq!(view.history_state.selected_id, child_state.2);
            assert_ne!(
                view.history_state.selected_id.as_ref(),
                Some(&parent_commit)
            );
            assert!(paths_equal(
                view.history_state.root.as_ref().unwrap(),
                &f.child
            ));
            assert!(view.history_state.references.is_empty());
            assert!(
                view.history_state.draft_refs.is_empty(),
                "stale checkbox must not select a parent reference"
            );
            assert!(!view.history_state.ref_menu);
            window.render_frame(cx);
        })
        .unwrap();
        f.settle(cx);
        cx.read(|cx| {
            let view = f.workspace.read(cx);
            assert!(paths_equal(
                &view.history_state.snapshot.as_ref().unwrap().root,
                &f.child
            ));
            assert!(
                view.history_state
                    .graph_rows
                    .iter()
                    .all(|row| row.commit.subject.starts_with("child "))
            );
            assert!(view.history_state.references.is_empty());
            assert!(view.history_state.draft_refs.is_empty());
            assert_ne!(
                view.history_state.selected_id.as_ref(),
                Some(&parent_commit)
            );
        });
    }
    assert_eq!(f.read_only_state(), unchanged);
}

#[gpui_kit::test]
fn minimum_width_history_keeps_commit_subject_and_hash_readable_in_both_themes(
    cx: &mut TestAppContext,
) {
    let f = Fixture::with_size(cx, size(px(1200.), px(800.)));
    let unchanged = f.read_only_state();
    // Multiple long reference labels on the first row must not consume the
    // subject's reserved width at the application's real minimum window size.
    for branch in [
        "aaa-layout/long-reference-alpha",
        "aaa-layout/long-reference-beta",
        "aaa-layout/long-reference-gamma",
    ] {
        git(&f.root, &["branch", branch]);
    }
    f.open(cx);
    for appearance in [
        crate::ui::theme::Appearance::Light,
        crate::ui::theme::Appearance::Dark,
    ] {
        cx.update_window(f.window.into(), |_, window, cx| {
            appearance.apply(window, cx);
            window.render_frame(cx);
            let subject = window.find(("history-subject", 0usize));
            let hash = window.find(("history-hash", 0usize));
            assert!(subject.visible() && hash.visible());
            assert!(
                subject.bounds().size.width >= px(180.),
                "subject width {:?}",
                subject.bounds()
            );
            assert!(hash.bounds().size.width >= px(62.));
            assert!(subject.bounds().left() >= px(0.) && subject.bounds().right() <= px(1200.));
            assert!(
                hash.bounds().left() >= px(0.) && hash.bounds().right() <= px(1200.),
                "hash fully inside minimum window: {:?}",
                hash.bounds()
            );
            assert!(subject.bounds().right() <= hash.bounds().left());
            assert!(window.find("history-apply-filter").visible());
            assert!(window.find("history-source").visible());
        })
        .unwrap();
    }
    assert_eq!(f.read_only_state(), unchanged);
}
