//! Native stash fixtures. No external Git executable or user repositories.
#[allow(dead_code)]
#[path = "../src/checkout.rs"]
mod checkout;
pub use checkout::{CheckoutError, CheckoutErrorKind};
#[allow(dead_code)]
#[path = "../src/stash.rs"]
mod stash;

use git2::{Oid, Repository, Signature, build::CheckoutBuilder};
use stash::{StashAction, StashErrorKind, StashRequest};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    directory: PathBuf,
    root: PathBuf,
    head: Oid,
}
impl Fixture {
    fn new() -> Self {
        let directory = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "gitviews-stash-{}-{}-{}",
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
        config.set_str("user.name", "储藏测试").unwrap();
        config
            .set_str("user.email", "stash@example.invalid")
            .unwrap();
        config.set_bool("core.autocrlf", false).unwrap();
        let mut builder = repository.treebuilder(None).unwrap();
        builder
            .insert("note.txt", repository.blob(b"base\n").unwrap(), 0o100644)
            .unwrap();
        builder
            .insert("other.txt", repository.blob(b"other\n").unwrap(), 0o100644)
            .unwrap();
        builder
            .insert(
                ".gitignore",
                repository.blob(b"ignored.txt\n").unwrap(),
                0o100644,
            )
            .unwrap();
        let tree_id = builder.write().unwrap();
        let tree = repository.find_tree(tree_id).unwrap();
        let signature = Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let head = repository
            .commit(Some("HEAD"), &signature, &signature, "base", &tree, &[])
            .unwrap();
        repository
            .checkout_head(Some(CheckoutBuilder::new().force()))
            .unwrap();
        Self {
            directory,
            root,
            head,
        }
    }
    fn request(&self, action: StashAction) -> StashRequest {
        StashRequest {
            expected_head: Some(self.head.to_string()),
            action,
        }
    }
    fn save(&self, message: &str, untracked: bool, ignored: bool) -> String {
        stash::execute(
            &self.root,
            &self.request(StashAction::Save {
                message: message.into(),
                include_untracked: untracked,
                include_ignored: ignored,
            }),
        )
        .unwrap();
        stash::list(&self.root).unwrap()[0].id.clone()
    }
    fn write(&self, path: &str, text: &str) {
        fs::write(self.root.join(path), text).unwrap();
    }
    fn text(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap()
    }
    fn assert_unlocked(&self) {
        let repo = Repository::open(&self.root).unwrap();
        for path in ["index.lock", "HEAD.lock", "refs/stash.lock"] {
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
                .starts_with("gitviews-stash-")
        );
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn save_and_apply_restore_distinct_staged_and_unstaged_versions() {
    let f = Fixture::new();
    f.write("note.txt", "staged\n");
    let repo = Repository::open(&f.root).unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("note.txt")).unwrap();
    index.write().unwrap();
    f.write("note.txt", "unstaged\n");
    f.write("untracked.txt", "keep untracked\n");
    f.write("ignored.txt", "keep ignored\n");
    let id = f.save("中文说明", false, false);
    assert_eq!(f.text("note.txt"), "base\n");
    assert_eq!(f.text("untracked.txt"), "keep untracked\n");
    assert_eq!(f.text("ignored.txt"), "keep ignored\n");
    stash::execute(
        &f.root,
        &f.request(StashAction::Apply {
            id: id.clone(),
            reinstate_index: true,
        }),
    )
    .unwrap();
    assert_eq!(f.text("note.txt"), "unstaged\n");
    let repo = Repository::open(&f.root).unwrap();
    let index = repo.index().unwrap();
    let blob = repo
        .find_blob(
            index
                .get_path(std::path::Path::new("note.txt"), 0)
                .unwrap()
                .id,
        )
        .unwrap();
    assert_eq!(blob.content(), b"staged\n");
    assert_eq!(stash::list(&f.root).unwrap()[0].id, id);
    f.assert_unlocked();
}

#[test]
fn opted_in_untracked_and_ignored_files_round_trip() {
    let f = Fixture::new();
    f.write("untracked.txt", "new\n");
    f.write("ignored.txt", "ignored\n");
    let id = f.save("all", true, true);
    assert!(!f.root.join("untracked.txt").exists());
    assert!(!f.root.join("ignored.txt").exists());
    stash::execute(
        &f.root,
        &f.request(StashAction::Pop {
            id,
            reinstate_index: true,
        }),
    )
    .unwrap();
    assert_eq!(f.text("untracked.txt"), "new\n");
    assert_eq!(f.text("ignored.txt"), "ignored\n");
    assert!(stash::list(&f.root).unwrap().is_empty());
    f.assert_unlocked();
}

#[test]
fn stable_oid_pop_survives_a_newer_stash() {
    let f = Fixture::new();
    f.write("note.txt", "first\n");
    let first = f.save("first", false, false);
    f.write("other.txt", "second\n");
    let second = f.save("second", false, false);
    stash::execute(
        &f.root,
        &f.request(StashAction::Pop {
            id: first,
            reinstate_index: true,
        }),
    )
    .unwrap();
    assert_eq!(f.text("note.txt"), "first\n");
    assert_eq!(f.text("other.txt"), "other\n");
    assert_eq!(
        stash::list(&f.root)
            .unwrap()
            .iter()
            .map(|e| e.id.clone())
            .collect::<Vec<_>>(),
        vec![second]
    );
    f.assert_unlocked();
}

#[test]
fn drop_targets_oid_and_does_not_change_worktree_or_index() {
    let f = Fixture::new();
    f.write("note.txt", "first\n");
    let first = f.save("first", false, false);
    f.write("note.txt", "second\n");
    let second = f.save("second", false, false);
    f.write("note.txt", "current\n");
    let index = fs::read(f.root.join(".git/index")).unwrap();
    stash::execute(&f.root, &f.request(StashAction::Drop { id: first.clone() })).unwrap();
    assert_eq!(f.text("note.txt"), "current\n");
    assert_eq!(fs::read(f.root.join(".git/index")).unwrap(), index);
    assert_eq!(stash::list(&f.root).unwrap()[0].id, second);
    assert_eq!(
        stash::execute(&f.root, &f.request(StashAction::Drop { id: first }))
            .unwrap_err()
            .kind,
        StashErrorKind::NotFound
    );
    assert_eq!(stash::list(&f.root).unwrap()[0].id, second);
    f.assert_unlocked();
}

#[test]
fn applying_over_local_conflicting_edits_preserves_files_and_stash() {
    let f = Fixture::new();
    f.write("note.txt", "stash\n");
    let id = f.save("stash", false, false);
    f.write("note.txt", "local\n");
    assert!(
        stash::execute(
            &f.root,
            &f.request(StashAction::Pop {
                id: id.clone(),
                reinstate_index: true
            })
        )
        .is_err()
    );
    assert_eq!(f.text("note.txt"), "local\n");
    assert_eq!(stash::list(&f.root).unwrap()[0].id, id);
    f.assert_unlocked();
}

#[test]
fn unrelated_worktree_edits_are_preserved_on_apply() {
    let f = Fixture::new();
    f.write("note.txt", "stash\n");
    let id = f.save("stash", false, false);
    f.write("other.txt", "local\n");
    stash::execute(
        &f.root,
        &f.request(StashAction::Apply {
            id,
            reinstate_index: false,
        }),
    )
    .unwrap();
    assert_eq!(f.text("note.txt"), "stash\n");
    assert_eq!(f.text("other.txt"), "local\n");
    f.assert_unlocked();
}

#[test]
fn collisions_with_untracked_files_are_preserved() {
    let f = Fixture::new();
    f.write("untracked.txt", "stash\n");
    let id = f.save("stash", true, false);
    f.write("untracked.txt", "local\n");
    assert!(
        stash::execute(
            &f.root,
            &f.request(StashAction::Pop {
                id: id.clone(),
                reinstate_index: true
            })
        )
        .is_err()
    );
    assert_eq!(f.text("untracked.txt"), "local\n");
    assert_eq!(stash::list(&f.root).unwrap()[0].id, id);
    f.assert_unlocked();
}

#[test]
fn stale_head_and_foreign_index_lock_block_without_modification() {
    let f = Fixture::new();
    f.write("note.txt", "local\n");
    let mut request = f.request(StashAction::Save {
        message: String::new(),
        include_untracked: false,
        include_ignored: false,
    });
    request.expected_head = Some(Oid::ZERO_SHA1.to_string());
    assert_eq!(
        stash::execute(&f.root, &request).unwrap_err().kind,
        StashErrorKind::ConcurrentChange
    );
    request.expected_head = Some(f.head.to_string());
    let lock = f.root.join(".git/index.lock");
    fs::write(&lock, b"foreign").unwrap();
    assert_eq!(
        stash::execute(&f.root, &request).unwrap_err().kind,
        StashErrorKind::ConcurrentChange
    );
    assert_eq!(fs::read(&lock).unwrap(), b"foreign");
    assert_eq!(f.text("note.txt"), "local\n");
    assert!(stash::list(&f.root).unwrap().is_empty());
}

#[test]
fn subdirectory_is_rejected_and_no_changes_is_non_destructive() {
    let f = Fixture::new();
    let sub = f.root.join("sub");
    fs::create_dir(&sub).unwrap();
    assert_eq!(
        stash::list(&sub).unwrap_err().kind,
        StashErrorKind::InvalidRepository
    );
    assert_eq!(
        stash::execute(
            &f.root,
            &f.request(StashAction::Save {
                message: String::new(),
                include_untracked: false,
                include_ignored: false
            })
        )
        .unwrap_err()
        .kind,
        StashErrorKind::NoChanges
    );
    assert_eq!(f.text("note.txt"), "base\n");
    f.assert_unlocked();
}

#[test]
fn in_progress_merge_is_rejected() {
    let f = Fixture::new();
    f.write("note.txt", "local\n");
    fs::write(f.root.join(".git/MERGE_HEAD"), format!("{}\n", f.head)).unwrap();
    let error = stash::execute(
        &f.root,
        &f.request(StashAction::Save {
            message: String::new(),
            include_untracked: false,
            include_ignored: false,
        }),
    )
    .unwrap_err();
    assert_eq!(error.kind, StashErrorKind::InProgress);
    assert_eq!(f.text("note.txt"), "local\n");
}

#[test]
fn merge_conflict_keeps_stash_and_publishes_conflict_index() {
    let f = Fixture::new();
    f.write("note.txt", "stash\n");
    let id = f.save("stash", false, false);
    let repo = Repository::open(&f.root).unwrap();
    let original = repo.find_commit(f.head).unwrap();
    let original_tree = original.tree().unwrap();
    let mut builder = repo.treebuilder(Some(&original_tree)).unwrap();
    builder
        .insert("note.txt", repo.blob(b"new head\n").unwrap(), 0o100644)
        .unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = repo.signature().unwrap();
    let head = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "later",
            &tree,
            &[&original],
        )
        .unwrap();
    repo.checkout_head(Some(CheckoutBuilder::new().force()))
        .unwrap();
    let request = StashRequest {
        expected_head: Some(head.to_string()),
        action: StashAction::Pop {
            id: id.clone(),
            reinstate_index: false,
        },
    };
    let error = stash::execute(&f.root, &request).unwrap_err();
    assert_eq!(error.kind, StashErrorKind::Conflict);
    assert!(
        Repository::open(&f.root)
            .unwrap()
            .index()
            .unwrap()
            .has_conflicts()
    );
    let text = f.text("note.txt");
    assert!(text.contains("<<<<<<<"));
    assert!(text.contains("stash"));
    assert!(text.contains("new head"));
    assert_eq!(stash::list(&f.root).unwrap()[0].id, id);
    f.assert_unlocked();
}

#[test]
fn linked_worktree_can_save_and_share_stash_list_without_changing_main_files() {
    let f = Fixture::new();
    let repo = Repository::open(&f.root).unwrap();
    let commit = repo.find_commit(f.head).unwrap();
    let branch = repo.branch("linked", &commit, false).unwrap();
    let linked = f.directory.join("linked");
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(branch.get()));
    repo.worktree("linked", &linked, Some(&options)).unwrap();
    fs::write(linked.join("note.txt"), "linked edits\n").unwrap();
    let request = f.request(StashAction::Save {
        message: "linked".into(),
        include_untracked: false,
        include_ignored: false,
    });
    stash::execute(&linked, &request).unwrap();
    assert_eq!(
        fs::read_to_string(linked.join("note.txt")).unwrap(),
        "base\n"
    );
    assert_eq!(f.text("note.txt"), "base\n");
    let main_entries = stash::list(&f.root).unwrap();
    assert_eq!(main_entries.len(), 1);
    assert_eq!(main_entries, stash::list(&linked).unwrap());
    stash::execute(
        &linked,
        &f.request(StashAction::Pop {
            id: main_entries[0].id.clone(),
            reinstate_index: true,
        }),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(linked.join("note.txt")).unwrap(),
        "linked edits\n"
    );
    assert_eq!(f.text("note.txt"), "base\n");
}

#[test]
fn hidden_index_flags_never_allow_stash_to_clean_unrecorded_edits() {
    for assume_unchanged in [true, false] {
        let f = Fixture::new();
        let repository = Repository::open(&f.root).unwrap();
        let mut index = repository.index().unwrap();
        let mut entry = index.get_path(std::path::Path::new("note.txt"), 0).unwrap();
        if assume_unchanged {
            entry.flags |= 0x8000;
        } else {
            entry.flags |= 0x4000;
            entry.flags_extended |= 0x4000;
        }
        index.add(&entry).unwrap();
        index.write().unwrap();
        f.write("note.txt", "hidden local edits\n");
        let index_before = fs::read(repository.path().join("index")).unwrap();
        let error = stash::execute(
            &f.root,
            &f.request(StashAction::Save {
                message: "unsafe".into(),
                include_untracked: false,
                include_ignored: false,
            }),
        )
        .unwrap_err();
        assert_eq!(error.kind, StashErrorKind::Unsupported);
        assert_eq!(f.text("note.txt"), "hidden local edits\n");
        assert_eq!(
            fs::read(repository.path().join("index")).unwrap(),
            index_before
        );
        assert!(stash::list(&f.root).unwrap().is_empty());
        f.assert_unlocked();
    }
}
