//! Real libgit2 merge/rebase fixtures, confined to the test temporary directory.
#[allow(dead_code)]
#[path = "../src/checkout.rs"]
mod checkout;
pub use checkout::{CheckoutError, CheckoutErrorKind};
#[allow(dead_code)]
#[path = "../src/conflicts.rs"]
mod conflicts;

use conflicts::{
    ConflictAction, ConflictErrorKind, ConflictOperation, ConflictRequest, ConflictResolution,
};
use git2::{Oid, Repository, Signature, build::CheckoutBuilder};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    directory: PathBuf,
    root: PathBuf,
    ours: Oid,
    theirs: Oid,
}
impl Fixture {
    fn new(conflict: bool) -> Self {
        let directory = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "gitviews-conflicts-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let root = directory.join("中文 仓库");
        let repository = Repository::init(&root).unwrap();
        let mut config = repository.config().unwrap();
        config.set_str("user.name", "解决冲突测试").unwrap();
        config
            .set_str("user.email", "merge@example.invalid")
            .unwrap();
        config.set_bool("core.autocrlf", false).unwrap();
        let base = commit(
            &repository,
            None,
            &[
                ("note.txt", "base\n"),
                ("other.txt", "base other\n"),
                (".gitignore", "ignored.txt\n"),
            ],
        );
        let ours = commit(&repository, Some(base), &[("note.txt", "ours\n")]);
        let theirs = commit(
            &repository,
            Some(base),
            if conflict {
                &[("note.txt", "theirs\n")]
            } else {
                &[("other.txt", "theirs other\n")]
            },
        );
        repository
            .reference("refs/heads/main", ours, false, "fixture")
            .unwrap();
        repository
            .reference("refs/heads/feature", theirs, false, "fixture")
            .unwrap();
        repository.set_head("refs/heads/main").unwrap();
        repository
            .checkout_head(Some(CheckoutBuilder::new().force()))
            .unwrap();
        Self {
            directory,
            root,
            ours,
            theirs,
        }
    }
    fn repo(&self) -> Repository {
        Repository::open(&self.root).unwrap()
    }
    fn request(&self, action: ConflictAction) -> ConflictRequest {
        ConflictRequest {
            expected_head: self
                .repo()
                .head()
                .unwrap()
                .target()
                .map(|oid| oid.to_string()),
            action,
        }
    }
    fn run(
        &self,
        action: ConflictAction,
    ) -> Result<conflicts::ConflictOutcome, conflicts::ConflictError> {
        conflicts::execute(&self.root, &self.request(action))
    }
    fn merge(&self) {
        self.run(ConflictAction::Merge {
            target: "feature".into(),
        })
        .unwrap();
    }
    fn resolve(&self, resolution: ConflictResolution) {
        let versions = conflicts::versions(&self.root, "note.txt").unwrap();
        self.run(ConflictAction::Resolve {
            path: "note.txt".into(),
            token: versions.token,
            resolution,
        })
        .unwrap();
    }
    fn text(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap()
    }
    fn assert_unlocked(&self) {
        let repo = self.repo();
        for path in ["index.lock", "HEAD.lock", "refs/heads/main.lock"] {
            assert!(!repo.path().join(path).exists(), "leaked {path}");
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let parent = fs::canonicalize(std::env::temp_dir()).unwrap();
        assert!(self.directory.starts_with(parent));
        assert!(
            self.directory
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("gitviews-conflicts-")
        );
        let _ = fs::remove_dir_all(&self.directory);
    }
}
fn commit(repository: &Repository, parent: Option<Oid>, changes: &[(&str, &str)]) -> Oid {
    let previous = parent.map(|oid| repository.find_commit(oid).unwrap());
    let tree = previous.as_ref().map(|commit| commit.tree().unwrap());
    let mut builder = repository.treebuilder(tree.as_ref()).unwrap();
    for (path, text) in changes {
        builder
            .insert(*path, repository.blob(text.as_bytes()).unwrap(), 0o100644)
            .unwrap();
    }
    let tree = repository.find_tree(builder.write().unwrap()).unwrap();
    let signature = Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repository
        .commit(
            None,
            &signature,
            &signature,
            "fixture commit",
            &tree,
            &previous.iter().collect::<Vec<_>>(),
        )
        .unwrap()
}

#[test]
fn merge_lists_three_sides_and_can_resolve_ours_then_continue() {
    let f = Fixture::new(true);
    f.merge();
    let state = conflicts::state(&f.root).unwrap();
    assert_eq!(state.kind, ConflictOperation::Merge);
    assert!(!state.can_continue);
    assert!(state.can_abort);
    assert_eq!(state.files[0].path, "note.txt");
    let versions = conflicts::versions(&f.root, "note.txt").unwrap();
    assert_eq!(versions.base.as_deref(), Some("base\n"));
    assert_eq!(versions.ours.as_deref(), Some("ours\n"));
    assert_eq!(versions.theirs.as_deref(), Some("theirs\n"));
    let backup = state.backup_ref.unwrap();
    assert_eq!(f.repo().refname_to_id(&backup).unwrap(), f.ours);
    assert_eq!(
        f.run(ConflictAction::Continue).unwrap_err().kind,
        ConflictErrorKind::Unresolved
    );
    f.resolve(ConflictResolution::Ours);
    assert_eq!(f.text("note.txt"), "ours\n");
    f.run(ConflictAction::Continue).unwrap();
    let repository = f.repo();
    let head = repository.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.parent_count(), 2);
    assert_eq!(head.parent_id(0).unwrap(), f.ours);
    assert_eq!(head.parent_id(1).unwrap(), f.theirs);
    assert_eq!(repository.state(), git2::RepositoryState::Clean);
    assert_eq!(repository.refname_to_id(&backup).unwrap(), f.ours);
    f.assert_unlocked();
}

#[test]
fn edited_resolution_preserves_backup_and_rejects_stale_token() {
    let f = Fixture::new(true);
    f.merge();
    let versions = conflicts::versions(&f.root, "note.txt").unwrap();
    fs::write(f.root.join("note.txt"), "edited outside\n").unwrap();
    let error = f
        .run(ConflictAction::Resolve {
            path: "note.txt".into(),
            token: versions.token,
            resolution: ConflictResolution::Edited("result\n".into()),
        })
        .unwrap_err();
    assert_eq!(error.kind, ConflictErrorKind::ConcurrentChange);
    assert_eq!(f.text("note.txt"), "edited outside\n");
    f.resolve(ConflictResolution::Edited("合并结果\n".into()));
    assert_eq!(f.text("note.txt"), "合并结果\n");
    let backups = fs::read_dir(f.repo().path().join("gitviews-conflict-backups"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "blob")
        })
        .collect::<Vec<_>>();
    assert!(
        backups
            .iter()
            .any(|path| fs::read_to_string(path).ok().as_deref() == Some("edited outside\n"))
    );
    f.assert_unlocked();
}

#[test]
fn continue_refuses_working_edits_after_resolution_without_losing_them() {
    let f = Fixture::new(true);
    f.merge();
    f.resolve(ConflictResolution::Theirs);
    fs::write(f.root.join("note.txt"), "new unstaged work\n").unwrap();
    assert_eq!(
        f.run(ConflictAction::Continue).unwrap_err().kind,
        ConflictErrorKind::Dirty
    );
    assert_eq!(f.text("note.txt"), "new unstaged work\n");
    assert_eq!(f.repo().head().unwrap().target(), Some(f.ours));
    let repository = f.repo();
    let entry = repository
        .index()
        .unwrap()
        .get_path(Path::new("note.txt"), 0)
        .unwrap();
    assert_eq!(
        repository.find_blob(entry.id).unwrap().content(),
        b"theirs\n"
    );
    f.assert_unlocked();
}

#[test]
fn abort_restores_original_branch_and_keeps_resolution_backup() {
    let f = Fixture::new(true);
    f.merge();
    f.resolve(ConflictResolution::Edited("resolved and discard\n".into()));
    fs::write(f.root.join("unrelated-untracked.txt"), "retain\n").unwrap();
    f.run(ConflictAction::Abort).unwrap();
    assert_eq!(f.text("note.txt"), "ours\n");
    assert_eq!(f.text("unrelated-untracked.txt"), "retain\n");
    assert_eq!(f.repo().head().unwrap().target(), Some(f.ours));
    assert_eq!(f.repo().state(), git2::RepositoryState::Clean);
    assert!(!f.repo().index().unwrap().has_conflicts());
    f.assert_unlocked();
}

#[test]
fn dirty_and_untracked_start_are_rejected_without_creating_operation() {
    for path in ["note.txt", "untracked.txt"] {
        let f = Fixture::new(true);
        fs::write(f.root.join(path), "local\n").unwrap();
        assert_eq!(
            f.run(ConflictAction::Merge {
                target: "feature".into()
            })
            .unwrap_err()
            .kind,
            ConflictErrorKind::Dirty
        );
        assert_eq!(f.text(path), "local\n");
        assert_eq!(f.repo().state(), git2::RepositoryState::Clean);
        assert!(!f.repo().path().join("gitviews-conflict-operation").exists());
        f.assert_unlocked();
    }
}

#[test]
fn ignored_collision_is_preserved_before_merge_or_rebase() {
    for rebase in [false, true] {
        let f = Fixture::new(false);
        let repository = f.repo();
        let target = commit(
            &repository,
            Some(f.theirs),
            &[("ignored.txt", "tracked target\n")],
        );
        repository
            .reference("refs/heads/target", target, false, "fixture")
            .unwrap();
        fs::write(f.root.join("ignored.txt"), "local ignored\n").unwrap();
        let action = if rebase {
            ConflictAction::Rebase {
                onto: "target".into(),
            }
        } else {
            ConflictAction::Merge {
                target: "target".into(),
            }
        };
        assert_eq!(f.run(action).unwrap_err().kind, ConflictErrorKind::Dirty);
        assert_eq!(f.text("ignored.txt"), "local ignored\n");
        assert_eq!(repository.head().unwrap().target(), Some(f.ours));
        f.assert_unlocked();
    }
}

#[test]
fn nonconflicting_merge_prepares_and_continue_creates_merge_commit() {
    let f = Fixture::new(false);
    f.merge();
    let state = conflicts::state(&f.root).unwrap();
    assert!(state.files.is_empty());
    assert!(state.can_continue);
    assert_eq!(f.text("note.txt"), "ours\n");
    assert_eq!(f.text("other.txt"), "theirs other\n");
    f.run(ConflictAction::Continue).unwrap();
    assert_eq!(
        f.repo()
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .parent_count(),
        2
    );
    f.assert_unlocked();
}

#[test]
fn rebase_replays_clean_commits_and_retains_original_backup() {
    let f = Fixture::new(false);
    f.run(ConflictAction::Rebase {
        onto: "feature".into(),
    })
    .unwrap();
    let repository = f.repo();
    assert_eq!(repository.state(), git2::RepositoryState::Clean);
    assert_eq!(
        repository.head().unwrap().name().unwrap(),
        "refs/heads/main"
    );
    let head = repository.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.parent_id(0).unwrap(), f.theirs);
    assert_eq!(f.text("note.txt"), "ours\n");
    assert_eq!(f.text("other.txt"), "theirs other\n");
    assert!(
        repository
            .references_glob("refs/gitviews/backups/*")
            .unwrap()
            .any(|reference| reference.unwrap().target() == Some(f.ours))
    );
    f.assert_unlocked();
}

#[test]
fn rebase_conflict_can_choose_replayed_side_and_continue() {
    let f = Fixture::new(true);
    f.run(ConflictAction::Rebase {
        onto: "feature".into(),
    })
    .unwrap();
    let state = conflicts::state(&f.root).unwrap();
    assert_eq!(state.kind, ConflictOperation::Rebase);
    assert!(!state.can_continue);
    assert!(state.can_abort);
    let versions = conflicts::versions(&f.root, "note.txt").unwrap();
    assert_eq!(versions.ours.as_deref(), Some("theirs\n"));
    assert_eq!(versions.theirs.as_deref(), Some("ours\n"));
    f.resolve(ConflictResolution::Theirs);
    f.run(ConflictAction::Continue).unwrap();
    assert_eq!(f.text("note.txt"), "ours\n");
    let repository = f.repo();
    assert_eq!(repository.state(), git2::RepositoryState::Clean);
    assert_eq!(
        repository
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .parent_id(0)
            .unwrap(),
        f.theirs
    );
    f.assert_unlocked();
}

#[test]
fn rebase_abort_restores_original_head_and_files() {
    let f = Fixture::new(true);
    f.run(ConflictAction::Rebase {
        onto: "feature".into(),
    })
    .unwrap();
    f.resolve(ConflictResolution::Edited("discard resolution\n".into()));
    f.run(ConflictAction::Abort).unwrap();
    assert_eq!(f.repo().state(), git2::RepositoryState::Clean);
    assert_eq!(f.repo().head().unwrap().name().unwrap(), "refs/heads/main");
    assert_eq!(f.repo().head().unwrap().target(), Some(f.ours));
    assert_eq!(f.text("note.txt"), "ours\n");
    f.assert_unlocked();
}

#[test]
fn external_rebase_is_readable_but_continue_and_abort_are_refused() {
    let f = Fixture::new(true);
    let repository = f.repo();
    let onto = repository.find_annotated_commit(f.theirs).unwrap();
    let mut rebase = repository.rebase(None, Some(&onto), None, None).unwrap();
    rebase.next().unwrap().unwrap();
    drop(rebase);
    let state = conflicts::state(&f.root).unwrap();
    assert_eq!(state.kind, ConflictOperation::Unsupported);
    assert!(!state.can_continue);
    assert!(!state.can_abort);
    assert_eq!(
        f.run(ConflictAction::Abort).unwrap_err().kind,
        ConflictErrorKind::Unsupported
    );
    assert_eq!(
        f.run(ConflictAction::Continue).unwrap_err().kind,
        ConflictErrorKind::Unresolved
    );
    assert!(f.repo().index().unwrap().has_conflicts());
    f.assert_unlocked();
}

#[test]
fn resolving_a_deleted_side_removes_the_file_and_marks_it_resolved() {
    let f = Fixture::new(true);
    f.merge();
    let repository = f.repo();
    let mut index = repository.index().unwrap();
    let conflict = index.conflict_get(Path::new("note.txt")).unwrap();
    index.conflict_remove(Path::new("note.txt")).unwrap();
    // Preserve stages 1 and 2, leaving theirs deleted.
    for entry in [conflict.ancestor, conflict.our].into_iter().flatten() {
        index.add(&entry).unwrap();
    }
    index.write().unwrap();
    let versions = conflicts::versions(&f.root, "note.txt").unwrap();
    assert!(versions.theirs.is_none());
    f.resolve(ConflictResolution::Theirs);
    assert!(!f.root.join("note.txt").exists());
    assert!(!f.repo().index().unwrap().has_conflicts());
    f.assert_unlocked();
}

#[test]
fn readonly_working_file_and_index_do_not_discard_content_or_conflict_stages() {
    for readonly_index in [false, true] {
        let f = Fixture::new(true);
        f.merge();
        let versions = conflicts::versions(&f.root, "note.txt").unwrap();
        let target = if readonly_index {
            f.repo().path().join("index")
        } else {
            f.root.join("note.txt")
        };
        let original_permissions = fs::metadata(&target).unwrap().permissions();
        let before = fs::read(f.repo().path().join("index")).unwrap();
        let content = f.text("note.txt");
        let mut permissions = original_permissions.clone();
        permissions.set_readonly(true);
        fs::set_permissions(&target, permissions).unwrap();
        let result = f.run(ConflictAction::Resolve {
            path: "note.txt".into(),
            token: versions.token,
            resolution: ConflictResolution::Edited("new edited result\n".into()),
        });
        fs::set_permissions(&target, original_permissions).unwrap();
        assert!(result.is_err());
        assert_eq!(f.text("note.txt"), content);
        assert_eq!(fs::read(f.repo().path().join("index")).unwrap(), before);
        assert!(f.repo().index().unwrap().has_conflicts());
        f.assert_unlocked();
    }
}

#[test]
fn metadata_and_windows_alias_requests_cannot_reach_files() {
    let f = Fixture::new(true);
    f.merge();
    let index_before = fs::read(f.repo().path().join("index")).unwrap();
    let config_before = fs::read(f.repo().path().join("config")).unwrap();
    for path in [
        "../outside.txt",
        ".git/config",
        ".GiT/config",
        "note.txt/../../outside.txt",
        "note.txt\\..\\outside.txt",
    ] {
        assert!(conflicts::versions(&f.root, path).is_err());
    }
    if cfg!(windows) {
        for path in [
            "note.txt:stream",
            ".git./config",
            ".git /config",
            "note.txt.",
        ] {
            assert!(conflicts::versions(&f.root, path).is_err());
        }
    }
    assert_eq!(
        fs::read(f.repo().path().join("index")).unwrap(),
        index_before
    );
    assert_eq!(
        fs::read(f.repo().path().join("config")).unwrap(),
        config_before
    );
    f.assert_unlocked();
}

#[test]
fn rebase_advances_multiple_picks_with_fresh_index_transactions() {
    let f = Fixture::new(false);
    let repository = f.repo();
    let second = commit(
        &repository,
        Some(f.ours),
        &[("second.txt", "second commit\n")],
    );
    repository
        .reference("refs/heads/main", second, true, "fixture")
        .unwrap();
    repository
        .checkout_head(Some(CheckoutBuilder::new().force()))
        .unwrap();
    f.run(ConflictAction::Rebase {
        onto: "feature".into(),
    })
    .unwrap();
    let repository = f.repo();
    let head = repository.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(head.parent(0).unwrap().parent_id(0).unwrap(), f.theirs);
    assert_eq!(f.text("second.txt"), "second commit\n");
    assert_eq!(f.text("note.txt"), "ours\n");
    assert_eq!(f.text("other.txt"), "theirs other\n");
    assert_eq!(repository.state(), git2::RepositoryState::Clean);
    f.assert_unlocked();
}

#[test]
fn required_signatures_are_never_silently_skipped_for_merge_or_rebase() {
    for rebase in [false, true] {
        let f = Fixture::new(true);
        f.repo()
            .config()
            .unwrap()
            .set_bool("commit.gpgsign", true)
            .unwrap();
        let action = if rebase {
            ConflictAction::Rebase {
                onto: "feature".into(),
            }
        } else {
            ConflictAction::Merge {
                target: "feature".into(),
            }
        };
        assert_eq!(
            f.run(action).unwrap_err().kind,
            ConflictErrorKind::Unsupported
        );
        assert_eq!(f.repo().head().unwrap().target(), Some(f.ours));
        assert_eq!(f.repo().state(), git2::RepositoryState::Clean);
    }
    let f = Fixture::new(true);
    f.merge();
    f.resolve(ConflictResolution::Ours);
    f.repo()
        .config()
        .unwrap()
        .set_bool("commit.gpgsign", true)
        .unwrap();
    assert_eq!(
        f.run(ConflictAction::Continue).unwrap_err().kind,
        ConflictErrorKind::Unsupported
    );
    assert_eq!(f.repo().head().unwrap().target(), Some(f.ours));
    assert_eq!(f.repo().state(), git2::RepositoryState::Merge);
    f.assert_unlocked();
}

#[test]
fn abort_never_rewinds_an_original_branch_advanced_by_another_writer() {
    let f = Fixture::new(true);
    f.run(ConflictAction::Rebase {
        onto: "feature".into(),
    })
    .unwrap();
    let repository = f.repo();
    let external = commit(
        &repository,
        Some(f.ours),
        &[("external.txt", "concurrent commit\n")],
    );
    repository
        .reference("refs/heads/main", external, true, "external writer")
        .unwrap();
    let head_before = repository.head().unwrap().target();
    let index_before = fs::read(repository.path().join("index")).unwrap();
    let files_before = fs::read(f.root.join("note.txt")).unwrap();
    let error = f.run(ConflictAction::Abort).unwrap_err();
    assert_eq!(error.kind, ConflictErrorKind::ConcurrentChange);
    assert_eq!(
        repository.refname_to_id("refs/heads/main").unwrap(),
        external
    );
    assert_eq!(repository.head().unwrap().target(), head_before);
    assert_eq!(
        fs::read(repository.path().join("index")).unwrap(),
        index_before
    );
    assert_eq!(fs::read(f.root.join("note.txt")).unwrap(), files_before);
    assert_eq!(repository.state(), git2::RepositoryState::RebaseMerge);
    assert!(
        repository
            .references_glob("refs/gitviews/backups/*")
            .unwrap()
            .any(|reference| reference.unwrap().target() == Some(f.ours))
    );
    f.assert_unlocked();
}
