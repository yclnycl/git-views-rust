//! Multi-branch graph reads use native libgit2 and isolated, disposable fixtures.
use git2::{Oid, Repository, Signature, Time};
use gitviews_git::{
    Git2Backend, GitBackend, GraphHistoryFilter, GraphHistoryPage, GraphHistoryRequest,
    GraphReferenceKind, HistoryToolsErrorKind,
};
use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
const EPOCH: i64 = 1_700_000_000;

struct Fixture {
    base: PathBuf,
    root: PathBuf,
    repo: Repository,
    first: Oid,
    second: Oid,
    side: Oid,
    merged: Oid,
    feature: Oid,
    remote: Oid,
    backup: Oid,
    tag_only: Oid,
}
impl Fixture {
    fn new() -> Self {
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp");
        fs::create_dir_all(&base).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let root = base.join(format!(
            "gitviews-graph-history-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let repo = Repository::init(&root).unwrap();
        let first = commit(
            &repo,
            "initial",
            b"note original",
            b"other original",
            &[],
            0,
        );
        let second = commit(
            &repo,
            "change note",
            b"note changed",
            b"other original",
            &[first],
            1,
        );
        let side = commit(
            &repo,
            "side other",
            b"note original",
            b"other changed",
            &[first],
            2,
        );
        let merged = commit(
            &repo,
            "merge both",
            b"note changed",
            b"other changed",
            &[second, side],
            3,
        );
        let feature = commit(
            &repo,
            "feature-only\n\nsearchable body detail",
            b"feature",
            b"other original",
            &[first],
            4,
        );
        let remote = commit(
            &repo,
            "remote-only",
            b"remote",
            b"other original",
            &[first],
            5,
        );
        let backup = commit(
            &repo,
            "backup excluded",
            b"backup",
            b"other original",
            &[],
            6,
        );
        let tag_only = commit(
            &repo,
            "tag-only excluded",
            b"tag only",
            b"other original",
            &[],
            7,
        );
        for (name, target) in [
            ("refs/heads/main", merged),
            ("refs/heads/feature", feature),
            ("refs/heads/topic/nested", feature),
            ("refs/remotes/origin/review", remote),
            ("refs/gitviews/backups/hidden", backup),
        ] {
            repo.reference(name, target, true, "fixture").unwrap();
        }
        repo.reference_symbolic(
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/review",
            true,
            "fixture",
        )
        .unwrap();
        repo.set_head("refs/heads/main").unwrap();
        repo.tag(
            "v1",
            &repo.find_object(first, None).unwrap(),
            &signature(0),
            "annotated",
            false,
        )
        .unwrap();
        repo.reference("refs/tags/detached-tag", tag_only, false, "fixture")
            .unwrap();
        repo.checkout_head(None).unwrap();
        Self {
            base,
            root,
            repo,
            first,
            second,
            side,
            merged,
            feature,
            remote,
            backup,
            tag_only,
        }
    }
    fn graph(&self, request: GraphHistoryRequest) -> GraphHistoryPage {
        Git2Backend
            .graph_history(&self.root, &request, &AtomicBool::new(false))
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let root = fs::canonicalize(&self.root).unwrap();
        assert_eq!(root.parent(), Some(self.base.as_path()));
        assert!(
            root.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("gitviews-graph-history-")
        );
        let _ = fs::remove_dir_all(root);
    }
}
fn signature(offset: i64) -> Signature<'static> {
    Signature::new(
        "测试作者",
        "fixture@example.invalid",
        &Time::new(EPOCH + offset, 0),
    )
    .unwrap()
}
fn commit(
    repo: &Repository,
    subject: &str,
    note: &[u8],
    other: &[u8],
    parents: &[Oid],
    time: i64,
) -> Oid {
    let mut builder = repo.treebuilder(None).unwrap();
    for (name, bytes) in [("note.txt", note), ("other.txt", other)] {
        builder
            .insert(name, repo.blob(bytes).unwrap(), 0o100644)
            .unwrap();
    }
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let parents = parents
        .iter()
        .map(|id| repo.find_commit(*id).unwrap())
        .collect::<Vec<_>>();
    repo.commit(
        None,
        &signature(time),
        &signature(time),
        subject,
        &tree,
        &parents.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}
fn ids(page: &GraphHistoryPage) -> BTreeSet<String> {
    page.rows.iter().map(|row| row.commit.id.clone()).collect()
}

#[test]
fn graph_history_defaults_to_all_local_remote_and_head_preserving_merge_parents_and_labels() {
    let f = Fixture::new();
    let before_head = f.repo.head().unwrap().target();
    let before_index = fs::read(f.repo.path().join("index")).unwrap();
    let before_file = fs::read(f.root.join("note.txt")).unwrap();
    let page = f.graph(GraphHistoryRequest::default());
    assert_eq!(
        ids(&page),
        [f.first, f.second, f.side, f.merged, f.feature, f.remote]
            .map(|id| id.to_string())
            .into_iter()
            .collect()
    );
    assert!(!ids(&page).contains(&f.backup.to_string()));
    assert!(!ids(&page).contains(&f.tag_only.to_string()));
    assert_eq!(page.snapshot.tips.len(), 3);
    assert!(page.rows.iter().all(|row| row.matches_filter));
    assert!(!page.has_more);
    let merge = page
        .rows
        .iter()
        .find(|row| row.commit.id == f.merged.to_string())
        .unwrap();
    assert_eq!(
        merge.commit.parents,
        vec![f.second.to_string(), f.side.to_string()]
    );
    let position = |id: Oid| {
        page.rows
            .iter()
            .position(|row| row.commit.id == id.to_string())
            .unwrap()
    };
    assert!(position(f.merged) < position(f.second));
    assert!(position(f.merged) < position(f.side));
    assert!(
        page.snapshot
            .references
            .iter()
            .any(|label| label.full_name == "refs/tags/v1"
                && label.target == f.first.to_string()
                && label.kind == GraphReferenceKind::Tag)
    );
    assert!(
        page.snapshot
            .references
            .iter()
            .any(|label| label.full_name == "refs/heads/main" && label.is_current)
    );
    assert!(
        page.snapshot
            .references
            .iter()
            .any(|label| label.full_name == "refs/heads/topic/nested")
    );
    assert!(
        page.snapshot
            .references
            .iter()
            .any(|label| label.full_name == "HEAD" && label.target == f.merged.to_string())
    );
    assert!(
        !page
            .snapshot
            .references
            .iter()
            .any(|label| label.full_name.contains("backups")
                || label.full_name == "refs/remotes/origin/HEAD")
    );
    assert_eq!(f.repo.head().unwrap().target(), before_head);
    assert_eq!(fs::read(f.repo.path().join("index")).unwrap(), before_index);
    assert_eq!(fs::read(f.root.join("note.txt")).unwrap(), before_file);
}

#[test]
fn graph_history_selected_multiple_branches_form_a_deduplicated_union() {
    let f = Fixture::new();
    let page = f.graph(GraphHistoryRequest {
        references: vec![
            "refs/heads/feature".into(),
            "refs/remotes/origin/review".into(),
            "refs/heads/feature".into(),
        ],
        ..Default::default()
    });
    assert_eq!(
        ids(&page),
        [f.feature, f.remote, f.first]
            .map(|id| id.to_string())
            .into_iter()
            .collect()
    );
    assert_eq!(page.rows.len(), 3);
    assert_eq!(page.snapshot.tips.len(), 2);
    // All labels remain available to the branch selector, even for unselected tips.
    assert!(
        page.snapshot
            .references
            .iter()
            .any(|reference| reference.full_name == "refs/heads/main")
    );
    let tag = f.graph(GraphHistoryRequest {
        references: vec!["refs/tags/detached-tag".into()],
        ..Default::default()
    });
    assert_eq!(ids(&tag), [f.tag_only.to_string()].into_iter().collect());
}

#[test]
fn graph_history_pagination_freezes_tips_and_labels_across_reference_moves_and_new_branches() {
    let f = Fixture::new();
    let expected = f.graph(GraphHistoryRequest::default());
    let first = f.graph(GraphHistoryRequest {
        limit: 2,
        ..Default::default()
    });
    assert!(first.has_more);
    let next = commit(
        &f.repo,
        "external new commit",
        b"later",
        b"other original",
        &[f.merged],
        20,
    );
    f.repo
        .reference("refs/heads/main", next, true, "external")
        .unwrap();
    f.repo
        .reference("refs/heads/new-branch", f.backup, true, "external")
        .unwrap();
    f.repo
        .reference("refs/tags/v1", next, true, "external")
        .unwrap();
    let mut combined = first.rows.clone();
    let snapshot = first.snapshot.clone();
    let mut offset = first.next_offset;
    loop {
        let page = f.graph(GraphHistoryRequest {
            snapshot: Some(snapshot.clone()),
            offset,
            limit: 2,
            ..Default::default()
        });
        assert_eq!(page.snapshot, snapshot);
        offset = page.next_offset;
        combined.extend(page.rows);
        if !page.has_more {
            break;
        }
    }
    assert_eq!(
        combined
            .iter()
            .map(|row| &row.commit.id)
            .collect::<Vec<_>>(),
        expected
            .rows
            .iter()
            .map(|row| &row.commit.id)
            .collect::<Vec<_>>()
    );
    assert_eq!(combined.len(), ids(&expected).len());
    assert!(!combined.iter().any(|row| row.commit.id == next.to_string()));
}

#[test]
fn graph_history_filters_mark_matches_and_keep_all_connecting_rows_and_parent_ids() {
    let f = Fixture::new();
    let baseline = f.graph(GraphHistoryRequest::default());
    let text = f.graph(GraphHistoryRequest {
        filter: GraphHistoryFilter {
            text: "SEARCHABLE BODY".into(),
            author: "FIXTURE@".into(),
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(ids(&text), ids(&baseline));
    assert_eq!(
        text.rows
            .iter()
            .filter(|row| row.matches_filter)
            .map(|row| row.commit.id.clone())
            .collect::<Vec<_>>(),
        vec![f.feature.to_string()]
    );
    let path = f.graph(GraphHistoryRequest {
        filter: GraphHistoryFilter {
            path: Some("note.txt".into()),
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(ids(&path), ids(&baseline));
    assert!(
        !path
            .rows
            .iter()
            .find(|row| row.commit.id == f.side.to_string())
            .unwrap()
            .matches_filter
    );
    assert!(
        path.rows
            .iter()
            .find(|row| row.commit.id == f.merged.to_string())
            .unwrap()
            .matches_filter
    );
    let dates = f.graph(GraphHistoryRequest {
        filter: GraphHistoryFilter {
            since: Some(EPOCH + 2),
            until: Some(EPOCH + 3),
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(
        dates
            .rows
            .iter()
            .filter(|row| row.matches_filter)
            .map(|row| row.commit.id.clone())
            .collect::<BTreeSet<_>>(),
        [f.side.to_string(), f.merged.to_string()]
            .into_iter()
            .collect()
    );
}

#[test]
fn graph_history_path_filter_handles_binary_deletion_modes_symlinks_gitlinks_and_directories_without_reading_worktree()
 {
    let f = Fixture::new();
    let text = f.repo.blob(b"text").unwrap();
    let binary = f.repo.blob(&[0, 255, 1]).unwrap();
    let symlink = f.repo.blob(b"../external-target").unwrap();
    let mut subtree = f.repo.treebuilder(None).unwrap();
    subtree.insert("nested.txt", text, 0o100644).unwrap();
    let directory = subtree.write().unwrap();
    let entries = [
        None,
        Some((text, 0o100644)),
        Some((binary, 0o100644)),
        Some((binary, 0o100755)),
        Some((symlink, 0o120000)),
        Some((f.first, 0o160000)),
        Some((directory, 0o040000)),
        None,
    ];
    let mut previous = None;
    let mut committed = Vec::new();
    for (index, entry) in entries.into_iter().enumerate() {
        let mut builder = f.repo.treebuilder(None).unwrap();
        if let Some((id, mode)) = entry {
            builder.insert("state", id, mode).unwrap();
        }
        let tree = f.repo.find_tree(builder.write().unwrap()).unwrap();
        let parent = previous.map(|id| f.repo.find_commit(id).unwrap());
        let parents = parent.iter().collect::<Vec<_>>();
        let id = f
            .repo
            .commit(
                None,
                &signature(index as i64),
                &signature(index as i64),
                &format!("type {index}"),
                &tree,
                &parents,
            )
            .unwrap();
        committed.push(id.to_string());
        previous = Some(id);
    }
    f.repo
        .reference("refs/heads/types", previous.unwrap(), false, "fixture")
        .unwrap();
    let page = f.graph(GraphHistoryRequest {
        references: vec!["refs/heads/types".into()],
        filter: GraphHistoryFilter {
            path: Some("state".into()),
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(page.rows.len(), 8);
    assert_eq!(
        page.rows
            .iter()
            .filter(|row| row.matches_filter)
            .map(|row| row.commit.id.clone())
            .collect::<BTreeSet<_>>(),
        committed[1..].iter().cloned().collect()
    );
    let nested = f.graph(GraphHistoryRequest {
        references: vec!["refs/heads/types".into()],
        filter: GraphHistoryFilter {
            path: Some("state/nested.txt".into()),
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(
        nested
            .rows
            .iter()
            .filter(|row| row.matches_filter)
            .map(|row| row.commit.id.clone())
            .collect::<BTreeSet<_>>(),
        committed[6..].iter().cloned().collect()
    );
    assert!(!f.root.join("state").exists());
}

#[test]
fn graph_history_supports_detached_head_and_unborn_repositories() {
    let f = Fixture::new();
    f.repo.set_head_detached(f.backup).unwrap();
    let page = f.graph(GraphHistoryRequest::default());
    assert!(ids(&page).contains(&f.backup.to_string()));
    assert!(
        page.snapshot
            .references
            .iter()
            .find(|reference| reference.full_name == "HEAD")
            .unwrap()
            .is_current
    );
    assert!(
        !page
            .snapshot
            .references
            .iter()
            .any(
                |reference| reference.kind == GraphReferenceKind::LocalBranch
                    && reference.is_current
            )
    );
    let child = f.root.join("unborn-child");
    let _child_repo = Repository::init(&child).unwrap();
    let empty = Git2Backend
        .graph_history(
            &child,
            &GraphHistoryRequest::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(empty.rows.is_empty());
    assert!(empty.snapshot.tips.is_empty());
    assert!(!empty.has_more);
}

#[test]
fn graph_history_rejects_stale_root_cancelled_invalid_and_out_of_budget_requests() {
    let f = Fixture::new();
    let page = f.graph(GraphHistoryRequest::default());
    let child = f.root.join("nested-child");
    let child_repo = Repository::init(&child).unwrap();
    let error = Git2Backend
        .graph_history(
            &child,
            &GraphHistoryRequest {
                snapshot: Some(page.snapshot.clone()),
                ..Default::default()
            },
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert_eq!(error.kind, HistoryToolsErrorKind::InvalidRequest);
    drop(child_repo);
    // Renaming only this fixture's metadata makes it a parent-owned directory.
    let fixture_child = fs::canonicalize(&child).unwrap();
    assert_eq!(
        fixture_child.parent(),
        Some(fs::canonicalize(&f.root).unwrap().as_path())
    );
    fs::rename(
        fixture_child.join(".git"),
        fixture_child.join(".saved-fixture-git"),
    )
    .unwrap();
    let error = Git2Backend
        .graph_history(
            &child,
            &GraphHistoryRequest::default(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
    assert_eq!(error.kind, HistoryToolsErrorKind::InvalidRepository);
    assert_eq!(
        Git2Backend
            .graph_history(
                &f.root,
                &GraphHistoryRequest::default(),
                &AtomicBool::new(true)
            )
            .unwrap_err()
            .kind,
        HistoryToolsErrorKind::Cancelled
    );
    for request in [
        GraphHistoryRequest {
            limit: 201,
            ..Default::default()
        },
        GraphHistoryRequest {
            offset: 50_001,
            ..Default::default()
        },
        GraphHistoryRequest {
            references: vec!["refs/gitviews/backups/hidden".into()],
            ..Default::default()
        },
        GraphHistoryRequest {
            references: vec!["refs/heads/missing".into()],
            ..Default::default()
        },
        GraphHistoryRequest {
            filter: GraphHistoryFilter {
                path: Some("../outside".into()),
                ..Default::default()
            },
            ..Default::default()
        },
        GraphHistoryRequest {
            filter: GraphHistoryFilter {
                since: Some(5),
                until: Some(4),
                ..Default::default()
            },
            ..Default::default()
        },
    ] {
        assert_eq!(
            Git2Backend
                .graph_history(&f.root, &request, &AtomicBool::new(false))
                .unwrap_err()
                .kind,
            HistoryToolsErrorKind::InvalidRequest
        );
    }
    let mut snapshot = page.snapshot;
    snapshot.tips = vec!["main".into()];
    assert_eq!(
        Git2Backend
            .graph_history(
                &f.root,
                &GraphHistoryRequest {
                    snapshot: Some(snapshot),
                    ..Default::default()
                },
                &AtomicBool::new(false)
            )
            .unwrap_err()
            .kind,
        HistoryToolsErrorKind::InvalidRequest
    );
}
