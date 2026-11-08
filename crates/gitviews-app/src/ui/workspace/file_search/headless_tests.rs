//! Search modes, duplicate paths, async scope guards and source navigation via real UI input.

use super::super::{Workspace, WorkspaceOptions};
use super::{SearchHit, SearchMode};
use gitviews_service::{
    AppSettings, FileTreeEntry, FileTreeKind, Head, RepositorySnapshot, RepositoryTree,
    SettingsStore,
};
use gpui_kit::{
    AppContext as _, Entity, TestAppContext, WindowHandle,
    base::Root,
    px, size,
    test::{TestQueryExt as _, TestWindowExt as _},
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
    window: WindowHandle<Root>,
    workspace: Entity<Workspace>,
}

impl Fixture {
    fn new(cx: &mut TestAppContext) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/headless-search-tests");
        fs::create_dir_all(&base).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let directory = base.join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let mut command = Command::new("git");
        command
            .args(["-c", "init.defaultBranch=main", "init", "--quiet"])
            .current_dir(&directory)
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
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
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        for (path, text) in [
            ("left/needle.rs", "left first\nleft second\nneedle 左侧\n"),
            (
                "right/needle.rs",
                "right first\nright second\nneedle 右侧\n",
            ),
        ] {
            let file = directory.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text).unwrap();
        }
        let (window, workspace) =
            crate::ui::headless::open_window(cx, size(px(1680.), px(1100.)), |window, cx| {
                cx.new(|cx| {
                    let mut view = Workspace::new(
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
                    );
                    view.repository = Some(RepositorySnapshot {
                        root: directory.clone(),
                        name: "搜索测试".into(),
                        head: Head::Unborn("main".into()),
                        changes: Vec::new(),
                        recent_commits: Vec::new(),
                    });
                    view.tree = Some(Arc::new(tree(&["left/needle.rs", "right/needle.rs"])));
                    view.preview = false;
                    view
                })
            });
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        Self {
            base,
            directory,
            window,
            workspace,
        }
    }

    fn search(&self, query: &str, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            self.workspace
                .update(cx, |view, cx| view.open_file_search(window, cx));
            window.render_frame(cx);
            window.press("ctrl-a", cx);
            window.input(query, cx);
        })
        .unwrap();
        self.settle(cx, Duration::from_millis(150));
    }

    fn settle(&self, cx: &mut TestAppContext, elapsed: Duration) {
        // Poll first so the production debounce timer is installed before advancing it.
        cx.run_until_parked();
        cx.executor().advance_clock(elapsed);
        cx.run_until_parked();
        cx.update_window(self.window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    fn click_mode(&self, id: &'static str, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| window.click(id, cx))
            .unwrap();
        self.settle(cx, Duration::from_millis(150));
    }

    fn copy_cursor_line(&self, cx: &mut TestAppContext) -> Option<String> {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.press("shift-end", cx);
            window.press("ctrl-c", cx);
            cx.read_from_clipboard().and_then(|item| item.text())
        })
        .unwrap()
    }

    fn filter(&self, id: &'static str, value: &str, cx: &mut TestAppContext) {
        cx.update_window(self.window.into(), |_, window, cx| {
            window.click(id, cx);
            window.press("ctrl-a", cx);
            window.input(value, cx);
            assert_eq!(window.find(id).value(), Some(value));
        })
        .unwrap();
        self.settle(cx, Duration::from_millis(150));
    }

    fn register_search_children(&self, cx: &mut TestAppContext) -> (PathBuf, PathBuf) {
        let left = self.directory.join("modules/left");
        let right = self.directory.join("modules/right");
        for (root, side) in [(&left, "左侧"), (&right, "右侧")] {
            fs::create_dir_all(root).unwrap();
            fixture_git(root, &["init", "--quiet", "--initial-branch=main"]);
            fixture_git(root, &["config", "core.autocrlf", "false"]);
            fs::write(
                root.join("same.txt"),
                format!("first\nsecond\nPRECISION {side}\n"),
            )
            .unwrap();
            fixture_git(root, &["add", "same.txt"]);
            fixture_git(root, &["commit", "--quiet", "-m", "child fixture"]);
        }
        fs::write(self.directory.join(".gitmodules"), "[submodule \"left\"]\n\tpath = modules/left\n\turl = https://example.invalid/left\n[submodule \"right\"]\n\tpath = modules/right\n\turl = https://example.invalid/right\n").unwrap();
        for (path, root) in [("modules/left", &left), ("modules/right", &right)] {
            let oid = fixture_git(root, &["rev-parse", "HEAD"]);
            fixture_git(
                &self.directory,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    "160000",
                    oid.trim(),
                    path,
                ],
            );
        }
        fixture_git(
            &self.directory,
            &["add", "left/needle.rs", "right/needle.rs", ".gitmodules"],
        );
        fixture_git(
            &self.directory,
            &["commit", "--quiet", "-m", "registered modules"],
        );
        cx.update(|cx| {
            self.workspace.update(cx, |view, cx| {
                view.service.invalidate(&self.directory);
                view.repository = Some(view.service.open(&self.directory).unwrap());
                view.details = Some(view.service.details(&self.directory).unwrap());
                view.tree = Some(Arc::new(view.service.tree(&self.directory).unwrap()));
                view.sync_file_list(cx);
                cx.notify();
            })
        });
        self.settle(cx, Duration::from_millis(40));
        (left, right)
    }
}

fn fixture_git(root: &Path, args: &[&str]) -> String {
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
        .arg(gitviews_service::display_path(root))
        .args(args)
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Search UI Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Search UI Fixture")
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

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.directory.parent() == Some(&self.base) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

fn tree(paths: &[&str]) -> RepositoryTree {
    RepositoryTree {
        entries: paths
            .iter()
            .map(|path| FileTreeEntry {
                path: (*path).into(),
                kind: FileTreeKind::File,
                index_status: '?',
                worktree_status: '?',
            })
            .collect(),
        ..Default::default()
    }
}

#[gpui_kit::test]
fn real_mode_buttons_distinguish_path_and_content_matches_and_duplicate_filenames(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    fixture.search("needle", cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        for path in ["left/needle.rs", "right/needle.rs"] {
            assert!(window.find_by_label(&format!("路径匹配 {path}")).visible());
            assert!(
                window
                    .find_by_label(&format!("内容匹配 {path} 第3行"))
                    .visible()
            );
        }
        assert_eq!(fixture.workspace.read(cx).file_search.results.len(), 4);
    })
    .unwrap();

    fixture.click_mode("search-paths", cx);
    cx.read(|cx| {
        let search = &fixture.workspace.read(cx).file_search;
        assert!(search.mode == SearchMode::Paths);
        assert_eq!(search.results.len(), 2);
        assert!(
            search
                .results
                .iter()
                .all(|hit| matches!(hit, SearchHit::Path(_)))
        );
    });
    fixture.click_mode("search-contents", cx);
    cx.read(|cx| {
        let search = &fixture.workspace.read(cx).file_search;
        assert!(search.mode == SearchMode::Contents);
        assert_eq!(search.results.len(), 2);
        assert!(
            search
                .results
                .iter()
                .all(|hit| matches!(hit, SearchHit::Content(_)))
        );
    });
    fixture.click_mode("search-all", cx);
    cx.read(|cx| assert_eq!(fixture.workspace.read(cx).file_search.results.len(), 4));
}

#[gpui_kit::test]
fn clicking_content_result_opens_the_exact_duplicate_path_and_focuses_the_matching_source_line(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    fixture.search("needle", cx);
    fixture.click_mode("search-contents", cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        let index = fixture
            .workspace
            .read(cx)
            .file_search
            .results
            .iter()
            .position(|hit| hit.path() == "right/needle.rs")
            .unwrap();
        window.click(("quick-open-result", index), cx);
        let view = fixture.workspace.read(cx);
        assert!(!view.file_search.open);
        assert_eq!(view.selected_file.as_deref(), Some("right/needle.rs"));
    })
    .unwrap();
    fixture.settle(cx, Duration::from_millis(40));
    assert_eq!(fixture.copy_cursor_line(cx).as_deref(), Some("needle 右侧"));
}

#[gpui_kit::test]
fn retained_source_navigation_waits_for_the_new_payload_before_clamping_the_match_line(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    cx.update_window(fixture.window.into(), |_, _, cx| {
        fixture.workspace.update(cx, |view, cx| {
            view.open_preview_tab("right/needle.rs".into(), false, None, true, cx)
        });
    })
    .unwrap();
    fixture.settle(cx, Duration::from_millis(40));
    let mut text = (1..80)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    text.push_str("newmarker 目标行\n");
    fs::write(fixture.directory.join("right/needle.rs"), text).unwrap();
    cx.update(|cx| {
        fixture
            .workspace
            .update(cx, |view, _| view.service.invalidate(&fixture.directory))
    });
    fixture.search("newmarker", cx);
    fixture.click_mode("search-contents", cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        assert!(
            window
                .find_by_label("内容匹配 right/needle.rs 第80行")
                .visible()
        );
        window.click(("quick-open-result", 0_usize), cx);
        // This frame still contains the old four-line retained source.
        window.render_frame(cx);
    })
    .unwrap();
    fixture.settle(cx, Duration::from_millis(40));
    assert_eq!(
        fixture.copy_cursor_line(cx).as_deref(),
        Some("newmarker 目标行")
    );
}

#[gpui_kit::test]
fn async_query_completion_cannot_install_results_from_an_old_tree_or_repository_epoch(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        fixture
            .workspace
            .update(cx, |view, cx| view.open_file_search(window, cx));
        window.render_frame(cx);
        window.input("needle", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| {
        fixture.workspace.update(cx, |view, _| {
            view.tree = Some(Arc::new(tree(&["replacement.rs"])));
            view.request_id = view.request_id.wrapping_add(1);
        })
    });
    fs::write(
        fixture.directory.join("replacement.rs"),
        "needle 新上下文\n",
    )
    .unwrap();
    fixture.settle(cx, Duration::from_millis(150));
    cx.read(|cx| {
        let search = &fixture.workspace.read(cx).file_search;
        assert!(search.results.is_empty());
        assert!(search.result_scope.is_none());
    });
    cx.update(|cx| {
        fixture
            .workspace
            .update(cx, |view, cx| view.refresh_file_search(cx))
    });
    fixture.settle(cx, Duration::from_millis(150));
    cx.read(|cx| {
        let search = &fixture.workspace.read(cx).file_search;
        assert!(!search.loading);
        assert_eq!(search.results.len(), 1);
        assert!(
            search
                .results
                .iter()
                .all(|hit| hit.path() == "replacement.rs")
        );
    });
}

#[gpui_kit::test]
fn scan_requeries_open_content_search_when_an_already_untracked_file_changes_again(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    // Establish the real native tree/status before searching. The second scan
    // below must retain this exact tree identity and the same status entries.
    cx.update(|cx| {
        fixture.workspace.update(cx, |view, cx| view.queue_scan(cx));
    });
    cx.run_until_parked();
    let (original_tree, original_changes, scans) = cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        assert!(view.scan_task.is_none());
        let changes = view.repository.as_ref().unwrap().changes.clone();
        assert!(
            changes
                .iter()
                .any(|change| { change.path == "right/needle.rs" && change.index_status == '?' })
        );
        (
            view.tree.as_ref().unwrap().clone(),
            changes,
            view.scan_count,
        )
    });
    fixture.search("live-token", cx);
    fixture.click_mode("search-contents", cx);
    let generation = cx.read(|cx| {
        let search = &fixture.workspace.read(cx).file_search;
        assert!(search.results.is_empty());
        assert!(!search.loading);
        search.generation
    });

    fs::write(
        fixture.directory.join("right/needle.rs"),
        "right first\nlive-token 新的正文\nright final\n",
    )
    .unwrap();
    cx.update(|cx| {
        fixture.workspace.update(cx, |view, cx| view.queue_scan(cx));
    });
    fixture.settle(cx, Duration::from_millis(150));
    cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        assert!(view.scan_task.is_none());
        assert_eq!(view.scan_count, scans + 1);
        assert_eq!(view.repository.as_ref().unwrap().changes, original_changes);
        assert!(Arc::ptr_eq(view.tree.as_ref().unwrap(), &original_tree));
        assert!(view.file_search.generation > generation);
        assert!(!view.file_search.loading);
        assert_eq!(view.file_search.results.len(), 1);
        let SearchHit::Content(hit) = &view.file_search.results[0] else {
            panic!("automatic scan refresh must return the newly matching content");
        };
        assert_eq!(hit.path.as_ref(), "right/needle.rs");
        assert_eq!(hit.line_number, 2);
        assert_eq!(hit.snippet.as_ref(), "live-token 新的正文");
    });
}

#[gpui_kit::test]
fn advanced_regex_case_directory_type_include_and_exclude_controls_filter_real_results(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    fixture.search("NEEDLE (左|右)侧", cx);
    fixture.click_mode("search-contents", cx);
    fixture.click_mode("search-regex", cx);
    cx.read(|cx| assert_eq!(fixture.workspace.read(cx).file_search.results.len(), 2));
    fixture.click_mode("search-case", cx);
    cx.read(|cx| assert!(fixture.workspace.read(cx).file_search.results.is_empty()));
    fixture.search("needle (左|右)侧", cx);
    fixture.filter("search-directory", "right", cx);
    fixture.filter("search-types", "rs", cx);
    fixture.filter("search-include", "*.rs", cx);
    fixture.filter("search-exclude", "left/**", cx);
    cx.read(|cx| {
        let search = &fixture.workspace.read(cx).file_search;
        assert!(search.options.regex && search.options.case_sensitive);
        assert_eq!(search.results.len(), 1);
        assert_eq!(search.results[0].path(), "right/needle.rs");
        let SearchHit::Content(hit) = &search.results[0] else {
            panic!("expected a content match");
        };
        assert_eq!(hit.line_number, 3);
        assert_eq!(&hit.snippet[hit.highlights[0].clone()], "needle 右侧");
    });
    fixture.filter("search-exclude", "right/**", cx);
    cx.read(|cx| assert!(fixture.workspace.read(cx).file_search.results.is_empty()));
    fixture.search("(", cx);
    cx.read(|cx| {
        let search = &fixture.workspace.read(cx).file_search;
        assert!(search.results.is_empty());
        assert!(!search.loading);
        assert!(search.status.contains("正则"), "{}", search.status);
    });
}

#[gpui_kit::test]
fn cross_submodule_same_name_content_click_opens_exact_root_and_source_line(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    let (_, right) = fixture.register_search_children(cx);
    fixture.search("PRECISION", cx);
    fixture.click_mode("search-contents", cx);
    cx.read(|cx| assert!(fixture.workspace.read(cx).file_search.results.is_empty()));
    fixture.click_mode("search-submodules", cx);
    let index = cx.read(|cx| {
        let search = &fixture.workspace.read(cx).file_search;
        assert_eq!(search.results.len(), 2, "{}", search.status);
        search
            .results
            .iter()
            .position(|hit| hit.display_path() == "modules/right/same.txt")
            .unwrap()
    });
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click(("quick-open-result", index), cx)
    })
    .unwrap();
    fixture.settle(cx, Duration::from_millis(100));
    cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        assert!(!view.submodule_loading, "{:?}", view.notice);
        assert!(
            gitviews_service::paths_equal(&view.changes_repository().unwrap().root, &right),
            "{:?}",
            view.notice
        );
        assert_eq!(view.selected_file.as_deref(), Some("same.txt"));
        let tab = view.opened_files.last().unwrap();
        assert!(gitviews_service::paths_equal(&tab.root, &right));
        assert_eq!(tab.path, "same.txt");
    });
    assert_eq!(
        fixture.copy_cursor_line(cx).as_deref(),
        Some("PRECISION 右侧")
    );
}

#[gpui_kit::test]
fn returning_to_parent_cancels_pending_cross_submodule_result_navigation(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    fixture.register_search_children(cx);
    fixture.search("PRECISION", cx);
    fixture.click_mode("search-contents", cx);
    fixture.click_mode("search-submodules", cx);
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.click(("quick-open-result", 0_usize), cx);
        fixture.workspace.update(cx, |view, cx| {
            assert!(view.submodule_loading);
            view.show_parent_changes(cx);
        });
    })
    .unwrap();
    fixture.settle(cx, Duration::from_millis(150));
    cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        assert!(view.submodule_changes.is_none());
        assert!(!view.submodule_loading);
        assert!(view.opened_files.iter().all(|tab| tab.path != "same.txt"));
        assert!(view.selected_file.as_deref() != Some("same.txt"));
        assert!(gitviews_service::paths_equal(
            &view.changes_repository().unwrap().root,
            &fixture.directory
        ));
    });
}
