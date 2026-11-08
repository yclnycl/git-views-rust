//! Real local libgit2 fixtures. Worktree bytes are asserted across every write.
#[allow(dead_code)]
#[path = "../src/checkout.rs"]
mod checkout;
#[allow(dead_code)]
#[path = "../src/write_ops.rs"]
mod write_ops;

use git2::{DiffOptions, Oid, Patch, Repository, Signature};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use write_ops::{WriteAction, WriteErrorKind, WriteRequest};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    parent: PathBuf,
    directory: PathBuf,
    root: PathBuf,
    repo: Repository,
}
impl Fixture {
    fn new(unborn: bool) -> Self {
        let parent = fs::canonicalize(std::env::temp_dir()).unwrap();
        let directory = parent.join(format!(
            "gitviews-write-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let root = directory.join("中文 仓库");
        let repo = Repository::init(&root).unwrap();
        {
            let mut config = repo.config().unwrap();
            config.set_bool("core.autocrlf", false).unwrap();
            config.set_bool("commit.gpgsign", false).unwrap();
            config.set_str("user.name", "Fixture Author").unwrap();
            config
                .set_str("user.email", "fixture@example.invalid")
                .unwrap();
        }
        let fixture = Self {
            parent,
            directory,
            root,
            repo,
        };
        if !unborn {
            fixture.write("note.txt", &base_text());
            fixture.write("other.txt", "other\n");
            fixture.write(".gitignore", "ignored.txt\n");
            let mut index = fixture.repo.index().unwrap();
            for path in ["note.txt", "other.txt", ".gitignore"] {
                index.add_path(Path::new(path)).unwrap();
            }
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = fixture.repo.find_tree(tree_id).unwrap();
            let signature = Signature::now("Fixture Author", "fixture@example.invalid").unwrap();
            fixture
                .repo
                .commit(Some("HEAD"), &signature, &signature, "base", &tree, &[])
                .unwrap();
        }
        fixture
    }
    fn write(&self, path: &str, text: &str) {
        fs::write(self.root.join(path), text).unwrap();
    }
    fn read(&self, path: &str) -> Vec<u8> {
        fs::read(self.root.join(path)).unwrap()
    }
    fn head(&self) -> Option<String> {
        Repository::open(&self.root)
            .unwrap()
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|oid| oid.to_string())
    }
    fn request(&self, action: WriteAction) -> WriteRequest {
        WriteRequest {
            expected_head: self.head(),
            action,
        }
    }
    fn run(&self, action: WriteAction) -> write_ops::WriteOutcome {
        write_ops::execute(&self.root, &self.request(action.clone()))
            .unwrap_or_else(|error| panic!("{action:?}: {error:?}"))
    }
    fn fail(&self, action: WriteAction) -> write_ops::WriteError {
        write_ops::execute(&self.root, &self.request(action)).unwrap_err()
    }
    fn blob(&self, path: &str) -> Option<Vec<u8>> {
        let repo = Repository::open(&self.root).unwrap();
        let index = repo.index().unwrap();
        index
            .get_path(Path::new(path), 0)
            .map(|entry| repo.find_blob(entry.id).unwrap().content().to_vec())
    }
    fn index_bytes(&self) -> Option<Vec<u8>> {
        fs::read(self.repo.path().join("index")).ok()
    }
    fn patch(&self, path: &str, staged: bool) -> String {
        let repo = Repository::open(&self.root).unwrap();
        let index = repo.index().unwrap();
        let tree = repo.head().ok().and_then(|head| head.peel_to_tree().ok());
        let mut options = DiffOptions::new();
        options
            .pathspec(path)
            .disable_pathspec_match(true)
            .context_lines(3)
            .update_index(false)
            .include_typechange(true)
            .include_untracked(true)
            .recurse_untracked_dirs(true)
            .show_untracked_content(true);
        let comparison = if staged {
            repo.diff_tree_to_index(tree.as_ref(), Some(&index), Some(&mut options))
        } else {
            repo.diff_index_to_workdir(Some(&index), Some(&mut options))
        }
        .unwrap();
        let mut patch = Patch::from_diff(&comparison, 0).unwrap().unwrap();
        String::from_utf8(patch.to_buf().unwrap().to_vec()).unwrap()
    }
    fn clean_leases(&self) {
        assert!(!self.repo.path().join("index.lock").exists());
        assert!(!self.repo.path().join("HEAD.lock").exists());
        assert!(fs::read_dir(self.repo.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".gitviews-index-")
        }));
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.directory.parent() == Some(self.parent.as_path())
            && self
                .directory
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("gitviews-write-")
        {
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
fn first_hunk(patch: &str) -> String {
    let positions: Vec<_> = patch
        .match_indices("\n@@ ")
        .map(|(offset, _)| offset + 1)
        .collect();
    assert!(
        positions.len() >= 2,
        "fixture needs two distinct hunks: {patch}"
    );
    patch[..positions[1]].to_owned()
}

#[test]
fn stage_and_unstage_files_preserve_worktree() {
    let fixture = Fixture::new(false);
    fixture.write("note.txt", "local edited\n");
    fixture.write("新文件.txt", "新增内容\n");
    let before = fixture.read("note.txt");
    fixture.run(WriteAction::StagePaths {
        paths: vec!["note.txt".into(), "新文件.txt".into()],
    });
    assert_eq!(fixture.blob("note.txt"), Some(before.clone()));
    assert_eq!(
        fixture.blob("新文件.txt"),
        Some("新增内容\n".as_bytes().to_vec())
    );
    fixture.run(WriteAction::UnstagePaths {
        paths: vec!["note.txt".into(), "新文件.txt".into()],
    });
    assert_eq!(fixture.blob("note.txt"), Some(base_text().into_bytes()));
    assert_eq!(fixture.blob("新文件.txt"), None);
    assert_eq!(fixture.read("note.txt"), before);
    assert_eq!(fixture.read("新文件.txt"), "新增内容\n".as_bytes());
    fixture.clean_leases();
}

#[test]
fn stage_all_handles_new_modified_deleted_and_ignores_ignored_files() {
    let fixture = Fixture::new(false);
    fixture.write("note.txt", "changed\n");
    fixture.write("new.txt", "new\n");
    fixture.write("ignored.txt", "private\n");
    fs::remove_file(fixture.root.join("other.txt")).unwrap();
    fixture.run(WriteAction::StageAll);
    assert_eq!(fixture.blob("note.txt"), Some(b"changed\n".to_vec()));
    assert_eq!(fixture.blob("new.txt"), Some(b"new\n".to_vec()));
    assert_eq!(fixture.blob("other.txt"), None);
    assert_eq!(fixture.blob("ignored.txt"), None);
    fixture.run(WriteAction::UnstageAll);
    assert_eq!(fixture.blob("note.txt"), Some(base_text().into_bytes()));
    assert_eq!(fixture.blob("other.txt"), Some(b"other\n".to_vec()));
    assert_eq!(fixture.blob("new.txt"), None);
    assert_eq!(fixture.read("note.txt"), b"changed\n");
    assert_eq!(fixture.read("ignored.txt"), b"private\n");
    assert!(!fixture.root.join("other.txt").exists());
    fixture.clean_leases();
}

#[test]
fn staging_combined_statuses_and_intent_to_add_preserves_every_working_file() {
    let fixture = Fixture::new(false);
    for path in ["mm.txt", "md.txt", "cached.txt", "before.txt"] {
        fixture.write(path, &format!("original {path}\n"));
    }
    fixture.run(WriteAction::StageAll);
    fixture.run(WriteAction::Commit {
        summary: "status matrix base".into(),
        description: String::new(),
    });
    for path in ["mm.txt", "md.txt", "am.txt", "ad.txt"] {
        fixture.write(path, &format!("staged {path}\n"));
    }
    fixture.run(WriteAction::StagePaths {
        paths: ["mm.txt", "md.txt", "am.txt", "ad.txt"]
            .map(str::to_owned)
            .to_vec(),
    });
    fixture.write("mm.txt", "last working modification\n");
    fixture.write("am.txt", "last new modification\n");
    fs::remove_file(fixture.root.join("md.txt")).unwrap();
    fs::remove_file(fixture.root.join("ad.txt")).unwrap();
    fs::rename(
        fixture.root.join("before.txt"),
        fixture.root.join("after.txt"),
    )
    .unwrap();
    fixture.write("ita.txt", "intent body\n");
    fixture.write("empty_ita.txt", "");
    {
        let mut index = fixture.repo.index().unwrap();
        index.remove_path(Path::new("cached.txt")).unwrap();
        index.remove_path(Path::new("before.txt")).unwrap();
        index.add_path(Path::new("after.txt")).unwrap();
        for path in ["ita.txt", "empty_ita.txt"] {
            index
                .add(&git2::IndexEntry {
                    ctime: git2::IndexTime::new(0, 0),
                    mtime: git2::IndexTime::new(0, 0),
                    dev: 0,
                    ino: 0,
                    mode: 0o100644,
                    uid: 0,
                    gid: 0,
                    file_size: 0,
                    id: fixture.repo.blob(b"").unwrap(),
                    flags: 0x4000,
                    flags_extended: 0x2000,
                    path: path.as_bytes().to_vec(),
                })
                .unwrap();
        }
        index.write().unwrap();
    }
    let paths = [
        "mm.txt",
        "md.txt",
        "am.txt",
        "ad.txt",
        "cached.txt",
        "before.txt",
        "after.txt",
        "ita.txt",
        "empty_ita.txt",
    ];
    let before: Vec<_> = paths
        .iter()
        .map(|path| fs::read(fixture.root.join(path)).ok())
        .collect();
    fixture.run(WriteAction::StageAll);
    for (path, bytes) in paths.iter().zip(&before) {
        assert_eq!(&fixture.blob(path), bytes, "staged {path}");
        assert_eq!(
            &fs::read(fixture.root.join(path)).ok(),
            bytes,
            "working {path}"
        );
    }
    // Writes open their own Repository; do not inspect this fixture's cached
    // in-memory index left over from constructing the intent-to-add entries.
    let reopened = Repository::open(&fixture.root).unwrap();
    let index = reopened.index().unwrap();
    for path in ["ita.txt", "empty_ita.txt"] {
        assert_eq!(
            index.get_path(Path::new(path), 0).unwrap().flags_extended & 0x2000,
            0
        );
    }
    fixture.run(WriteAction::UnstageAll);
    for (path, bytes) in paths.iter().zip(&before) {
        assert_eq!(
            &fs::read(fixture.root.join(path)).ok(),
            bytes,
            "unstage working {path}"
        );
    }
    assert_eq!(
        fixture.blob("before.txt"),
        Some(b"original before.txt\n".to_vec())
    );
    assert_eq!(fixture.blob("after.txt"), None);
    assert_eq!(
        fixture.blob("cached.txt"),
        Some(b"original cached.txt\n".to_vec())
    );
    assert_eq!(fixture.blob("ita.txt"), None);
    fixture.clean_leases();
}

#[test]
fn stage_paths_replaces_empty_intent_to_add_without_touching_unselected_placeholders() {
    let fixture = Fixture::new(false);
    for path in ["selected.txt", "unselected.txt"] {
        fixture.write(path, "");
    }
    {
        let mut index = fixture.repo.index().unwrap();
        for path in ["selected.txt", "unselected.txt"] {
            index
                .add(&git2::IndexEntry {
                    ctime: git2::IndexTime::new(0, 0),
                    mtime: git2::IndexTime::new(0, 0),
                    dev: 0,
                    ino: 0,
                    mode: 0o100644,
                    uid: 0,
                    gid: 0,
                    file_size: 0,
                    id: fixture.repo.blob(b"").unwrap(),
                    flags: 0x4000,
                    flags_extended: 0x2000,
                    path: path.as_bytes().to_vec(),
                })
                .unwrap();
        }
        index.write().unwrap();
    }
    fixture.run(WriteAction::StagePaths {
        paths: vec!["selected.txt".into()],
    });
    let repository = Repository::open(&fixture.root).unwrap();
    let index = repository.index().unwrap();
    assert_eq!(
        index
            .get_path(Path::new("selected.txt"), 0)
            .unwrap()
            .flags_extended
            & 0x2000,
        0
    );
    assert_eq!(
        index
            .get_path(Path::new("unselected.txt"), 0)
            .unwrap()
            .flags_extended
            & 0x2000,
        0x2000
    );
    assert_eq!(fixture.read("selected.txt"), b"");
    assert_eq!(fixture.read("unselected.txt"), b"");
    assert_eq!(fixture.blob("note.txt"), Some(base_text().into_bytes()));
    fixture.clean_leases();
}

#[test]
fn commit_omits_unstaged_intent_to_add_and_preserves_its_real_index_flags() {
    let fixture = Fixture::new(false);
    {
        let mut index = fixture.repo.index().unwrap();
        for (path, content) in [("empty_ita.txt", ""), ("body_ita.txt", "not staged\n")] {
            fixture.write(path, content);
            index
                .add(&git2::IndexEntry {
                    ctime: git2::IndexTime::new(0, 0),
                    mtime: git2::IndexTime::new(0, 0),
                    dev: 0,
                    ino: 0,
                    mode: 0o100644,
                    uid: 0,
                    gid: 0,
                    file_size: 0,
                    id: fixture.repo.blob(b"").unwrap(),
                    flags: 0x4000,
                    flags_extended: 0x2000,
                    path: path.as_bytes().to_vec(),
                })
                .unwrap();
        }
        index.write().unwrap();
    }
    let before = fixture.index_bytes();
    assert_eq!(
        fixture
            .fail(WriteAction::Commit {
                summary: "no staged content".into(),
                description: String::new()
            })
            .kind,
        WriteErrorKind::NothingToCommit
    );
    assert_eq!(fixture.index_bytes(), before);
    fixture.write("note.txt", "actually staged\n");
    fixture.run(WriteAction::StagePaths {
        paths: vec!["note.txt".into()],
    });
    let before = fixture.index_bytes();
    fixture.run(WriteAction::Commit {
        summary: "only staged content".into(),
        description: String::new(),
    });
    assert_eq!(fixture.index_bytes(), before);
    let repository = Repository::open(&fixture.root).unwrap();
    let tree = repository.head().unwrap().peel_to_tree().unwrap();
    assert!(tree.get_path(Path::new("empty_ita.txt")).is_err());
    assert!(tree.get_path(Path::new("body_ita.txt")).is_err());
    for path in ["empty_ita.txt", "body_ita.txt"] {
        assert_eq!(
            repository
                .index()
                .unwrap()
                .get_path(Path::new(path), 0)
                .unwrap()
                .flags_extended
                & 0x2000,
            0x2000
        );
    }
    assert_eq!(fixture.read("empty_ita.txt"), b"");
    assert_eq!(fixture.read("body_ita.txt"), b"not staged\n");
    fixture.clean_leases();
}

#[test]
fn first_commit_works_and_unstage_all_keeps_unborn_files() {
    let fixture = Fixture::new(true);
    fixture.write("first.txt", "initial\n");
    fixture.run(WriteAction::StageAll);
    fixture.run(WriteAction::UnstageAll);
    assert_eq!(fixture.blob("first.txt"), None);
    assert_eq!(fixture.read("first.txt"), b"initial\n");
    fixture.run(WriteAction::StageAll);
    let outcome = fixture.run(WriteAction::Commit {
        summary: "初次提交".into(),
        description: "详情\n第二行".into(),
    });
    let repo = Repository::open(&fixture.root).unwrap();
    let commit = repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(commit.parent_count(), 0);
    assert_eq!(commit.message().unwrap(), "初次提交\n\n详情\n第二行");
    assert_eq!(outcome.commit_id.as_deref(), fixture.head().as_deref());
    assert_eq!(commit.author().name().unwrap(), "Fixture Author");
    assert_eq!(
        repo.reflog("HEAD").unwrap().get(0).unwrap().id_new(),
        commit.id()
    );
    fixture.clean_leases();
}

#[test]
fn commit_uses_only_index_and_preserves_subsequent_edits_and_untracked() {
    let fixture = Fixture::new(false);
    let reflog_before = fixture.repo.reflog("HEAD").unwrap().len();
    let previous = fixture.head();
    fixture.write("note.txt", "staged version\n");
    fixture.run(WriteAction::StagePaths {
        paths: vec!["note.txt".into()],
    });
    let index_before = fixture.index_bytes();
    fixture.write("note.txt", "later unstaged version\n");
    fixture.write("private.txt", "untracked\n");
    fixture.run(WriteAction::Commit {
        summary: "commit staged only".into(),
        description: String::new(),
    });
    let repo = Repository::open(&fixture.root).unwrap();
    let commit = repo.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(commit.parent_id(0).unwrap().to_string(), previous.unwrap());
    assert_eq!(
        repo.reflog("HEAD").unwrap().get(0).unwrap().id_new(),
        commit.id()
    );
    assert_eq!(repo.reflog("HEAD").unwrap().len(), reflog_before + 1);
    let entry = commit
        .tree()
        .unwrap()
        .get_path(Path::new("note.txt"))
        .unwrap();
    assert_eq!(
        repo.find_blob(entry.id()).unwrap().content(),
        b"staged version\n"
    );
    assert_eq!(fixture.index_bytes(), index_before);
    assert_eq!(fixture.read("note.txt"), b"later unstaged version\n");
    assert_eq!(fixture.read("private.txt"), b"untracked\n");
    assert_eq!(fixture.blob("private.txt"), None);
    fixture.clean_leases();
}

#[test]
fn empty_commit_missing_identity_and_bad_messages_leave_everything_unchanged() {
    let fixture = Fixture::new(false);
    let head = fixture.head();
    let bytes = fixture.index_bytes();
    assert_eq!(
        fixture
            .fail(WriteAction::Commit {
                summary: "nothing".into(),
                description: String::new()
            })
            .kind,
        WriteErrorKind::NothingToCommit
    );
    fixture.write("note.txt", "new\n");
    fixture.run(WriteAction::StageAll);
    fixture
        .repo
        .config()
        .unwrap()
        .set_str("user.email", "")
        .unwrap();
    assert_eq!(
        fixture
            .fail(WriteAction::Commit {
                summary: "identity".into(),
                description: String::new()
            })
            .kind,
        WriteErrorKind::IdentityMissing
    );
    assert_eq!(
        fixture
            .fail(WriteAction::Commit {
                summary: "bad\nsummary".into(),
                description: String::new()
            })
            .kind,
        WriteErrorKind::InvalidRequest
    );
    assert_eq!(fixture.head(), head);
    assert_ne!(fixture.index_bytes(), bytes);
    assert_eq!(fixture.read("note.txt"), b"new\n");
    fixture.clean_leases();
}

#[test]
fn stale_head_and_index_lock_reject_before_changes() {
    let fixture = Fixture::new(false);
    fixture.write("note.txt", "new\n");
    let bytes = fixture.index_bytes();
    let request = WriteRequest {
        expected_head: Some(Oid::ZERO_SHA1.to_string()),
        action: WriteAction::StageAll,
    };
    assert_eq!(
        write_ops::execute(&fixture.root, &request)
            .unwrap_err()
            .kind,
        WriteErrorKind::ConcurrentChange
    );
    fs::write(fixture.repo.path().join("index.lock"), b"another writer").unwrap();
    assert_eq!(
        fixture.fail(WriteAction::StageAll).kind,
        WriteErrorKind::ConcurrentChange
    );
    assert_eq!(
        fs::read(fixture.repo.path().join("index.lock")).unwrap(),
        b"another writer"
    );
    assert_eq!(fixture.index_bytes(), bytes);
    fs::remove_file(fixture.repo.path().join("index.lock")).unwrap();
    fixture.clean_leases();
}

#[test]
fn head_lock_and_in_progress_state_reject_without_publishing_index() {
    let fixture = Fixture::new(false);
    fixture.write("note.txt", "new\n");
    let bytes = fixture.index_bytes();
    fs::write(fixture.repo.path().join("HEAD.lock"), b"busy").unwrap();
    assert_eq!(
        fixture.fail(WriteAction::StageAll).kind,
        WriteErrorKind::ConcurrentChange
    );
    assert_eq!(fixture.index_bytes(), bytes);
    fs::remove_file(fixture.repo.path().join("HEAD.lock")).unwrap();
    fs::write(
        fixture.repo.path().join("MERGE_HEAD"),
        fixture.head().unwrap(),
    )
    .unwrap();
    assert_eq!(
        fixture.fail(WriteAction::StageAll).kind,
        WriteErrorKind::InProgress
    );
    assert_eq!(fixture.index_bytes(), bytes);
    fs::remove_file(fixture.repo.path().join("MERGE_HEAD")).unwrap();
    fixture.clean_leases();
}

#[test]
fn unsafe_paths_ignored_files_and_failed_batches_are_atomic() {
    let fixture = Fixture::new(false);
    fixture.write("note.txt", "new\n");
    fixture.write("ignored.txt", "private\n");
    let bytes = fixture.index_bytes();
    for path in [
        "../outside",
        "/absolute",
        "C:/outside",
        "x/../note.txt",
        ".git/HEAD",
        "x//y",
        "note.txt:stream",
        "dir\\file",
        "dir./file",
        "control\u{1b}.txt",
    ] {
        assert_eq!(
            fixture
                .fail(WriteAction::StagePaths {
                    paths: vec![path.into()]
                })
                .kind,
            WriteErrorKind::InvalidPath,
            "{path}"
        );
        assert_eq!(fixture.index_bytes(), bytes);
    }
    assert_eq!(
        fixture
            .fail(WriteAction::StagePaths {
                paths: vec!["note.txt".into(), "ignored.txt".into()]
            })
            .kind,
        WriteErrorKind::InvalidPath
    );
    assert_eq!(fixture.index_bytes(), bytes);
    assert_eq!(
        fixture
            .fail(WriteAction::StagePaths {
                paths: vec!["note.txt".into(), "absent.txt".into()]
            })
            .kind,
        WriteErrorKind::InvalidPath
    );
    assert_eq!(fixture.index_bytes(), bytes);
    fixture.clean_leases();
}

#[cfg(windows)]
#[test]
fn windows_devices_and_invalid_file_spellings_are_rejected_before_reading() {
    let fixture = Fixture::new(false);
    let bytes = fixture.index_bytes();
    for path in [
        "CON",
        "NUL.txt",
        "AUX",
        "PRN.log",
        "COM1",
        "LPT9.txt",
        "CONIN$",
        "CONOUT$",
        "wild*.txt",
        "path?name",
    ] {
        assert_eq!(
            fixture
                .fail(WriteAction::StagePaths {
                    paths: vec![path.into()]
                })
                .kind,
            WriteErrorKind::InvalidPath,
            "{path}"
        );
        assert_eq!(fixture.index_bytes(), bytes);
    }
    fixture.clean_leases();
}

#[test]
fn stage_selected_hunk_then_unstage_it_preserves_other_hunk_and_worktree() {
    let fixture = Fixture::new(false);
    fixture.write("note.txt", &changed_text());
    let selected = first_hunk(&fixture.patch("note.txt", false));
    fixture.run(WriteAction::StageHunk {
        path: "note.txt".into(),
        patch: selected,
    });
    assert_eq!(
        fixture.blob("note.txt"),
        Some(
            base_text()
                .replace("line 2\n", "first change\n")
                .into_bytes()
        )
    );
    assert_eq!(fixture.read("note.txt"), changed_text().as_bytes());
    let staged = fixture.patch("note.txt", true);
    fixture.run(WriteAction::UnstageHunk {
        path: "note.txt".into(),
        patch: staged,
    });
    assert_eq!(fixture.blob("note.txt"), Some(base_text().into_bytes()));
    assert_eq!(fixture.read("note.txt"), changed_text().as_bytes());
    fixture.clean_leases();
}

#[test]
fn unstage_selected_hunk_leaves_other_staged_hunk() {
    let fixture = Fixture::new(false);
    fixture.write("note.txt", &changed_text());
    fixture.run(WriteAction::StageAll);
    fixture.run(WriteAction::UnstageHunk {
        path: "note.txt".into(),
        patch: first_hunk(&fixture.patch("note.txt", true)),
    });
    assert_eq!(
        fixture.blob("note.txt"),
        Some(
            base_text()
                .replace("line 28\n", "last change\n")
                .into_bytes()
        )
    );
    assert_eq!(fixture.read("note.txt"), changed_text().as_bytes());
    fixture.clean_leases();
}

#[test]
fn stale_and_forged_hunks_never_modify_real_index() {
    let fixture = Fixture::new(false);
    fixture.write("note.txt", &changed_text());
    let selected = first_hunk(&fixture.patch("note.txt", false));
    let bytes = fixture.index_bytes();
    let forged = selected.replace("+first change", "+injected content");
    assert_eq!(
        fixture
            .fail(WriteAction::StageHunk {
                path: "note.txt".into(),
                patch: forged
            })
            .kind,
        WriteErrorKind::PatchStale
    );
    assert_eq!(fixture.index_bytes(), bytes);
    fixture.write(
        "note.txt",
        &changed_text().replace("first change", "different edit"),
    );
    assert_eq!(
        fixture
            .fail(WriteAction::StageHunk {
                path: "note.txt".into(),
                patch: selected
            })
            .kind,
        WriteErrorKind::PatchStale
    );
    assert_eq!(fixture.index_bytes(), bytes);
    fixture.clean_leases();
}

#[test]
fn stale_index_baseline_hunk_is_rejected() {
    let fixture = Fixture::new(false);
    fixture.write("note.txt", &changed_text());
    let selected = first_hunk(&fixture.patch("note.txt", false));
    fixture.run(WriteAction::StageAll);
    let bytes = fixture.index_bytes();
    assert_eq!(
        fixture
            .fail(WriteAction::StageHunk {
                path: "note.txt".into(),
                patch: selected
            })
            .kind,
        WriteErrorKind::PatchStale
    );
    assert_eq!(fixture.index_bytes(), bytes);
    fixture.clean_leases();
}

#[test]
fn stage_hunk_preserves_crlf_and_missing_final_newline() {
    let fixture = Fixture::new(false);
    fixture.write("note.txt", "old\r\nlast");
    fixture.run(WriteAction::StageAll);
    fixture.run(WriteAction::Commit {
        summary: "CRLF base".into(),
        description: String::new(),
    });
    fixture.write("note.txt", "new\r\nlast");
    fixture.run(WriteAction::StageHunk {
        path: "note.txt".into(),
        patch: fixture.patch("note.txt", false),
    });
    assert_eq!(fixture.blob("note.txt"), Some(b"new\r\nlast".to_vec()));
    assert_eq!(fixture.read("note.txt"), b"new\r\nlast");
    fixture.clean_leases();
}

#[test]
fn newly_added_and_deleted_text_hunks_are_supported() {
    let fixture = Fixture::new(false);
    fixture.write("new.txt", "new text\n");
    fixture.run(WriteAction::StageHunk {
        path: "new.txt".into(),
        patch: fixture.patch("new.txt", false),
    });
    assert_eq!(fixture.blob("new.txt"), Some(b"new text\n".to_vec()));
    fixture.run(WriteAction::UnstageHunk {
        path: "new.txt".into(),
        patch: fixture.patch("new.txt", true),
    });
    assert_eq!(fixture.blob("new.txt"), None);
    fs::remove_file(fixture.root.join("other.txt")).unwrap();
    fixture.run(WriteAction::StageHunk {
        path: "other.txt".into(),
        patch: fixture.patch("other.txt", false),
    });
    assert_eq!(fixture.blob("other.txt"), None);
    fixture.run(WriteAction::UnstageHunk {
        path: "other.txt".into(),
        patch: fixture.patch("other.txt", true),
    });
    assert_eq!(fixture.blob("other.txt"), Some(b"other\n".to_vec()));
    assert!(!fixture.root.join("other.txt").exists());
    fixture.clean_leases();
}

#[test]
fn nested_repository_paths_are_not_added_to_parent() {
    let fixture = Fixture::new(false);
    let nested = fixture.root.join("nested");
    Repository::init(&nested).unwrap();
    fs::write(nested.join("private.txt"), "private").unwrap();
    let bytes = fixture.index_bytes();
    assert_eq!(
        fixture
            .fail(WriteAction::StagePaths {
                paths: vec!["nested/private.txt".into()]
            })
            .kind,
        WriteErrorKind::InvalidPath
    );
    assert_eq!(fixture.index_bytes(), bytes);
    fixture.clean_leases();
}

#[test]
fn detached_head_can_commit_without_moving_other_branches() {
    let fixture = Fixture::new(false);
    let previous = Oid::from_str(&fixture.head().unwrap()).unwrap();
    let branch = fixture.repo.head().unwrap().name().unwrap().to_owned();
    fixture.repo.set_head_detached(previous).unwrap();
    fixture.write("note.txt", "detached changes\n");
    fixture.run(WriteAction::StageAll);
    fixture.run(WriteAction::Commit {
        summary: "detached".into(),
        description: String::new(),
    });
    let repo = Repository::open(&fixture.root).unwrap();
    assert!(repo.head_detached().unwrap());
    assert_ne!(repo.head().unwrap().target(), Some(previous));
    assert_eq!(repo.refname_to_id(&branch).unwrap(), previous);
    fixture.clean_leases();
}

#[test]
fn bare_or_child_directories_are_not_accepted_as_worktree_root() {
    let fixture = Fixture::new(false);
    let child = fixture.root.join("child");
    fs::create_dir(&child).unwrap();
    let request = fixture.request(WriteAction::StageAll);
    assert_eq!(
        write_ops::execute(&child, &request).unwrap_err().kind,
        WriteErrorKind::InvalidRepository
    );
    let bare = fixture.directory.join("bare.git");
    Repository::init_bare(&bare).unwrap();
    assert_eq!(
        write_ops::execute(&bare, &request).unwrap_err().kind,
        WriteErrorKind::InvalidRepository
    );
    fixture.clean_leases();
}

#[test]
fn ui_backend_diff_patch_is_accepted_for_stage_and_unstage() {
    use gitviews_git::{Git2Backend, GitBackend};
    let fixture = Fixture::new(false);
    fixture.write("note.txt", &changed_text());
    let displayed = Git2Backend.diff(&fixture.root, "note.txt", false).unwrap();
    Git2Backend
        .write(
            &fixture.root,
            &gitviews_git::WriteRequest {
                expected_head: fixture.head(),
                action: gitviews_git::WriteAction::StageHunk {
                    path: "note.txt".into(),
                    patch: first_hunk(&displayed.patch),
                },
            },
        )
        .unwrap();
    assert_eq!(
        fixture.blob("note.txt"),
        Some(
            base_text()
                .replace("line 2\n", "first change\n")
                .into_bytes()
        )
    );
    let displayed = Git2Backend.diff(&fixture.root, "note.txt", true).unwrap();
    Git2Backend
        .write(
            &fixture.root,
            &gitviews_git::WriteRequest {
                expected_head: fixture.head(),
                action: gitviews_git::WriteAction::UnstageHunk {
                    path: "note.txt".into(),
                    patch: displayed.patch,
                },
            },
        )
        .unwrap();
    assert_eq!(fixture.blob("note.txt"), Some(base_text().into_bytes()));
    assert_eq!(fixture.read("note.txt"), changed_text().as_bytes());
    fixture.clean_leases();
}

#[test]
fn ui_backend_diff_new_file_patch_matches_native_write_contract() {
    use gitviews_git::{Git2Backend, GitBackend};
    let fixture = Fixture::new(true);
    fixture.write("新增.txt", "initial content\nwithout final newline");
    let displayed = Git2Backend.diff(&fixture.root, "新增.txt", false).unwrap();
    Git2Backend
        .write(
            &fixture.root,
            &gitviews_git::WriteRequest {
                expected_head: None,
                action: gitviews_git::WriteAction::StageHunk {
                    path: "新增.txt".into(),
                    patch: displayed.patch,
                },
            },
        )
        .unwrap();
    assert_eq!(
        fixture.blob("新增.txt"),
        Some(b"initial content\nwithout final newline".to_vec())
    );
    fixture.clean_leases();
}

#[test]
fn hidden_index_flags_are_rejected_without_changing_files_or_index() {
    for assume_unchanged in [true, false] {
        let fixture = Fixture::new(false);
        let mut index = fixture.repo.index().unwrap();
        let mut entry = index.get_path(Path::new("note.txt"), 0).unwrap();
        if assume_unchanged {
            entry.flags |= 0x8000;
        } else {
            entry.flags |= 0x4000;
            entry.flags_extended |= 0x4000;
        }
        index.add(&entry).unwrap();
        index.write().unwrap();
        drop(index);
        fixture.write("note.txt", "hidden changes\n");
        let bytes = fixture.index_bytes();
        assert_eq!(
            fixture.fail(WriteAction::StageAll).kind,
            WriteErrorKind::Unsupported
        );
        assert_eq!(
            fixture.fail(WriteAction::UnstageAll).kind,
            WriteErrorKind::Unsupported
        );
        assert_eq!(fixture.index_bytes(), bytes);
        assert_eq!(fixture.read("note.txt"), b"hidden changes\n");
        fixture.clean_leases();
    }
}

#[test]
fn conflicted_index_is_not_silently_staged_or_committed() {
    let fixture = Fixture::new(false);
    let mut index = fixture.repo.index().unwrap();
    let base = index.get_path(Path::new("note.txt"), 0).unwrap();
    index.remove_path(Path::new("note.txt")).unwrap();
    for stage in 1..=3 {
        let mut entry = git2::IndexEntry {
            ctime: base.ctime,
            mtime: base.mtime,
            dev: base.dev,
            ino: base.ino,
            mode: base.mode,
            uid: base.uid,
            gid: base.gid,
            file_size: base.file_size,
            id: base.id,
            flags: (base.flags & !0x3000) | (stage << 12),
            flags_extended: base.flags_extended,
            path: base.path.clone(),
        };
        entry.id = fixture
            .repo
            .blob(format!("stage {stage}\n").as_bytes())
            .unwrap();
        index.add(&entry).unwrap();
    }
    assert!(index.has_conflicts());
    index.write().unwrap();
    drop(index);
    let bytes = fixture.index_bytes();
    assert_eq!(
        fixture.fail(WriteAction::StageAll).kind,
        WriteErrorKind::Conflict
    );
    assert_eq!(
        fixture.fail(WriteAction::UnstageAll).kind,
        WriteErrorKind::Conflict
    );
    assert_eq!(
        fixture
            .fail(WriteAction::Commit {
                summary: "unsafe".into(),
                description: String::new()
            })
            .kind,
        WriteErrorKind::Conflict
    );
    assert_eq!(fixture.index_bytes(), bytes);
    assert_eq!(fixture.read("note.txt"), base_text().as_bytes());
    fixture.clean_leases();
}

#[test]
fn linked_worktree_write_and_commit_update_only_its_index_and_branch() {
    let fixture = Fixture::new(false);
    let parent_head = fixture.head();
    let parent_index = fixture.index_bytes();
    let commit = fixture
        .repo
        .find_commit(Oid::from_str(parent_head.as_ref().unwrap()).unwrap())
        .unwrap();
    let branch = fixture.repo.branch("linked", &commit, false).unwrap();
    let root = fixture.directory.join("链接工作树");
    let mut options = git2::WorktreeAddOptions::new();
    options.reference(Some(branch.get()));
    fixture
        .repo
        .worktree("linked", &root, Some(&options))
        .unwrap();
    fs::write(root.join("note.txt"), "linked staged\n").unwrap();
    write_ops::execute(
        &root,
        &WriteRequest {
            expected_head: parent_head.clone(),
            action: WriteAction::StageAll,
        },
    )
    .unwrap();
    fs::write(root.join("note.txt"), "linked unstaged\n").unwrap();
    let outcome = write_ops::execute(
        &root,
        &WriteRequest {
            expected_head: parent_head.clone(),
            action: WriteAction::Commit {
                summary: "linked worktree commit".into(),
                description: String::new(),
            },
        },
    )
    .unwrap();
    let linked = Repository::open(&root).unwrap();
    let committed = linked.head().unwrap().peel_to_commit().unwrap();
    assert_eq!(outcome.commit_id, Some(committed.id().to_string()));
    assert_eq!(
        committed.parent_id(0).unwrap().to_string(),
        parent_head.unwrap()
    );
    let entry = committed
        .tree()
        .unwrap()
        .get_path(Path::new("note.txt"))
        .unwrap();
    assert_eq!(
        linked.find_blob(entry.id()).unwrap().content(),
        b"linked staged\n"
    );
    assert_eq!(
        fs::read(root.join("note.txt")).unwrap(),
        b"linked unstaged\n"
    );
    assert_eq!(fixture.head(), Some(commit.id().to_string()));
    assert_eq!(fixture.index_bytes(), parent_index);
    assert_eq!(fixture.read("note.txt"), base_text().as_bytes());
    assert!(!linked.path().join("index.lock").exists());
    fixture.clean_leases();
}

#[test]
fn transaction_verify_detects_outside_writers_that_ignore_lock() {
    let fixture = Fixture::new(false);
    let repo = Repository::open(&fixture.root).unwrap();
    let lease = checkout::index_transaction::IndexTransaction::prepare(&repo).unwrap();
    lease.verify().unwrap();
    let original = fixture.index_bytes().unwrap();
    let mut altered = original.clone();
    altered.push(0);
    fs::write(repo.path().join("index"), altered).unwrap();
    assert_eq!(
        lease.verify().unwrap_err().kind,
        checkout::CheckoutErrorKind::ConcurrentChange
    );
    fs::write(repo.path().join("index"), original).unwrap();
    drop(lease);
    fixture.clean_leases();
}

#[test]
fn required_commit_signing_is_never_silently_ignored() {
    let fixture = Fixture::new(false);
    fixture
        .repo
        .config()
        .unwrap()
        .set_bool("commit.gpgsign", true)
        .unwrap();
    fixture.write("note.txt", "sign this commit\n");
    fixture.run(WriteAction::StageAll);
    let head = fixture.head();
    let index = fixture.index_bytes();
    assert_eq!(
        fixture
            .fail(WriteAction::Commit {
                summary: "signed required".into(),
                description: String::new()
            })
            .kind,
        WriteErrorKind::Unsupported
    );
    assert_eq!(fixture.head(), head);
    assert_eq!(fixture.index_bytes(), index);
    assert_eq!(fixture.read("note.txt"), b"sign this commit\n");
    fixture.clean_leases();
}

#[test]
fn unstage_rename_destination_restores_both_index_paths_preserving_working_move() {
    use gitviews_git::{Git2Backend, GitBackend};
    let fixture = Fixture::new(false);
    fs::rename(
        fixture.root.join("note.txt"),
        fixture.root.join("renamed.txt"),
    )
    .unwrap();
    fixture.run(WriteAction::StageAll);
    let snapshot = Git2Backend.inspect(&fixture.root).unwrap();
    assert!(
        snapshot
            .changes
            .iter()
            .any(|change| change.path == "renamed.txt" && change.index_status == 'R'),
        "fixture must use the same single rename row the UI displays: {:?}",
        snapshot.changes
    );
    Git2Backend
        .write(
            &fixture.root,
            &gitviews_git::WriteRequest {
                expected_head: fixture.head(),
                action: gitviews_git::WriteAction::UnstagePaths {
                    paths: vec!["renamed.txt".into()],
                },
            },
        )
        .unwrap();
    assert_eq!(fixture.blob("note.txt"), Some(base_text().into_bytes()));
    assert_eq!(fixture.blob("renamed.txt"), None);
    assert_eq!(fixture.read("renamed.txt"), base_text().as_bytes());
    assert!(!fixture.root.join("note.txt").exists());
    assert!(
        Git2Backend
            .inspect(&fixture.root)
            .unwrap()
            .changes
            .iter()
            .all(|change| change.index_status == ' ' || change.index_status == '?')
    );
    fixture.clean_leases();
}

#[test]
fn staging_submodule_records_only_child_head_not_private_child_files() {
    let fixture = Fixture::new(false);
    let module_root = fixture.root.join("module");
    let child = Repository::init(&module_root).unwrap();
    let signature = Signature::now("Module Author", "module@example.invalid").unwrap();
    let child_commit = |text: &str| {
        fs::write(module_root.join("child.txt"), text).unwrap();
        let mut index = child.index().unwrap();
        index.add_path(Path::new("child.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = child.find_tree(tree_id).unwrap();
        let parent = child
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok());
        let parents: Vec<_> = parent.iter().collect();
        child
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "child change",
                &tree,
                &parents,
            )
            .unwrap()
    };
    let initial = child_commit("initial child\n");
    fixture.write(
        ".gitmodules",
        "[submodule \"module\"]\n\tpath = module\n\turl = https://example.invalid/module.git\n",
    );
    let mut index = fixture.repo.index().unwrap();
    index.add_path(Path::new(".gitmodules")).unwrap();
    index
        .add(&git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o160000,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: initial,
            flags: 0,
            flags_extended: 0,
            path: b"module".to_vec(),
        })
        .unwrap();
    index.write().unwrap();
    drop(index);
    fixture.run(WriteAction::Commit {
        summary: "register module".into(),
        description: String::new(),
    });
    let updated = child_commit("updated child\n");
    fs::write(module_root.join("private.txt"), "private untracked\n").unwrap();
    fixture.run(WriteAction::StagePaths {
        paths: vec!["module".into()],
    });
    let repo = Repository::open(&fixture.root).unwrap();
    let index = repo.index().unwrap();
    let module = index.get_path(Path::new("module"), 0).unwrap();
    assert_eq!(module.mode, 0o160000);
    assert_eq!(module.id, updated);
    assert!(index.get_path(Path::new("module/private.txt"), 0).is_none());
    assert_eq!(
        fs::read(module_root.join("private.txt")).unwrap(),
        b"private untracked\n"
    );
    fixture.run(WriteAction::UnstagePaths {
        paths: vec!["module".into()],
    });
    assert_eq!(
        Repository::open(&fixture.root)
            .unwrap()
            .index()
            .unwrap()
            .get_path(Path::new("module"), 0)
            .unwrap()
            .id,
        initial
    );
    assert_eq!(child.head().unwrap().target(), Some(updated));
    fixture.clean_leases();
}
