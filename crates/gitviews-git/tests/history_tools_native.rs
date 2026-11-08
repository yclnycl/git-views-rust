//! Native history analysis through the production backend, on local fixtures only.
use git2::{Oid, Repository, Signature, Time};
use gitviews_git::{
    BranchAction, BranchRequest, Git2Backend, GitBackend, HistoryGraphPage, HistoryToolsErrorKind,
    HistoryToolsRequest as Request, HistoryToolsResult as ResultData,
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    base: PathBuf,
    root: PathBuf,
    repo: Repository,
    first: Oid,
    second: Oid,
    side: Oid,
    merged: Oid,
}
impl Fixture {
    fn new() -> Self {
        let base = fs::canonicalize(std::env::temp_dir()).unwrap();
        let root = base.join(format!(
            "gitviews-history-tools-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let repo = Repository::init(&root).unwrap();
        repo.config()
            .unwrap()
            .set_bool("core.autocrlf", false)
            .unwrap();
        repo.config()
            .unwrap()
            .set_str("user.name", "GitViews fixture")
            .unwrap();
        repo.config()
            .unwrap()
            .set_str("user.email", "fixture@example.invalid")
            .unwrap();
        let first = commit(
            &repo,
            &[("note.txt", b"alpha\nbeta\n"), ("other.txt", b"other\n")],
            &[],
            "initial",
            0,
        );
        let second = commit(
            &repo,
            &[("note.txt", b"alpha\nchanged\n"), ("other.txt", b"other\n")],
            &[first],
            "change note",
            1,
        );
        let side = commit(
            &repo,
            &[("note.txt", b"alpha\nbeta\n"), ("other.txt", b"side\n")],
            &[first],
            "side changes only other",
            2,
        );
        let merged = commit(
            &repo,
            &[("note.txt", b"alpha\nchanged\n"), ("other.txt", b"side\n")],
            &[second, side],
            "merge",
            3,
        );
        repo.reference("refs/heads/main", merged, false, "fixture")
            .unwrap();
        repo.set_head("refs/heads/main").unwrap();
        repo.checkout_head(None).unwrap();
        repo.tag(
            "v1",
            &repo.find_object(first, None).unwrap(),
            &Signature::now("Fixture", "fixture@example.invalid").unwrap(),
            "first version",
            false,
        )
        .unwrap();
        Self {
            base,
            root,
            repo,
            first,
            second,
            side,
            merged,
        }
    }
    fn run(&self, request: Request) -> Result<ResultData, gitviews_git::HistoryToolsError> {
        Git2Backend.history_tools(&self.root, &request, &AtomicBool::new(false))
    }
    fn graph(
        &self,
        anchor: Option<String>,
        offset: usize,
        limit: usize,
        path: Option<String>,
    ) -> HistoryGraphPage {
        let ResultData::Commits(page) = self
            .run(Request::Commits {
                anchor,
                offset,
                limit,
                path,
            })
            .unwrap()
        else {
            panic!("graph expected")
        };
        page
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let resolved = fs::canonicalize(&self.root).unwrap();
        assert_eq!(resolved.parent(), Some(self.base.as_path()));
        assert!(
            resolved
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("gitviews-history-tools-")
        );
        let _ = fs::remove_dir_all(resolved);
    }
}
fn commit(
    repo: &Repository,
    files: &[(&str, &[u8])],
    parents: &[Oid],
    message: &str,
    time: i64,
) -> Oid {
    let mut builder = repo.treebuilder(None).unwrap();
    for (path, bytes) in files {
        builder
            .insert(*path, repo.blob(bytes).unwrap(), 0o100644)
            .unwrap();
    }
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let parents = parents
        .iter()
        .map(|id| repo.find_commit(*id).unwrap())
        .collect::<Vec<_>>();
    let signature = Signature::new(
        "测试作者",
        "fixture@example.invalid",
        &Time::new(1_700_000_000 + time, 0),
    )
    .unwrap();
    repo.commit(
        None,
        &signature,
        &signature,
        message,
        &tree,
        &parents.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}

#[test]
fn graph_pages_have_merge_parents_and_pin_anchor_across_new_commits() {
    let f = Fixture::new();
    let page = f.graph(None, 0, 2, None);
    assert_eq!(page.commits[0].id, f.merged.to_string());
    assert_eq!(
        page.commits[0].parents,
        vec![f.second.to_string(), f.side.to_string()]
    );
    assert!(page.has_more);
    let next = commit(
        &f.repo,
        &[("note.txt", b"later\n")],
        &[f.merged],
        "external newer HEAD",
        4,
    );
    f.repo
        .reference("refs/heads/main", next, true, "external")
        .unwrap();
    let rest = f.graph(page.anchor, page.next_offset, 10, None);
    assert!(!rest.has_more);
    assert_eq!(rest.commits.len(), 2);
    assert!(!rest.commits.iter().any(|c| c.id == next.to_string()));
    assert!(
        !page
            .commits
            .iter()
            .any(|a| rest.commits.iter().any(|b| a.id == b.id))
    );
}

#[test]
fn file_history_filters_unrelated_commits_and_keeps_merge_resolution() {
    let f = Fixture::new();
    let page = f.graph(None, 0, 100, Some("note.txt".into()));
    let ids = page
        .commits
        .iter()
        .map(|c| c.id.as_str())
        .collect::<Vec<_>>();
    assert!(ids.contains(&f.first.to_string().as_str()));
    assert!(ids.contains(&f.second.to_string().as_str()));
    assert!(ids.contains(&f.merged.to_string().as_str()));
    assert!(!ids.contains(&f.side.to_string().as_str()));
    assert_eq!(page.scanned_commits, 4);
}

#[test]
fn blame_version_and_working_buffer_preserve_text_and_mark_uncommitted_lines() {
    let f = Fixture::new();
    let ResultData::Blame(view) = f
        .run(Request::Blame {
            path: "note.txt".into(),
            revision: Some(f.second.to_string()),
        })
        .unwrap()
    else {
        panic!("blame expected")
    };
    assert_eq!(
        view.lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<String>(),
        "alpha\nchanged\n"
    );
    assert_eq!(view.lines[0].commit_id, Some(f.first.to_string()));
    assert_eq!(view.lines[1].commit_id, Some(f.second.to_string()));
    assert_eq!(view.lines[1].original_line_number, 2);
    let index = fs::read(f.repo.path().join("index")).unwrap();
    fs::write(f.root.join("note.txt"), "alpha\nlocal edit\nnew line\n").unwrap();
    let ResultData::Blame(view) = f
        .run(Request::Blame {
            path: "note.txt".into(),
            revision: None,
        })
        .unwrap()
    else {
        panic!("blame expected")
    };
    assert_eq!(view.lines[0].commit_id, Some(f.first.to_string()));
    assert!(view.lines[1..].iter().all(|line| line.commit_id.is_none()));
    assert_eq!(view.lines.len(), 3);
    assert_eq!(fs::read(f.repo.path().join("index")).unwrap(), index);
    assert_eq!(f.repo.head().unwrap().target(), Some(f.merged));
}

#[test]
fn compares_any_two_commits_in_both_directions_and_annotated_tags() {
    let f = Fixture::new();
    let ResultData::Comparison(view) = f
        .run(Request::Compare {
            left: "v1".into(),
            right: f.merged.to_string(),
            path: Some("note.txt".into()),
        })
        .unwrap()
    else {
        panic!("comparison expected")
    };
    assert_eq!(view.left, f.first.to_string());
    assert_eq!(view.files.len(), 2);
    assert_eq!((view.insertions, view.deletions), (2, 2));
    let diff = view.diff.unwrap();
    assert_eq!(diff.old_text, "alpha\nbeta\n");
    assert_eq!(diff.new_text, "alpha\nchanged\n");
    assert!(diff.patch.contains("-beta"));
    assert!(diff.patch.contains("+changed"));
    let ResultData::Comparison(reverse) = f
        .run(Request::Compare {
            left: f.merged.to_string(),
            right: f.first.to_string(),
            path: Some("note.txt".into()),
        })
        .unwrap()
    else {
        panic!("comparison expected")
    };
    assert_eq!(reverse.diff.unwrap().new_text, "alpha\nbeta\n");
}

#[test]
fn utf16_versions_are_decoded_for_diffs_and_blame_reports_encoding_limit() {
    let f = Fixture::new();
    let old = utf16("中文\n旧行\n", false);
    let new = utf16("中文\n新行\n", true);
    let a = commit(
        &f.repo,
        &[("中文.txt", &old)],
        &[f.merged],
        "utf16 little",
        5,
    );
    let b = commit(&f.repo, &[("中文.txt", &new)], &[a], "utf16 big", 6);
    let ResultData::Comparison(view) = f
        .run(Request::Compare {
            left: a.to_string(),
            right: b.to_string(),
            path: Some("中文.txt".into()),
        })
        .unwrap()
    else {
        panic!("comparison expected")
    };
    assert_eq!((view.insertions, view.deletions), (1, 1));
    let diff = view.diff.unwrap();
    assert_eq!(diff.old_text, "中文\n旧行\n");
    assert_eq!(diff.new_text, "中文\n新行\n");
    assert!(diff.patch.contains("+新行"));
    assert_eq!(
        f.run(Request::Blame {
            path: "中文.txt".into(),
            revision: Some(b.to_string())
        })
        .unwrap_err()
        .kind,
        HistoryToolsErrorKind::UnsupportedEncoding
    );
}
fn utf16(text: &str, big: bool) -> Vec<u8> {
    let mut result = if big {
        vec![0xfe, 0xff]
    } else {
        vec![0xff, 0xfe]
    };
    for value in text.encode_utf16() {
        result.extend_from_slice(&if big {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        });
    }
    result
}

#[test]
fn binary_and_oversized_selected_files_fail_clearly_without_blocking_metadata_list() {
    let f = Fixture::new();
    let large = vec![b'x'; 2 * 1024 * 1024 + 1];
    let b = commit(
        &f.repo,
        &[("binary.bin", &[0xff, 0x00, 0x01]), ("large.txt", &large)],
        &[f.merged],
        "large and binary",
        5,
    );
    for (path, expected) in [
        ("binary.bin", HistoryToolsErrorKind::BinaryFile),
        ("large.txt", HistoryToolsErrorKind::Limit),
    ] {
        assert_eq!(
            f.run(Request::Compare {
                left: f.merged.to_string(),
                right: b.to_string(),
                path: Some(path.into())
            })
            .unwrap_err()
            .kind,
            expected
        );
    }
    let ResultData::Comparison(summary) = f
        .run(Request::Compare {
            left: f.merged.to_string(),
            right: b.to_string(),
            path: None,
        })
        .unwrap()
    else {
        panic!("comparison expected")
    };
    assert!(summary.files.iter().any(|file| file.path == "large.txt"));
    assert!(summary.truncated);
}

#[test]
fn recovery_lists_retained_deleted_branch_and_reflog_then_restores_without_checkout() {
    let f = Fixture::new();
    f.repo
        .reference("refs/heads/to-delete", f.side, false, "fixture")
        .unwrap();
    let deletion = Git2Backend
        .manage_branch(
            &f.root,
            &BranchRequest {
                expected_head: Some(f.merged.to_string()),
                action: BranchAction::Delete {
                    name: "to-delete".into(),
                    force: false,
                },
            },
        )
        .unwrap();
    let ResultData::Recovery(page) = f
        .run(Request::RecoveryList {
            offset: 0,
            limit: 200,
        })
        .unwrap()
    else {
        panic!("recovery expected")
    };
    assert!(
        page.entries
            .iter()
            .any(|entry| entry.commit_id == f.side.to_string()
                && entry.source_reference
                    == deletion.recovery_reference.as_ref().unwrap().as_str())
    );
    assert!(
        page.entries
            .iter()
            .any(|entry| entry.label.contains("to-delete"))
    );
    fs::write(f.root.join("note.txt"), "local stays\n").unwrap();
    let index = fs::read(f.repo.path().join("index")).unwrap();
    f.run(Request::Restore {
        name: "recovered/中文".into(),
        commit_id: f.side.to_string(),
        expected_head: Some(f.merged.to_string()),
    })
    .unwrap();
    assert_eq!(
        f.repo.refname_to_id("refs/heads/recovered/中文").unwrap(),
        f.side
    );
    assert_eq!(f.repo.head().unwrap().shorthand().unwrap(), "main");
    assert_eq!(fs::read(f.repo.path().join("index")).unwrap(), index);
    assert_eq!(
        fs::read_to_string(f.root.join("note.txt")).unwrap(),
        "local stays\n"
    );
    assert!(
        f.run(Request::Restore {
            name: "main".into(),
            commit_id: f.side.to_string(),
            expected_head: Some(f.merged.to_string())
        })
        .is_err()
    );
}

#[test]
fn recovery_includes_detached_reflog_tips_that_are_not_current_branches() {
    let f = Fixture::new();
    let orphan = commit(&f.repo, &[("lost.txt", b"lost\n")], &[], "lost detached", 7);
    f.repo.set_head_detached(orphan).unwrap();
    f.repo.set_head("refs/heads/main").unwrap();
    let ResultData::Recovery(page) = f
        .run(Request::RecoveryList {
            offset: 0,
            limit: 200,
        })
        .unwrap()
    else {
        panic!("recovery expected")
    };
    assert!(
        page.entries
            .iter()
            .any(|entry| entry.commit_id == orphan.to_string()
                && entry.source_reference.starts_with("HEAD@{"))
    );
}

#[test]
fn invalid_paths_limits_revisions_and_cancel_are_rejected_before_work() {
    let f = Fixture::new();
    for path in ["../escape", ".git/HEAD", "a\\b", "a:ads", "/absolute"] {
        assert_eq!(
            f.run(Request::Blame {
                path: path.into(),
                revision: None
            })
            .unwrap_err()
            .kind,
            HistoryToolsErrorKind::InvalidRequest
        );
    }
    for request in [
        Request::Commits {
            anchor: None,
            offset: 0,
            limit: 201,
            path: None,
        },
        Request::RecoveryList {
            offset: 50_001,
            limit: 10,
        },
        Request::Compare {
            left: "HEAD\0bad".into(),
            right: "HEAD".into(),
            path: None,
        },
    ] {
        assert_eq!(
            f.run(request).unwrap_err().kind,
            HistoryToolsErrorKind::InvalidRequest
        );
    }
    let request = Request::Commits {
        anchor: None,
        offset: 0,
        limit: 100,
        path: None,
    };
    assert_eq!(
        Git2Backend
            .history_tools(&f.root, &request, &AtomicBool::new(true))
            .unwrap_err()
            .kind,
        HistoryToolsErrorKind::Cancelled
    );
    let nested = f.root.join("folder");
    fs::create_dir(&nested).unwrap();
    assert_eq!(
        Git2Backend
            .history_tools(&nested, &request, &AtomicBool::new(false))
            .unwrap_err()
            .kind,
        HistoryToolsErrorKind::InvalidRepository
    );
}

#[test]
fn unborn_history_is_empty_and_restore_requires_full_commit_id_and_current_head() {
    let f = Fixture::new();
    f.repo.set_head("refs/heads/unborn").unwrap();
    assert!(f.graph(None, 0, 100, None).commits.is_empty());
    assert_eq!(
        f.run(Request::Restore {
            name: "recovered".into(),
            commit_id: f.first.to_string()[..8].into(),
            expected_head: None
        })
        .unwrap_err()
        .kind,
        HistoryToolsErrorKind::InvalidRequest
    );
    assert_eq!(
        f.run(Request::Restore {
            name: "recovered".into(),
            commit_id: f.first.to_string(),
            expected_head: Some(f.merged.to_string())
        })
        .unwrap_err()
        .kind,
        HistoryToolsErrorKind::ConcurrentChange
    );
    f.run(Request::Restore {
        name: "recovered".into(),
        commit_id: f.first.to_string(),
        expected_head: None,
    })
    .unwrap();
    assert_eq!(
        f.repo
            .find_reference("HEAD")
            .unwrap()
            .symbolic_target()
            .unwrap(),
        Some("refs/heads/unborn")
    );
}

#[test]
fn oversized_annotated_tag_is_rejected_before_revision_content_loading() {
    let f = Fixture::new();
    f.repo
        .tag(
            "oversized-tag",
            &f.repo.find_object(f.first, None).unwrap(),
            &Signature::now("Fixture", "fixture@example.invalid").unwrap(),
            &"x".repeat(1024 * 1024 + 1),
            false,
        )
        .unwrap();
    let error = f
        .run(Request::Compare {
            left: "oversized-tag".into(),
            right: "HEAD".into(),
            path: None,
        })
        .unwrap_err();
    assert_eq!(error.kind, HistoryToolsErrorKind::Limit);
}

#[test]
fn initial_commit_can_be_compared_with_empty_tree_in_both_directions() {
    let f = Fixture::new();
    let ResultData::Comparison(view) = f
        .run(Request::Compare {
            left: ":empty".into(),
            right: f.first.to_string(),
            path: Some("note.txt".into()),
        })
        .unwrap()
    else {
        panic!("comparison expected")
    };
    assert_eq!(view.files.len(), 2);
    assert!(view.files.iter().all(|file| file.status == 'A'));
    let diff = view.diff.unwrap();
    assert!(diff.old_text.is_empty());
    assert_eq!(diff.new_text, "alpha\nbeta\n");
    let ResultData::Comparison(reverse) = f
        .run(Request::Compare {
            left: f.first.to_string(),
            right: ":empty".into(),
            path: Some("note.txt".into()),
        })
        .unwrap()
    else {
        panic!("comparison expected")
    };
    assert!(reverse.files.iter().all(|file| file.status == 'D'));
    assert!(reverse.diff.unwrap().new_text.is_empty());
}

#[test]
fn three_parent_stash_previews_track_staged_unstaged_and_untracked_without_applying() {
    let mut f = Fixture::new();
    fs::write(f.root.join("note.txt"), "staged text\n").unwrap();
    {
        let mut index = f.repo.index().unwrap();
        index.add_path(std::path::Path::new("note.txt")).unwrap();
        index.write().unwrap();
    }
    fs::write(f.root.join("note.txt"), "working text\n").unwrap();
    fs::write(f.root.join("draft.txt"), "untracked snapshot\n").unwrap();
    let stash = f
        .repo
        .stash_save(
            &Signature::now("Fixture", "fixture@example.invalid").unwrap(),
            "preview fixture",
            Some(git2::StashFlags::INCLUDE_UNTRACKED),
        )
        .unwrap();
    assert_eq!(f.repo.find_commit(stash).unwrap().parent_count(), 3);
    let index = fs::read(f.repo.path().join("index")).unwrap();
    let stash_ref = f.repo.refname_to_id("refs/stash").unwrap();
    fs::write(f.root.join("note.txt"), "later local content\n").unwrap();
    for (left, right, path, old, new) in [
        (
            format!("{stash}^1"),
            stash.to_string(),
            "note.txt",
            "alpha\nchanged\n",
            "working text\n",
        ),
        (
            format!("{stash}^1"),
            format!("{stash}^2"),
            "note.txt",
            "alpha\nchanged\n",
            "staged text\n",
        ),
        (
            format!("{stash}^2"),
            stash.to_string(),
            "note.txt",
            "staged text\n",
            "working text\n",
        ),
        (
            ":empty".into(),
            format!("{stash}^3"),
            "draft.txt",
            "",
            "untracked snapshot\n",
        ),
    ] {
        let ResultData::Comparison(view) = f
            .run(Request::Compare {
                left,
                right,
                path: Some(path.into()),
            })
            .unwrap()
        else {
            panic!("comparison expected")
        };
        let diff = view.diff.unwrap();
        assert_eq!(diff.old_text, old);
        assert_eq!(diff.new_text, new);
    }
    assert_eq!(fs::read(f.repo.path().join("index")).unwrap(), index);
    assert_eq!(f.repo.refname_to_id("refs/stash").unwrap(), stash_ref);
    assert_eq!(
        fs::read_to_string(f.root.join("note.txt")).unwrap(),
        "later local content\n"
    );
    assert!(!f.root.join("draft.txt").exists());
    assert_eq!(f.repo.head().unwrap().target(), Some(f.merged));
}
