use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use gitviews_git::{
    DiffLineKind, FileChange, FileTreeKind, Git2Backend, GitBackend, GitError, Head, TextEncoding,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gitviews test repository {} {unique} {sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn init() -> Self {
        let directory = Self::new();
        directory.git(&["init", "--quiet", "--initial-branch=main"]);
        directory
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn git(&self, arguments: &[&str]) -> Output {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.0)
            .args([
                "-c",
                "user.name=GitViews Tests",
                "-c",
                "user.email=gitviews@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.hooksPath=",
            ])
            .args(arguments)
            .env("LC_ALL", "C")
            .output()
            .expect("Git must be installed to run the repository integration tests");
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn commit_all(&self, subject: &str) {
        self.git(&["add", "--all"]);
        self.git(&["commit", "--quiet", "--message", subject]);
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        // The path is a unique directory created by this helper under temp_dir.
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn combined_status_matrix_preserves_both_sides_and_cached_deletion() {
    let directory = TestDirectory::init();
    for name in [
        "mm", "md", "staged", "cached", "deleted", "rename", "rm", "hidden", "sparse",
    ] {
        directory.write(
            &format!("{name}.txt"),
            &format!("distinct original {name}\n"),
        );
    }
    directory.write(".gitignore", "ignored/\n*.log\n");
    directory.write("tracked.log", "tracked even when ignored\n");
    directory.git(&["add", "--force", "--", "tracked.log"]);
    directory.commit_all("matrix base");
    for name in ["mm", "md", "staged", "am", "ad"] {
        directory.write(&format!("{name}.txt"), &format!("staged version {name}\n"));
        directory.git(&["add", "--", &format!("{name}.txt")]);
    }
    for name in ["mm", "am"] {
        directory.write(
            &format!("{name}.txt"),
            &format!("worktree version {name}\n"),
        );
    }
    for name in ["md", "ad"] {
        fs::remove_file(directory.path().join(format!("{name}.txt"))).unwrap();
    }
    directory.git(&["rm", "--cached", "--", "cached.txt"]);
    directory.git(&["rm", "--", "deleted.txt"]);
    directory.git(&["mv", "--", "rename.txt", "renamed.txt"]);
    directory.git(&["mv", "--", "rm.txt", "renamed_modified.txt"]);
    directory.write(
        "renamed_modified.txt",
        "distinct original rm\nadditional change\n",
    );
    for name in ["ita.txt", "empty_ita.txt"] {
        directory.write(
            name,
            if name == "ita.txt" {
                "intent content\n"
            } else {
                ""
            },
        );
        directory.git(&["add", "-N", "--", name]);
    }
    directory.git(&["update-index", "--assume-unchanged", "--", "hidden.txt"]);
    directory.git(&["update-index", "--skip-worktree", "--", "sparse.txt"]);
    for name in [
        "hidden.txt",
        "sparse.txt",
        "tracked.log",
        "ignored/cache.txt",
    ] {
        directory.write(name, "worktree modification\n");
    }
    let special = [
        "-开头.txt",
        "#井号.txt",
        "!感叹.txt",
        "单引号'.txt",
        "带 空格🎉.txt",
        "new/a/b/deep.txt",
        "empty.txt",
    ];
    for name in special {
        directory.write(
            name,
            if name == "empty.txt" {
                ""
            } else {
                "new content\n"
            },
        );
    }
    let before = fs::read(directory.path().join(".git/index")).unwrap();
    let snapshot = Git2Backend.inspect(directory.path()).unwrap();
    let statuses = |path: &str| -> Vec<(char, char)> {
        snapshot
            .changes
            .iter()
            .filter(|change| change.path == path)
            .map(|change| (change.index_status, change.worktree_status))
            .collect()
    };
    for (path, expected) in [
        ("mm.txt", ('M', 'M')),
        ("md.txt", ('M', 'D')),
        ("staged.txt", ('M', ' ')),
        ("am.txt", ('A', 'M')),
        ("ad.txt", ('A', 'D')),
        ("deleted.txt", ('D', ' ')),
        ("renamed.txt", ('R', ' ')),
        ("renamed_modified.txt", ('R', 'M')),
        ("ita.txt", (' ', 'A')),
        ("empty_ita.txt", (' ', 'A')),
        ("tracked.log", (' ', 'M')),
    ] {
        assert_eq!(statuses(path), [expected], "{path}: {:?}", snapshot.changes);
    }
    assert_eq!(statuses("cached.txt"), [('D', ' '), ('?', '?')]);
    for path in special {
        assert_eq!(statuses(path), [('?', '?')], "{path}");
    }
    for path in ["hidden.txt", "sparse.txt", "ignored/cache.txt"] {
        assert!(statuses(path).is_empty(), "{path}");
    }
    let tree = Git2Backend
        .tree_with_changes(directory.path(), &snapshot.changes)
        .unwrap();
    assert!(
        !tree
            .entries
            .iter()
            .any(|entry| entry.path == "rename.txt" || entry.path == "rm.txt")
    );
    let cached = tree
        .entries
        .iter()
        .find(|entry| entry.path == "cached.txt")
        .unwrap();
    assert_eq!((cached.index_status, cached.worktree_status), ('D', '?'));
    for (path, staged, old, new) in [
        ("ita.txt", false, "", "intent content\n"),
        ("empty_ita.txt", false, "", ""),
        ("ad.txt", false, "staged version ad\n", ""),
        ("cached.txt", true, "distinct original cached\n", ""),
        ("cached.txt", false, "", "distinct original cached\n"),
        (
            "renamed.txt",
            true,
            "distinct original rename\n",
            "distinct original rename\n",
        ),
        (
            "renamed_modified.txt",
            false,
            "distinct original rm\n",
            "distinct original rm\nadditional change\n",
        ),
    ] {
        let diff = Git2Backend.diff(directory.path(), path, staged).unwrap();
        assert_eq!(
            (diff.old_text.as_str(), diff.new_text.as_str()),
            (old, new),
            "{path} staged={staged}"
        );
    }
    assert_eq!(
        fs::read(directory.path().join(".git/index")).unwrap(),
        before,
        "all status/tree/diff reads must leave index bytes intact"
    );
}

#[test]
fn all_seven_unmerged_pairs_are_preserved_without_mutating_index() {
    let directory = TestDirectory::init();
    let pairs = [
        ("dd", [true, false, false], ('D', 'D')),
        ("au", [false, true, false], ('A', 'U')),
        ("ud", [true, true, false], ('U', 'D')),
        ("ua", [false, false, true], ('U', 'A')),
        ("du", [true, false, true], ('D', 'U')),
        ("aa", [false, true, true], ('A', 'A')),
        ("uu", [true, true, true], ('U', 'U')),
    ];
    for (name, _, _) in pairs {
        directory.write(&format!("{name}.txt"), "base\n");
    }
    directory.commit_all("conflict base");
    let repo = git2::Repository::open(directory.path()).unwrap();
    let mut index = repo.index().unwrap();
    for (name, stages, _) in pairs {
        let path = format!("{name}.txt");
        index.remove_path(Path::new(&path)).unwrap();
        for (position, present) in stages.into_iter().enumerate() {
            if !present {
                continue;
            }
            let stage = position + 1;
            let id = repo.blob(format!("stage {stage}\n").as_bytes()).unwrap();
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
                    id,
                    flags: (stage as u16) << 12,
                    flags_extended: 0,
                    path: path.as_bytes().to_vec(),
                })
                .unwrap();
        }
    }
    index.write().unwrap();
    let before = fs::read(directory.path().join(".git/index")).unwrap();
    let snapshot = Git2Backend.inspect(directory.path()).unwrap();
    let tree = Git2Backend
        .tree_with_changes(directory.path(), &snapshot.changes)
        .unwrap();
    for (name, _, expected) in pairs {
        let path = format!("{name}.txt");
        let change = snapshot
            .changes
            .iter()
            .find(|change| change.path == path)
            .unwrap();
        assert_eq!(
            (change.index_status, change.worktree_status),
            expected,
            "{name}"
        );
        assert!(change.is_conflicted());
        let entry = tree
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap();
        assert_eq!((entry.index_status, entry.worktree_status), expected);
        assert!(matches!(
            Git2Backend.diff(directory.path(), &path, false),
            Err(GitError::Unsupported { .. })
        ));
        assert!(
            Git2Backend
                .conflict_versions(directory.path(), &path)
                .is_ok(),
            "{name}"
        );
    }
    assert_eq!(
        fs::read(directory.path().join(".git/index")).unwrap(),
        before
    );
}

#[test]
fn configured_copy_detection_preserves_the_source_in_preview_and_tree() {
    let directory = TestDirectory::init();
    let original = (0..30)
        .map(|line| format!("original source line {line}\n"))
        .collect::<String>();
    directory.write("source.txt", &original);
    directory.commit_all("copy base");
    directory.git(&["config", "status.renames", "copies"]);
    directory.write("source.txt", &(original.clone() + "source modified\n"));
    directory.write("copy.txt", &original);
    directory.git(&["add", "--", "source.txt", "copy.txt"]);
    let snapshot = Git2Backend.inspect(directory.path()).unwrap();
    let copy = snapshot
        .changes
        .iter()
        .find(|change| change.path == "copy.txt")
        .unwrap();
    assert_eq!((copy.index_status, copy.worktree_status), ('C', ' '));
    let diff = Git2Backend
        .diff(directory.path(), "copy.txt", true)
        .unwrap();
    assert_eq!(diff.old_text, original);
    assert_eq!(diff.new_text, original);
    assert!(diff.patch.contains("copy from source.txt"));
    let tree = Git2Backend
        .tree_with_changes(directory.path(), &snapshot.changes)
        .unwrap();
    assert!(tree.entries.iter().any(|entry| entry.path == "source.txt"));
    assert!(
        tree.entries
            .iter()
            .any(|entry| entry.path == "copy.txt" && entry.index_status == 'C')
    );
}

#[test]
fn staged_type_change_is_readable_without_creating_an_os_symlink() {
    let directory = TestDirectory::init();
    directory.write("kind.txt", "original ordinary file\n");
    directory.commit_all("type base");
    let repo = git2::Repository::open(directory.path()).unwrap();
    let mut index = repo.index().unwrap();
    let mut entry = index.get_path(Path::new("kind.txt"), 0).unwrap();
    entry.mode = 0o120000;
    entry.id = repo.blob(b"target.txt").unwrap();
    entry.file_size = 10;
    index.add(&entry).unwrap();
    index.write().unwrap();
    let snapshot = Git2Backend.inspect(directory.path()).unwrap();
    assert!(
        snapshot
            .changes
            .iter()
            .any(|change| change.path == "kind.txt" && change.index_status == 'T')
    );
    let diff = Git2Backend
        .diff(directory.path(), "kind.txt", true)
        .unwrap();
    assert_eq!(diff.old_text, "original ordinary file\n");
    assert_eq!(diff.new_text, "target.txt");
    assert!(diff.patch.contains("120000"));
}

#[test]
fn submodule_preview_reports_gitlink_commits_and_dirty_working_tree() {
    let source = TestDirectory::init();
    source.write("source.txt", "child base\n");
    source.commit_all("child base");
    let directory = TestDirectory::init();
    directory.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "--quiet",
        source.path().to_str().unwrap(),
        "deps/child",
    ]);
    directory.commit_all("parent base");
    let child_root = directory.path().join("deps/child");
    let child = git2::Repository::open(&child_root).unwrap();
    let old_id = child.head().unwrap().target().unwrap();
    fs::write(child_root.join("source.txt"), "child changed\n").unwrap();
    let dirty = Git2Backend
        .diff(directory.path(), "deps/child", false)
        .unwrap();
    assert_eq!(dirty.old_text, format!("Subproject commit {old_id}\n"));
    assert_eq!(
        dirty.new_text,
        format!("Subproject commit {old_id}-dirty\n")
    );
    assert!(dirty.patch.contains("-dirty"));
    let mut index = child.index().unwrap();
    index.add_path(Path::new("source.txt")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = child.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    let new_id = child
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "new child commit",
            &tree,
            &[&child.find_commit(old_id).unwrap()],
        )
        .unwrap();
    let moved = Git2Backend
        .diff(directory.path(), "deps/child", false)
        .unwrap();
    assert_eq!(moved.new_text, format!("Subproject commit {new_id}\n"));
    directory.git(&["add", "--", "deps/child"]);
    let staged = Git2Backend
        .diff(directory.path(), "deps/child", true)
        .unwrap();
    assert_eq!(staged.old_text, format!("Subproject commit {old_id}\n"));
    assert_eq!(staged.new_text, format!("Subproject commit {new_id}\n"));
    fs::write(child_root.join("untracked.txt"), "untracked child\n").unwrap();
    let untracked = Git2Backend
        .diff(directory.path(), "deps/child", false)
        .unwrap();
    assert_eq!(
        untracked.new_text,
        format!("Subproject commit {new_id}-dirty\n")
    );
}

#[test]
fn rejects_a_directory_that_is_not_a_repository() {
    let directory = TestDirectory::new();
    let error = Git2Backend.inspect(directory.path()).unwrap_err();
    assert!(matches!(
        error,
        GitError::NotRepository { ref path } if path == directory.path()
    ));
}

#[test]
fn lightweight_root_lookup_does_not_read_a_corrupted_index() {
    let directory = TestDirectory::init();
    directory.write("base.txt", "base\n");
    directory.commit_all("Base");
    fs::write(directory.path().join(".git/index"), b"invalid test index").unwrap();
    assert_eq!(
        Git2Backend.repository_root(directory.path()).unwrap(),
        fs::canonicalize(directory.path()).unwrap()
    );
    assert!(Git2Backend.inspect(directory.path()).is_err());
    let metadata = Git2Backend.inspect_metadata(directory.path()).unwrap();
    assert_eq!(metadata.root, fs::canonicalize(directory.path()).unwrap());
    assert_eq!(metadata.head, Head::Branch("main".into()));
    assert!(metadata.changes.is_empty());
    assert!(metadata.recent_commits.is_empty());
}

#[test]
fn recent_overview_does_not_traverse_the_complete_commit_graph() {
    let directory = TestDirectory::init();
    let repo = git2::Repository::open(directory.path()).unwrap();
    let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let mut previous = None;
    let mut oldest = None;
    for number in 0..80 {
        let signature = git2::Signature::new(
            "Tests",
            "tests@example.invalid",
            &git2::Time::new(1_700_000_000 + number, 0),
        )
        .unwrap();
        let parent = previous.map(|id| repo.find_commit(id).unwrap());
        let parents = parent.iter().collect::<Vec<_>>();
        let id = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                &format!("Commit {number}"),
                &tree,
                &parents,
            )
            .unwrap();
        oldest.get_or_insert(id);
        previous = Some(id);
    }
    // Removing this disposable fixture's oldest object makes an eager full
    // graph traversal fail, while reading the recent prefix remains possible.
    let oldest = oldest.unwrap().to_string();
    fs::remove_file(
        repo.path()
            .join("objects")
            .join(&oldest[..2])
            .join(&oldest[2..]),
    )
    .unwrap();
    let snapshot = Git2Backend.inspect(directory.path()).unwrap();
    assert_eq!(snapshot.recent_commits.len(), 20);
    assert_eq!(snapshot.recent_commits[0].subject, "Commit 79");
    assert_eq!(snapshot.recent_commits[19].subject, "Commit 60");
}

#[test]
fn recent_overview_keeps_head_first_and_deduplicates_merged_ancestry() {
    let directory = TestDirectory::init();
    let repo = git2::Repository::open(directory.path()).unwrap();
    let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let commit = |subject: &str, seconds: i64, parents: &[git2::Oid]| {
        let signature = git2::Signature::new(
            "Tests",
            "tests@example.invalid",
            &git2::Time::new(seconds, 0),
        )
        .unwrap();
        let parents = parents
            .iter()
            .map(|id| repo.find_commit(*id).unwrap())
            .collect::<Vec<_>>();
        repo.commit(
            None,
            &signature,
            &signature,
            subject,
            &tree,
            &parents.iter().collect::<Vec<_>>(),
        )
        .unwrap()
    };
    let base = commit("Base", 1_700_000_000, &[]);
    let left = commit("Left", 1_700_000_100, &[base]);
    let right = commit("Right", 1_700_000_200, &[base]);
    // A clock-skewed merge must remain first: callers use its ID as HEAD.
    let merge = commit("Merge", 1_700_000_050, &[left, right]);
    repo.reference("refs/heads/main", merge, true, "fixture")
        .unwrap();
    let snapshot = Git2Backend.inspect(directory.path()).unwrap();
    assert_eq!(
        snapshot
            .recent_commits
            .iter()
            .map(|commit| commit.subject.as_str())
            .collect::<Vec<_>>(),
        ["Merge", "Right", "Left", "Base"]
    );
    assert_eq!(snapshot.recent_commits[0].id, merge.to_string());
}

#[test]
fn file_tree_uses_supplied_status_instead_of_scanning_status_again() {
    let directory = TestDirectory::init();
    directory.write("tracked.txt", "base\n");
    directory.commit_all("Base");
    directory.write("tracked.txt", "changed\n");
    let supplied = [FileChange {
        path: "tracked.txt".into(),
        index_status: ' ',
        worktree_status: 'T',
    }];
    let tree = Git2Backend
        .tree_with_changes(directory.path(), &supplied)
        .unwrap();
    let file = tree
        .entries
        .iter()
        .find(|entry| entry.path == "tracked.txt")
        .unwrap();
    assert_eq!(file.worktree_status, 'T');
    let ordinary = Git2Backend.tree(directory.path()).unwrap();
    assert!(
        ordinary
            .entries
            .iter()
            .any(|entry| { entry.path == "tracked.txt" && entry.worktree_status == 'M' })
    );
}

#[test]
fn opens_an_unborn_repository_and_lists_untracked_files() {
    let directory = TestDirectory::init();
    directory.write("first file.txt", "waiting for the first commit\n");

    let snapshot = Git2Backend.inspect(directory.path()).unwrap();
    assert_eq!(snapshot.head, Head::Unborn("main".to_owned()));
    assert!(snapshot.recent_commits.is_empty());
    assert_eq!(snapshot.changes.len(), 1);
    assert_eq!(snapshot.changes[0].path, "first file.txt");
    assert_eq!(snapshot.changes[0].index_status, '?');
    assert_eq!(snapshot.changes[0].worktree_status, '?');
}

#[test]
fn reads_branch_history_and_status_from_a_subdirectory_with_spaces() {
    let directory = TestDirectory::init();
    directory.write("staged file.txt", "initial\n");
    directory.write("changed file.txt", "initial\n");
    directory.write("before name.txt", "rename this file\n");
    directory.commit_all("Initial commit with spaces");
    directory.write("staged file.txt", "staged\n");
    directory.git(&["add", "--", "staged file.txt"]);
    directory.write("changed file.txt", "unstaged\n");
    directory.git(&["mv", "--", "before name.txt", "after name.txt"]);
    directory.write("new file with spaces.txt", "untracked\n");
    directory.write("nested directory/新 文件.txt", "another untracked file\n");

    let snapshot = Git2Backend
        .inspect(&directory.path().join("nested directory"))
        .unwrap();
    assert_eq!(snapshot.head, Head::Branch("main".to_owned()));
    assert_eq!(
        fs::canonicalize(&snapshot.root).unwrap(),
        fs::canonicalize(directory.path()).unwrap()
    );
    assert_eq!(snapshot.root, fs::canonicalize(directory.path()).unwrap());
    assert_eq!(
        snapshot.name,
        directory.path().file_name().unwrap().to_string_lossy()
    );
    assert_eq!(snapshot.recent_commits.len(), 1);
    assert_eq!(
        snapshot.recent_commits[0].subject,
        "Initial commit with spaces"
    );
    assert_eq!(snapshot.recent_commits[0].author, "GitViews Tests");
    assert!(snapshot.recent_commits[0].id.len() >= 40);

    let status = |path: &str| {
        let change = snapshot
            .changes
            .iter()
            .find(|change| change.path == path)
            .unwrap();
        (change.index_status, change.worktree_status)
    };
    assert_eq!(snapshot.changes.len(), 5);
    assert_eq!(status("staged file.txt"), ('M', ' '));
    assert_eq!(status("changed file.txt"), (' ', 'M'));
    assert_eq!(status("after name.txt"), ('R', ' '));
    assert_eq!(status("new file with spaces.txt"), ('?', '?'));
    assert_eq!(status("nested directory/新 文件.txt"), ('?', '?'));
    assert!(
        !snapshot
            .changes
            .iter()
            .any(|change| change.path == "before name.txt")
    );
}

#[test]
fn reads_detached_head_and_newest_first_history() {
    let directory = TestDirectory::init();
    directory.write("file.txt", "first\n");
    directory.commit_all("First commit");
    directory.write("file.txt", "second\n");
    directory.commit_all("Second commit");
    directory.git(&["checkout", "--quiet", "--detach"]);

    let snapshot = Git2Backend.inspect(directory.path()).unwrap();
    assert_eq!(snapshot.recent_commits.len(), 2);
    assert_eq!(snapshot.recent_commits[0].subject, "Second commit");
    assert_eq!(snapshot.recent_commits[1].subject, "First commit");
    assert_eq!(
        snapshot.head,
        Head::Detached(snapshot.recent_commits[0].id.clone())
    );
    assert!(snapshot.changes.is_empty());
}

#[test]
fn rejects_a_bare_repository_with_a_useful_error() {
    let directory = TestDirectory::new();
    directory.git(&["init", "--bare", "--quiet"]);
    let error = Git2Backend.inspect(directory.path()).unwrap_err();
    assert!(matches!(error, GitError::NotRepository { .. }));
    assert!(error.to_string().contains("working tree"));
}

#[test]
fn branch_names_remain_unambiguous_when_a_tag_has_the_same_name() {
    let directory = TestDirectory::init();
    directory.write("file.txt", "initial\n");
    directory.commit_all("Initial commit");
    directory.git(&["tag", "main"]);

    let snapshot = Git2Backend.inspect(directory.path()).unwrap();
    assert_eq!(snapshot.head, Head::Branch("main".to_owned()));
}

#[test]
fn does_not_misreport_a_broken_branch_as_an_empty_repository() {
    let directory = TestDirectory::init();
    directory.write(
        ".git/refs/heads/main",
        "0000000000000000000000000000000000000000\n",
    );

    let error = Git2Backend.inspect(directory.path()).unwrap_err();
    assert!(matches!(error, GitError::Native { .. }));
    assert!(error.to_string().contains("refs/heads/main"));
}

#[test]
fn lists_worktrees_local_branches_and_remote_tracking_branches() {
    let directory = TestDirectory::init();
    directory.write("file.txt", "initial\n");
    directory.commit_all("Initial commit");
    directory.git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let linked = TestDirectory::new();
    directory.git(&[
        "worktree",
        "add",
        "--quiet",
        "-b",
        "feature/worktrees",
        linked.path().to_str().unwrap(),
    ]);

    let details = Git2Backend.details(directory.path()).unwrap();
    assert_eq!(details.worktrees.len(), 2);
    assert!(details.worktrees[0].is_main);
    assert_eq!(details.worktrees[0].branch.as_deref(), Some("main"));
    assert_eq!(
        details.worktrees[1].branch.as_deref(),
        Some("feature/worktrees")
    );
    assert!(!details.worktrees[1].is_main);
    assert!(
        details
            .branches
            .iter()
            .any(|branch| branch.name == "main" && branch.is_current && !branch.is_remote)
    );
    assert!(
        details
            .branches
            .iter()
            .any(|branch| branch.name == "feature/worktrees" && !branch.is_current)
    );
    assert!(
        details
            .branches
            .iter()
            .any(|branch| branch.name == "origin/main" && branch.is_remote)
    );
    let linked_details = Git2Backend.details(linked.path()).unwrap();
    assert!(
        linked_details
            .branches
            .iter()
            .any(|branch| branch.name == "feature/worktrees" && branch.is_current)
    );
}

#[test]
fn lists_an_unborn_branch_in_repository_details() {
    let directory = TestDirectory::init();
    let details = Git2Backend.details(directory.path()).unwrap();
    assert_eq!(details.worktrees.len(), 1);
    assert_eq!(details.worktrees[0].branch.as_deref(), Some("main"));
    assert!(details.worktrees[0].head.is_none());
    assert_eq!(details.branches.len(), 1);
    assert_eq!(details.branches[0].name, "main");
    assert!(details.branches[0].is_current);
}

#[test]
fn compares_head_index_and_working_file_with_correct_line_numbers() {
    let directory = TestDirectory::init();
    directory.write("file with spaces.txt", "same\noriginal\nlast\n");
    directory.commit_all("Initial commit");
    directory.write("file with spaces.txt", "same\nstaged\nlast\n");
    directory.git(&["add", "--", "file with spaces.txt"]);
    directory.write("file with spaces.txt", "same\nworking\nlast\n");

    let staged = Git2Backend
        .diff(directory.path(), "file with spaces.txt", true)
        .unwrap();
    assert_eq!(staged.old_text, "same\noriginal\nlast\n");
    assert_eq!(staged.new_text, "same\nstaged\nlast\n");
    assert!(
        staged
            .lines
            .iter()
            .any(|line| line.kind == DiffLineKind::Deleted
                && line.old_line == Some(2)
                && line.new_line.is_none()
                && line.text == "original")
    );
    assert!(
        staged
            .lines
            .iter()
            .any(|line| line.kind == DiffLineKind::Added
                && line.new_line == Some(2)
                && line.old_line.is_none()
                && line.text == "staged")
    );
    let unstaged = Git2Backend
        .diff(directory.path(), "file with spaces.txt", false)
        .unwrap();
    assert_eq!(unstaged.old_text, "same\nstaged\nlast\n");
    assert_eq!(unstaged.new_text, "same\nworking\nlast\n");
    assert!(unstaged.patch.contains("+working"));
}

#[test]
fn previews_untracked_files_and_files_staged_before_the_first_commit() {
    let directory = TestDirectory::init();
    directory.write("-new file.txt", "first\nsecond");
    let unstaged = Git2Backend
        .diff(directory.path(), "-new file.txt", false)
        .unwrap();
    assert!(unstaged.old_text.is_empty());
    assert_eq!(unstaged.new_text, "first\nsecond");
    assert!(unstaged.patch.contains("No newline at end of file"));
    assert_eq!(
        unstaged
            .lines
            .iter()
            .filter(|line| line.kind == DiffLineKind::Added)
            .count(),
        2
    );
    directory.git(&["add", "--", "-new file.txt"]);
    let staged = Git2Backend
        .diff(directory.path(), "-new file.txt", true)
        .unwrap();
    assert!(staged.old_text.is_empty());
    assert_eq!(staged.new_text, "first\nsecond");
}

#[test]
fn previews_renamed_and_deleted_files() {
    let directory = TestDirectory::init();
    directory.write("before name.txt", "contents\n");
    directory.commit_all("Initial commit");
    directory.git(&["mv", "--", "before name.txt", "after name.txt"]);
    let renamed = Git2Backend
        .diff(directory.path(), "after name.txt", true)
        .unwrap();
    assert_eq!(renamed.old_text, "contents\n");
    assert_eq!(renamed.new_text, "contents\n");
    assert!(renamed.patch.contains("rename from before name.txt"));
    assert!(
        Git2Backend
            .inspect(directory.path())
            .unwrap()
            .changes
            .iter()
            .any(|change| { change.path == "after name.txt" && change.index_status == 'R' })
    );
    fs::remove_file(directory.path().join("after name.txt")).unwrap();
    let deleted = Git2Backend
        .diff(directory.path(), "after name.txt", false)
        .unwrap();
    assert_eq!(deleted.old_text, "contents\n");
    assert!(deleted.new_text.is_empty());
    assert!(
        deleted
            .lines
            .iter()
            .any(|line| line.kind == DiffLineKind::Deleted && line.old_line == Some(1))
    );
}

#[test]
fn oversized_rename_candidates_keep_add_delete_status_and_small_files_readable() {
    let directory = TestDirectory::init();
    directory.write("large-before.txt", &"x".repeat(2 * 1024 * 1024 + 1));
    directory.write("small-before.txt", "small rename\n");
    directory.write("ordinary.txt", "before\n");
    directory.commit_all("initial files");
    directory.git(&["mv", "--", "large-before.txt", "large-after.txt"]);
    directory.git(&["mv", "--", "small-before.txt", "small-after.txt"]);
    directory.write("ordinary.txt", "after\n");
    directory.git(&["add", "--", "ordinary.txt"]);

    let status = Git2Backend.inspect(directory.path()).unwrap();
    for (path, index_status) in [
        ("large-before.txt", 'D'),
        ("large-after.txt", 'A'),
        ("small-before.txt", 'D'),
        ("small-after.txt", 'A'),
        ("ordinary.txt", 'M'),
    ] {
        assert!(
            status
                .changes
                .iter()
                .any(|change| { change.path == path && change.index_status == index_status })
        );
    }

    let small = Git2Backend
        .diff(directory.path(), "ordinary.txt", true)
        .unwrap();
    assert_eq!(small.old_text, "before\n");
    assert_eq!(small.new_text, "after\n");
    let addition = Git2Backend
        .diff(directory.path(), "small-after.txt", true)
        .unwrap();
    assert!(addition.old_text.is_empty());
    assert_eq!(addition.new_text, "small rename\n");
    assert!(!addition.patch.contains("rename from"));
    assert!(matches!(
        Git2Backend.diff(directory.path(), "large-after.txt", true),
        Err(GitError::FileTooLarge { .. })
    ));

    directory.commit_all("rename with an oversized candidate");
    let id = git2::Repository::open(directory.path())
        .unwrap()
        .head()
        .unwrap()
        .target()
        .unwrap();
    let details = Git2Backend
        .commit(directory.path(), &id.to_string())
        .unwrap();
    for (path, status) in [
        ("large-before.txt", 'D'),
        ("large-after.txt", 'A'),
        ("small-before.txt", 'D'),
        ("small-after.txt", 'A'),
    ] {
        assert!(
            details
                .changes
                .iter()
                .any(|change| change.path == path && change.index_status == status)
        );
    }
    let historical = Git2Backend
        .commit_diff(directory.path(), &id.to_string(), "ordinary.txt")
        .unwrap();
    assert_eq!(historical.old_text, "before\n");
    assert_eq!(historical.new_text, "after\n");
}

#[test]
fn rejects_unsafe_binary_and_oversized_diff_inputs() {
    let directory = TestDirectory::init();
    for path in [
        "../outside.txt",
        "/absolute.txt",
        "C:/Windows/file.txt",
        "nested/../../outside",
        ".git/config",
        "folder\\..\\outside",
    ] {
        assert!(
            matches!(
                Git2Backend.diff(directory.path(), path, false),
                Err(GitError::InvalidPath { .. })
            ),
            "{path}"
        );
    }
    fs::write(directory.path().join("binary.bin"), [0, 1, 2, 3]).unwrap();
    assert!(matches!(
        Git2Backend.diff(directory.path(), "binary.bin", false),
        Err(GitError::BinaryFile { .. })
    ));
    directory.write("large.txt", &"x".repeat(2 * 1024 * 1024 + 1));
    assert!(matches!(
        Git2Backend.diff(directory.path(), "large.txt", false),
        Err(GitError::FileTooLarge { .. })
    ));
    assert!(matches!(
        Git2Backend.diff(directory.path(), "missing.txt", false),
        Err(GitError::InvalidPath { .. })
    ));
}

#[test]
fn tree_includes_tracked_untracked_and_deleted_files_but_obeys_ignores() {
    let directory = TestDirectory::init();
    directory.write("src/tracked.rs", "tracked\n");
    directory.write("target/keep.txt", "tracked despite ignore\n");
    directory.write("deleted.txt", "delete in worktree\n");
    directory.write("removed.txt", "delete from index\n");
    directory.commit_all("Initial tree");
    directory.write(".gitignore", "target/\n*.tmp\n");
    directory.commit_all("Add ignore rules");
    directory.write("target/cache.bin", "ignored\n");
    directory.write("ignored.tmp", "ignored\n");
    directory.write("new folder/新 文件.txt", "new\n");
    fs::remove_file(directory.path().join("deleted.txt")).unwrap();
    directory.git(&["rm", "--quiet", "--", "removed.txt"]);
    let tree = Git2Backend.tree(directory.path()).unwrap();
    let entry = |path| {
        tree.entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap()
    };
    assert_eq!(entry("src").kind, FileTreeKind::Directory);
    assert_eq!(entry("src/tracked.rs").kind, FileTreeKind::File);
    assert_eq!(entry("new folder/新 文件.txt").index_status, '?');
    assert_eq!(entry("deleted.txt").worktree_status, 'D');
    assert_eq!(entry("removed.txt").index_status, 'D');
    assert_eq!(entry("target/keep.txt").kind, FileTreeKind::File);
    assert!(
        !tree
            .entries
            .iter()
            .any(|entry| entry.path == "target/cache.bin"
                || entry.path == "ignored.tmp"
                || entry.path.starts_with(".git/"))
    );
    let clean = Git2Backend
        .diff(directory.path(), "src/tracked.rs", false)
        .unwrap();
    assert!(clean.lines.is_empty());
    assert_eq!(clean.new_text, "tracked\n");
}

#[test]
fn reads_initialized_dirty_and_uninitialized_submodules_without_recursing() {
    let source = TestDirectory::init();
    source.write("source.txt", "nested repository\n");
    source.commit_all("Nested commit");
    let directory = TestDirectory::init();
    directory.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "--quiet",
        source.path().to_str().unwrap(),
        "deps/native lib",
    ]);
    directory.commit_all("Add module");
    directory.write("deps/native lib/dirty.txt", "untracked module content\n");
    let registration = Git2Backend
        .registered_submodule(directory.path(), "deps/native lib")
        .unwrap()
        .unwrap();
    assert!(registration.initialized);
    // Registration reads do not inspect child worktree changes.
    assert!(!registration.dirty);
    assert!(
        Git2Backend
            .registered_submodule(directory.path(), "source.txt")
            .unwrap()
            .is_none()
    );
    let tree = Git2Backend.tree(directory.path()).unwrap();
    let module = tree
        .submodules
        .iter()
        .find(|module| module.path == "deps/native lib")
        .unwrap();
    assert!(module.initialized);
    assert!(module.dirty);
    assert!(module.url.is_some());
    assert_eq!(module.head_id, module.index_id);
    assert_eq!(module.index_id, module.workdir_id);
    assert!(tree.entries.iter().any(|entry| entry.path == module.path
        && entry.kind == FileTreeKind::Submodule
        && entry.worktree_status == 'M'));
    assert!(
        !tree
            .entries
            .iter()
            .any(|entry| entry.path.starts_with("deps/native lib/"))
    );
    directory.git(&["submodule", "deinit", "--force", "--", "deps/native lib"]);
    assert!(
        !Git2Backend
            .registered_submodule(directory.path(), "deps/native lib")
            .unwrap()
            .unwrap()
            .initialized
    );
    let tree = Git2Backend.tree(directory.path()).unwrap();
    let module = &tree.submodules[0];
    assert!(!module.initialized);
    assert!(module.workdir_id.is_none());
    assert!(module.index_id.is_some());
}

#[test]
fn tree_reuses_submodule_dirtiness_without_reading_child_status_again() {
    let source = TestDirectory::init();
    source.write("source.txt", "nested repository\n");
    source.commit_all("Nested commit");
    let directory = TestDirectory::init();
    directory.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "--quiet",
        source.path().to_str().unwrap(),
        "deps/child",
    ]);
    directory.commit_all("Add module");
    directory.write("deps/child/new.txt", "untracked child content\n");
    let snapshot = Git2Backend.inspect(directory.path()).unwrap();
    assert!(
        snapshot
            .changes
            .iter()
            .any(|change| change.path == "deps/child")
    );
    let child = git2::Repository::open(directory.path().join("deps/child")).unwrap();
    fs::write(child.path().join("index"), b"invalid child index fixture").unwrap();
    let tree = Git2Backend
        .tree_with_changes(directory.path(), &snapshot.changes)
        .unwrap();
    assert!(tree.submodules[0].dirty);
    assert!(tree.entries.iter().any(|entry| entry.path == "deps/child"
        && entry.kind == FileTreeKind::Submodule
        && entry.worktree_status == 'M'));
}

#[test]
fn tree_keeps_child_dirtiness_visible_when_parent_status_ignores_it() {
    let source = TestDirectory::init();
    source.write("source.txt", "nested repository\n");
    source.commit_all("Nested commit");
    let directory = TestDirectory::init();
    directory.git(&[
        "-c",
        "protocol.file.allow=always",
        "submodule",
        "add",
        "--quiet",
        source.path().to_str().unwrap(),
        "deps/child",
    ]);
    directory.commit_all("Add module");
    // libgit2's native status reads the global diff ignore setting; this
    // deliberately hides the child from parent status to exercise the tree's
    // independent dirty-child fallback.
    directory.git(&["config", "diff.ignoreSubmodules", "all"]);
    directory.write("deps/child/new.txt", "untracked child content\n");
    let snapshot = Git2Backend.inspect(directory.path()).unwrap();
    assert!(
        !snapshot
            .changes
            .iter()
            .any(|change| change.path == "deps/child")
    );
    let tree = Git2Backend
        .tree_with_changes(directory.path(), &snapshot.changes)
        .unwrap();
    assert!(tree.submodules[0].dirty);
    assert!(
        tree.entries
            .iter()
            .any(|entry| entry.path == "deps/child" && entry.worktree_status == 'M')
    );
    directory.git(&["config", "--unset", "diff.ignoreSubmodules"]);
    directory.git(&[
        "config",
        "--file",
        ".gitmodules",
        "submodule.deps/child.ignore",
        "all",
    ]);
    let snapshot = Git2Backend.inspect(directory.path()).unwrap();
    assert!(
        !snapshot
            .changes
            .iter()
            .any(|change| change.path == "deps/child")
    );
    let tree = Git2Backend
        .tree_with_changes(directory.path(), &snapshot.changes)
        .unwrap();
    assert!(tree.submodules[0].dirty);
    assert!(
        tree.entries
            .iter()
            .any(|entry| entry.path == "deps/child" && entry.worktree_status == 'M')
    );
}

#[test]
fn file_tree_combines_status_and_index_for_many_ignored_and_untracked_paths() {
    let directory = TestDirectory::init();
    directory.write(".gitignore", "build/\n");
    for index in 0..256 {
        directory.write(
            &format!("src/group{}/file{index}.txt", index / 32),
            "tracked\n",
        );
    }
    directory.commit_all("Many tracked files");
    for index in 0..128 {
        directory.write(&format!("new/nested/file{index}.txt"), "untracked\n");
        directory.write(&format!("build/cache{index}.tmp"), "ignored\n");
    }
    fs::remove_file(directory.path().join("src/group0/file0.txt")).unwrap();
    let snapshot = Git2Backend.inspect(directory.path()).unwrap();
    let tree = Git2Backend
        .tree_with_changes(directory.path(), &snapshot.changes)
        .unwrap();
    assert_eq!(
        tree.entries
            .iter()
            .filter(|entry| entry.path.starts_with("new/nested/file"))
            .count(),
        128
    );
    assert!(
        !tree
            .entries
            .iter()
            .any(|entry| entry.path.starts_with("build/"))
    );
    assert!(
        tree.entries
            .iter()
            .any(|entry| entry.path == "src/group0/file0.txt" && entry.worktree_status == 'D')
    );
    assert!(
        tree.entries
            .iter()
            .any(|entry| entry.path == "src/group7/file255.txt" && entry.worktree_status == ' ')
    );
}

#[test]
fn discovers_index_gitlinks_without_gitmodules_configuration() {
    let directory = TestDirectory::init();
    directory.write("file.txt", "base\n");
    directory.commit_all("Base");
    let output = directory.git(&["rev-parse", "HEAD"]);
    let id = String::from_utf8(output.stdout).unwrap();
    directory.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        "160000",
        id.trim(),
        "vendor/orphan",
    ]);
    let registration = Git2Backend
        .registered_submodule(directory.path(), "vendor/orphan")
        .unwrap()
        .unwrap();
    assert_eq!(registration.index_id.as_deref(), Some(id.trim()));
    assert!(!registration.initialized);
    let tree = Git2Backend.tree(directory.path()).unwrap();
    let module = tree
        .submodules
        .iter()
        .find(|module| module.path == "vendor/orphan")
        .unwrap();
    assert_eq!(module.name, "vendor/orphan");
    assert!(module.url.is_none());
    assert_eq!(module.index_id.as_deref(), Some(id.trim()));
    assert!(!module.initialized);
    assert!(
        tree.entries
            .iter()
            .any(|entry| entry.path == module.path && entry.kind == FileTreeKind::Submodule)
    );
}

#[test]
fn conflict_status_and_tree_are_readable_while_diff_reports_unmerged_index() {
    let directory = TestDirectory::init();
    directory.write("conflict.txt", "base\n");
    directory.commit_all("Base");
    directory.git(&["checkout", "--quiet", "-b", "other"]);
    directory.write("conflict.txt", "other\n");
    directory.commit_all("Other version");
    directory.git(&["checkout", "--quiet", "main"]);
    directory.write("conflict.txt", "main\n");
    directory.commit_all("Main version");
    let output = Command::new("git")
        .arg("-C")
        .arg(directory.path())
        .args([
            "-c",
            "user.name=GitViews Tests",
            "-c",
            "user.email=gitviews@example.invalid",
            "merge",
            "other",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let snapshot = Git2Backend.inspect(directory.path()).unwrap();
    let conflict = snapshot
        .changes
        .iter()
        .find(|change| change.path == "conflict.txt")
        .unwrap();
    assert_eq!(
        (conflict.index_status, conflict.worktree_status),
        ('U', 'U')
    );
    let tree = Git2Backend.tree(directory.path()).unwrap();
    assert!(
        tree.entries
            .iter()
            .any(|entry| entry.path == "conflict.txt" && entry.index_status == 'U')
    );
    assert!(matches!(
        Git2Backend.diff(directory.path(), "conflict.txt", false),
        Err(GitError::Unsupported { .. })
    ));
}

#[test]
fn native_backend_does_not_require_git_in_path() {
    const REPOSITORY_ENV: &str = "GITVIEWS_NATIVE_BACKEND_TEST_REPOSITORY";
    if let Some(path) = std::env::var_os(REPOSITORY_ENV) {
        let path = Path::new(&path);
        assert_eq!(Git2Backend.inspect(path).unwrap().recent_commits.len(), 1);
        assert_eq!(Git2Backend.details(path).unwrap().branches.len(), 1);
        assert!(!Git2Backend.tree(path).unwrap().entries.is_empty());
        assert_eq!(
            Git2Backend.diff(path, "file.txt", false).unwrap().new_text,
            "working\n"
        );
        return;
    }
    let directory = TestDirectory::init();
    directory.write("file.txt", "base\n");
    directory.commit_all("Base");
    directory.write("file.txt", "working\n");
    let empty_path = TestDirectory::new();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "native_backend_does_not_require_git_in_path",
            "--nocapture",
        ])
        .env("PATH", empty_path.path())
        .env(REPOSITORY_ENV, directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn nested_untracked_repositories_are_tree_boundaries() {
    let directory = TestDirectory::init();
    directory.git(&["init", "--quiet", "--initial-branch=nested", "nested"]);
    directory.write(
        "nested/inside.txt",
        "contents belonging to another repository\n",
    );
    let tree = Git2Backend.tree(directory.path()).unwrap();
    assert!(tree.entries.iter().any(|entry| entry.path == "nested"
        && entry.kind == FileTreeKind::Directory
        && entry.worktree_status == '?'));
    assert!(
        !tree
            .entries
            .iter()
            .any(|entry| entry.path.starts_with("nested/"))
    );
}

#[test]
fn source_reads_utf8_and_bom_encoded_chinese_with_empty_files() {
    let directory = TestDirectory::init();
    let content = "中文🙂\r\n第二行\n";
    let mut utf8 = vec![0xef, 0xbb, 0xbf];
    utf8.extend_from_slice(content.as_bytes());
    let mut little = vec![0xff, 0xfe];
    let mut big = vec![0xfe, 0xff];
    for unit in content.encode_utf16() {
        little.extend_from_slice(&unit.to_le_bytes());
        big.extend_from_slice(&unit.to_be_bytes());
    }
    for (name, bytes, encoding) in [
        ("utf8.txt", content.as_bytes().to_vec(), TextEncoding::Utf8),
        ("utf8-bom.txt", utf8, TextEncoding::Utf8Bom),
        ("utf16-le.txt", little, TextEncoding::Utf16Le),
        ("utf16-be.txt", big, TextEncoding::Utf16Be),
    ] {
        fs::write(directory.path().join(name), bytes).unwrap();
        let source = Git2Backend.source(directory.path(), name, None).unwrap();
        assert_eq!(source.text, content);
        assert_eq!(source.encoding, encoding);
        assert_eq!(source.line_count, 3);
    }
    directory.write("empty.txt", "");
    let empty = Git2Backend
        .source(directory.path(), "empty.txt", None)
        .unwrap();
    assert!(empty.text.is_empty());
    assert_eq!(empty.line_count, 1);
    directory.commit_all("Encoded files");
    assert_eq!(
        Git2Backend
            .source(directory.path(), "utf16-le.txt", Some("HEAD"))
            .unwrap()
            .text,
        content
    );
    assert_eq!(
        Git2Backend
            .source(directory.path(), "utf16-be.txt", Some(":index"))
            .unwrap()
            .encoding,
        TextEncoding::Utf16Be
    );
}

#[test]
fn source_selects_working_index_and_historical_versions_independently_of_diff() {
    let directory = TestDirectory::init();
    directory.write("file.txt", "committed\n");
    directory.commit_all("Initial");
    directory.write("file.txt", "staged\n");
    directory.git(&["add", "--", "file.txt"]);
    directory.write("file.txt", "working\n");
    assert_eq!(
        Git2Backend
            .source(directory.path(), "file.txt", None)
            .unwrap()
            .text,
        "working\n"
    );
    assert_eq!(
        Git2Backend
            .source(directory.path(), "file.txt", Some(":index"))
            .unwrap()
            .text,
        "staged\n"
    );
    assert_eq!(
        Git2Backend
            .source(directory.path(), "file.txt", Some("HEAD"))
            .unwrap()
            .text,
        "committed\n"
    );
    fs::remove_file(directory.path().join("file.txt")).unwrap();
    assert_eq!(
        Git2Backend
            .source(directory.path(), "file.txt", Some("HEAD"))
            .unwrap()
            .text,
        "committed\n"
    );
    // Larger than the diff's 2 MiB cap, while remaining below the source
    // editor's separate 100,000-line and 16,384-character-per-line budgets.
    directory.write("large.txt", &"line with sufficient width\n".repeat(90_000));
    assert!(
        Git2Backend
            .source(directory.path(), "large.txt", None)
            .unwrap()
            .text
            .len()
            > 2 * 1024 * 1024
    );
    assert!(matches!(
        Git2Backend.diff(directory.path(), "large.txt", false),
        Err(GitError::FileTooLarge { .. })
    ));
}

#[test]
fn source_and_diff_reject_100001_short_lines_in_worktree_index_and_history() {
    let directory = TestDirectory::init();
    let content = "x\n".repeat(100_000) + "x";
    assert!(content.len() < 2 * 1024 * 1024);
    directory.write("many-lines.txt", &content);
    assert!(matches!(
        Git2Backend.source(directory.path(), "many-lines.txt", None),
        Err(GitError::LineCountLimit {
            limit_lines: 100_000,
            ..
        })
    ));
    assert!(matches!(
        Git2Backend.diff(directory.path(), "many-lines.txt", false),
        Err(GitError::LineCountLimit {
            limit_lines: 100_000,
            ..
        })
    ));
    directory.git(&["add", "--", "many-lines.txt"]);
    assert!(matches!(
        Git2Backend.source(directory.path(), "many-lines.txt", Some(":index")),
        Err(GitError::LineCountLimit {
            limit_lines: 100_000,
            ..
        })
    ));
    assert!(matches!(
        Git2Backend.diff(directory.path(), "many-lines.txt", true),
        Err(GitError::LineCountLimit {
            limit_lines: 100_000,
            ..
        })
    ));
    directory.commit_all("many short lines stay bounded");
    assert!(matches!(
        Git2Backend.source(directory.path(), "many-lines.txt", Some("HEAD")),
        Err(GitError::LineCountLimit {
            limit_lines: 100_000,
            ..
        })
    ));
    let id = git2::Repository::open(directory.path())
        .unwrap()
        .head()
        .unwrap()
        .target()
        .unwrap();
    assert!(matches!(
        Git2Backend.commit_diff(directory.path(), &id.to_string(), "many-lines.txt"),
        Err(GitError::LineCountLimit {
            limit_lines: 100_000,
            ..
        })
    ));
}

#[test]
fn source_and_diff_reject_abnormal_text_and_long_lines_before_rendering() {
    let directory = TestDirectory::init();
    fs::write(directory.path().join("binary.bin"), [0, 1, 2]).unwrap();
    assert!(matches!(
        Git2Backend.source(directory.path(), "binary.bin", None),
        Err(GitError::BinaryFile { .. })
    ));
    fs::write(
        directory.path().join("invalid-utf16.txt"),
        [0xff, 0xfe, 0x00, 0xd8],
    )
    .unwrap();
    assert!(matches!(
        Git2Backend.source(directory.path(), "invalid-utf16.txt", None),
        Err(GitError::InvalidEncoding { .. })
    ));
    fs::write(
        directory.path().join("truncated-utf16.txt"),
        [0xff, 0xfe, 0x01],
    )
    .unwrap();
    assert!(matches!(
        Git2Backend.source(directory.path(), "truncated-utf16.txt", None),
        Err(GitError::InvalidEncoding { .. })
    ));
    directory.write("long.txt", &"字".repeat(16_385));
    assert!(matches!(
        Git2Backend.source(directory.path(), "long.txt", None),
        Err(GitError::LineTooLong { .. })
    ));
    assert!(matches!(
        Git2Backend.diff(directory.path(), "long.txt", false),
        Err(GitError::LineTooLong { .. })
    ));
    fs::File::create(directory.path().join("huge.txt"))
        .unwrap()
        .set_len(8 * 1024 * 1024 + 1)
        .unwrap();
    assert!(matches!(
        Git2Backend.source(directory.path(), "huge.txt", None),
        Err(GitError::FileTooLarge { .. })
    ));
    for file in [
        "../outside",
        ".GiT/config",
        "C:/Windows/file",
        "folder/../../secret",
    ] {
        assert!(matches!(
            Git2Backend.source(directory.path(), file, None),
            Err(GitError::InvalidPath { .. })
        ));
    }
}

#[test]
fn history_pages_beyond_twenty_commits_without_gaps_or_duplicates() {
    let directory = TestDirectory::init();
    // Create real commits through libgit2 to keep this pagination fixture fast.
    let repo = git2::Repository::open(directory.path()).unwrap();
    let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let mut previous = None;
    for number in 0..33 {
        let signature = git2::Signature::new(
            "History Tester",
            "history@example.invalid",
            &git2::Time::new(1_700_000_000 + number, 0),
        )
        .unwrap();
        let parents: Vec<_> = previous.as_ref().into_iter().collect();
        let id = repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                &format!("Commit {number}"),
                &tree,
                &parents,
            )
            .unwrap();
        previous = Some(repo.find_commit(id).unwrap());
    }
    let first = Git2Backend.history(directory.path(), 0, 20).unwrap();
    let second = Git2Backend.history(directory.path(), 20, 20).unwrap();
    assert_eq!(first.commits.len(), 20);
    assert!(first.has_more);
    assert_eq!(second.commits.len(), 13);
    assert!(!second.has_more);
    assert_eq!(first.commits[0].subject, "Commit 32");
    assert_eq!(second.commits[0].subject, "Commit 12");
    let ids: std::collections::HashSet<_> = first
        .commits
        .iter()
        .chain(&second.commits)
        .map(|commit| &commit.id)
        .collect();
    assert_eq!(ids.len(), 33);
    assert!(
        Git2Backend
            .history(directory.path(), 33, 20)
            .unwrap()
            .commits
            .is_empty()
    );
    assert!(matches!(
        Git2Backend.history(directory.path(), 0, 0),
        Err(GitError::InvalidRequest { .. })
    ));
    let unborn = TestDirectory::init();
    assert!(
        Git2Backend
            .history(unborn.path(), 0, 20)
            .unwrap()
            .commits
            .is_empty()
    );
}

#[test]
fn selected_commits_show_initial_changes_renames_and_deletions() {
    let directory = TestDirectory::init();
    directory.write("before.txt", "initial\n");
    directory.commit_all("Initial commit\n\nFull commit description.");
    let initial = Git2Backend
        .history(directory.path(), 0, 20)
        .unwrap()
        .commits[0]
        .id
        .clone();
    let details = Git2Backend.commit(directory.path(), &initial).unwrap();
    assert!(details.parent_ids.is_empty());
    assert!(details.message.contains("Full commit description."));
    assert!(
        details
            .changes
            .iter()
            .any(|file| file.path == "before.txt" && file.index_status == 'A')
    );
    let diff = Git2Backend
        .commit_diff(directory.path(), &initial, "before.txt")
        .unwrap();
    assert!(diff.old_text.is_empty());
    assert_eq!(diff.new_text, "initial\n");
    directory.git(&["mv", "--", "before.txt", "after.txt"]);
    directory.commit_all("Rename");
    let renamed = Git2Backend.history(directory.path(), 0, 1).unwrap().commits[0]
        .id
        .clone();
    let details = Git2Backend.commit(directory.path(), &renamed).unwrap();
    assert_eq!(details.parent_ids, vec![initial.clone()]);
    assert!(
        details
            .changes
            .iter()
            .any(|file| file.path == "after.txt" && file.index_status == 'R')
    );
    let diff = Git2Backend
        .commit_diff(directory.path(), &renamed, "after.txt")
        .unwrap();
    assert_eq!(diff.old_text, "initial\n");
    assert_eq!(diff.new_text, "initial\n");
    directory.git(&["rm", "--", "after.txt"]);
    directory.commit_all("Delete");
    let deleted = Git2Backend.history(directory.path(), 0, 1).unwrap().commits[0]
        .id
        .clone();
    let diff = Git2Backend
        .commit_diff(directory.path(), &deleted, "after.txt")
        .unwrap();
    assert_eq!(diff.old_text, "initial\n");
    assert!(diff.new_text.is_empty());
    assert_eq!(
        Git2Backend
            .source(directory.path(), "before.txt", Some(&initial))
            .unwrap()
            .text,
        "initial\n"
    );
}

#[test]
fn tree_reports_recoverable_bad_configuration_while_retaining_healthy_files() {
    let directory = TestDirectory::init();
    directory.write("healthy.txt", "still visible\n");
    directory.commit_all("Healthy");
    directory.write(".gitmodules", "[submodule invalid syntax\n");
    let tree = Git2Backend.tree(directory.path()).unwrap();
    assert!(tree.entries.iter().any(|entry| entry.path == "healthy.txt"));
    assert!(!tree.warnings.is_empty());
}

#[cfg(windows)]
#[test]
#[ignore = "creates a Windows junction; run explicitly when validating filesystem boundaries"]
fn tree_does_not_follow_windows_junctions() {
    let directory = TestDirectory::init();
    let outside = TestDirectory::new();
    outside.write("outside.txt", "not part of the selected repository\n");
    let link = directory.path().join("outside junction");
    let output = Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&link)
        .arg(outside.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = Git2Backend.tree(directory.path()).unwrap();
    assert!(
        tree.entries
            .iter()
            .any(|entry| entry.path == "outside junction" && entry.kind == FileTreeKind::Symlink)
    );
    assert!(
        !tree
            .entries
            .iter()
            .any(|entry| entry.path.starts_with("outside junction/"))
    );
    assert!(matches!(
        Git2Backend.diff(directory.path(), "outside junction/outside.txt", false),
        Err(GitError::InvalidPath { .. })
    ));
}
