//! Local libgit2 fixtures only. Import the implementation directly so these
//! tests also run independently of the UI and backend forwarding layers.

#[allow(dead_code)]
#[path = "../src/checkout.rs"]
mod checkout;

use checkout::{CheckoutErrorKind, CheckoutRequest};
use git2::{
    BranchType, Oid, Repository, Signature, Time, WorktreeAddOptions, build::CheckoutBuilder,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    directory: PathBuf,
    parent: PathBuf,
    root: PathBuf,
    repository: Repository,
    main: Oid,
    feature: Oid,
}

impl Fixture {
    fn new() -> Self {
        let parent = fs::canonicalize(std::env::temp_dir()).unwrap();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = parent.join(format!(
            "gitviews-checkout-{}-{timestamp}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let root = directory.join("中文 仓库");
        let repository = Repository::init(&root).unwrap();
        repository
            .config()
            .unwrap()
            .set_bool("core.autocrlf", false)
            .unwrap();
        let main = commit(
            &repository,
            &[("note.txt", "main\n"), (".gitignore", "ignored.txt\n")],
            None,
            "main",
        );
        let feature = commit(
            &repository,
            &[("note.txt", "feature\n"), (".gitignore", "ignored.txt\n")],
            Some(main),
            "feature",
        );
        repository
            .reference("refs/heads/main", main, false, "fixture")
            .unwrap();
        repository
            .reference("refs/heads/feature", feature, false, "fixture")
            .unwrap();
        repository.set_head("refs/heads/main").unwrap();
        repository
            .checkout_head(Some(CheckoutBuilder::new().safe()))
            .unwrap();
        Self {
            directory,
            parent,
            root,
            repository,
            main,
            feature,
        }
    }

    fn request(&self, branch: &str, remote: bool) -> CheckoutRequest {
        let head = self.repository.head().unwrap();
        CheckoutRequest {
            branch: branch.into(),
            remote,
            expected_branch: head.is_branch().then(|| head.shorthand().unwrap().into()),
            expected_head: head.target().map(|id| id.to_string()),
        }
    }

    fn remote(&self, oid: Oid) {
        self.repository
            .remote("origin", "https://example.invalid/never-connected")
            .unwrap();
        self.repository
            .reference("refs/remotes/origin/feature", oid, false, "fixture")
            .unwrap();
    }

    fn remove_feature(&self) {
        self.repository
            .find_reference("refs/heads/feature")
            .unwrap()
            .delete()
            .unwrap();
    }

    fn assert_main(&self) {
        let reopened = Repository::open(&self.root).unwrap();
        assert_eq!(reopened.head().unwrap().name().unwrap(), "refs/heads/main");
        assert_eq!(
            reopened.refname_to_id("refs/heads/main").unwrap(),
            self.main
        );
        assert!(!reopened.path().join("HEAD.lock").exists());
    }

    fn clean(&self) {
        let mut options = git2::StatusOptions::new();
        options.include_untracked(true).recurse_untracked_dirs(true);
        assert!(
            Repository::open(&self.root)
                .unwrap()
                .statuses(Some(&mut options))
                .unwrap()
                .is_empty()
        );
        assert!(!self.repository.path().join("index.lock").exists());
        assert!(fs::read_dir(self.repository.path()).unwrap().all(|entry| {
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
                .is_some_and(|name| name.to_string_lossy().starts_with("gitviews-checkout-"))
        {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

fn commit(
    repository: &Repository,
    files: &[(&str, &str)],
    parent: Option<Oid>,
    message: &str,
) -> Oid {
    let mut builder = repository.treebuilder(None).unwrap();
    for (path, text) in files {
        builder
            .insert(path, repository.blob(text.as_bytes()).unwrap(), 0o100644)
            .unwrap();
    }
    let tree = repository.find_tree(builder.write().unwrap()).unwrap();
    let signature = Signature::new(
        "Checkout Fixture",
        "checkout@example.invalid",
        &Time::new(1_700_000_000, 0),
    )
    .unwrap();
    let parent = parent.map(|id| repository.find_commit(id).unwrap());
    let parents = parent.as_ref().into_iter().collect::<Vec<_>>();
    repository
        .commit(None, &signature, &signature, message, &tree, &parents)
        .unwrap()
}

#[test]
fn local_switch_updates_head_files_and_index_without_moving_branches() {
    let fixture = Fixture::new();
    let outcome = checkout::execute(&fixture.root, &fixture.request("feature", false)).unwrap();
    assert_eq!(outcome.branch, "feature");
    assert_eq!(outcome.head, fixture.feature.to_string());
    assert!(!outcome.created);
    let repository = Repository::open(&fixture.root).unwrap();
    assert_eq!(
        repository.head().unwrap().name().unwrap(),
        "refs/heads/feature"
    );
    assert_eq!(
        repository.refname_to_id("refs/heads/main").unwrap(),
        fixture.main
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
        "feature\n"
    );
    fixture.clean();
    checkout::execute(&fixture.root, &fixture.request("refs/heads/main", false)).unwrap();
    fixture.assert_main();
    fixture.clean();
}

#[test]
fn remote_switch_creates_a_local_tracking_branch_and_preserves_other_config() {
    let fixture = Fixture::new();
    fixture.remote(fixture.feature);
    fixture.remove_feature();
    fixture
        .repository
        .config()
        .unwrap()
        .set_str("branch.feature.description", "keep this description")
        .unwrap();
    let outcome = checkout::execute(
        &fixture.root,
        &fixture.request("refs/remotes/origin/feature", true),
    )
    .unwrap();
    assert!(outcome.created);
    assert_eq!(outcome.upstream.as_deref(), Some("origin/feature"));
    let repository = Repository::open(&fixture.root).unwrap();
    let local = repository
        .find_branch("feature", BranchType::Local)
        .unwrap();
    assert_eq!(
        local.upstream().unwrap().get().name().unwrap(),
        "refs/remotes/origin/feature"
    );
    assert_eq!(
        repository
            .config()
            .unwrap()
            .get_string("branch.feature.description")
            .unwrap(),
        "keep this description"
    );
    assert!(!repository.commondir().join("config.lock").exists());
    assert_eq!(
        fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
        "feature\n"
    );
    fixture.clean();
}

#[test]
fn remote_switch_reuses_matching_local_tracking_branch_without_resetting_its_tip() {
    let fixture = Fixture::new();
    let remote_tip = commit(
        &fixture.repository,
        &[("note.txt", "remote future\n")],
        Some(fixture.feature),
        "future",
    );
    fixture.remote(remote_tip);
    fixture
        .repository
        .find_branch("feature", BranchType::Local)
        .unwrap()
        .set_upstream(Some("origin/feature"))
        .unwrap();
    let outcome =
        checkout::execute(&fixture.root, &fixture.request("origin/feature", true)).unwrap();
    assert!(!outcome.created);
    assert_eq!(outcome.head, fixture.feature.to_string());
    assert_eq!(
        Repository::open(&fixture.root)
            .unwrap()
            .head()
            .unwrap()
            .target(),
        Some(fixture.feature)
    );
    fixture.clean();
}

#[test]
fn remote_switch_refuses_a_conflicting_local_name_without_overwriting_config() {
    let fixture = Fixture::new();
    fixture.remote(fixture.feature);
    let before = fs::read(fixture.repository.commondir().join("config")).unwrap();
    let error =
        checkout::execute(&fixture.root, &fixture.request("origin/feature", true)).unwrap_err();
    assert_eq!(error.kind, CheckoutErrorKind::BranchConflict);
    fixture.assert_main();
    assert_eq!(
        fs::read(fixture.repository.commondir().join("config")).unwrap(),
        before
    );
    fixture.clean();
}

#[test]
fn colliding_modified_and_staged_files_block_switching_and_are_preserved() {
    for state in ["modified", "staged"] {
        let fixture = Fixture::new();
        let path = if state == "untracked" {
            "new.txt"
        } else {
            "note.txt"
        };
        fs::write(fixture.root.join(path), "user content\n").unwrap();
        if state == "staged" {
            let mut index = fixture.repository.index().unwrap();
            index.add_path(Path::new(path)).unwrap();
            index.write().unwrap();
        }
        let index = fs::read(fixture.repository.path().join("index")).unwrap();
        let error =
            checkout::execute(&fixture.root, &fixture.request("feature", false)).unwrap_err();
        assert_eq!(error.kind, CheckoutErrorKind::DirtyWorkingTree, "{state}");
        assert!(
            error.message.contains("note.txt"),
            "{state}: {}",
            error.message
        );
        fixture.assert_main();
        assert_eq!(
            fs::read_to_string(fixture.root.join(path)).unwrap(),
            "user content\n"
        );
        assert_eq!(
            fs::read(fixture.repository.path().join("index")).unwrap(),
            index
        );
    }
}

#[test]
fn nonconflicting_tracked_staged_and_untracked_edits_survive_switching() {
    for state in ["modified", "staged", "untracked"] {
        let fixture = Fixture::new();
        let main = commit(
            &fixture.repository,
            &[("note.txt", "main\n"), ("shared.txt", "original\n")],
            Some(fixture.main),
            "main with shared file",
        );
        let feature = commit(
            &fixture.repository,
            &[("note.txt", "feature\n"), ("shared.txt", "original\n")],
            Some(main),
            "feature with shared file",
        );
        fixture
            .repository
            .reference("refs/heads/main", main, true, "fixture")
            .unwrap();
        fixture
            .repository
            .reference("refs/heads/feature", feature, true, "fixture")
            .unwrap();
        fixture
            .repository
            .checkout_head(Some(CheckoutBuilder::new().safe()))
            .unwrap();
        let path = if state == "untracked" {
            "local.txt"
        } else {
            "shared.txt"
        };
        fs::write(fixture.root.join(path), "local content\n").unwrap();
        let staged = if state == "staged" {
            let mut index = fixture.repository.index().unwrap();
            index.add_path(Path::new(path)).unwrap();
            index.write().unwrap();
            Some(index.get_path(Path::new(path), 0).unwrap().id)
        } else {
            None
        };
        let outcome = checkout::execute(&fixture.root, &fixture.request("feature", false))
            .unwrap_or_else(|error| panic!("{state}: {error}"));
        assert_eq!(outcome.head, feature.to_string());
        assert_eq!(
            fs::read_to_string(fixture.root.join(path)).unwrap(),
            "local content\n",
            "{state}"
        );
        assert_eq!(
            fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
            "feature\n"
        );
        if let Some(id) = staged {
            assert_eq!(
                Repository::open(&fixture.root)
                    .unwrap()
                    .index()
                    .unwrap()
                    .get_path(Path::new(path), 0)
                    .unwrap()
                    .id,
                id
            );
        }
    }
}

#[test]
fn untracked_file_colliding_with_target_is_preserved() {
    let fixture = Fixture::new();
    let feature = commit(
        &fixture.repository,
        &[("note.txt", "feature\n"), ("local.txt", "target content\n")],
        Some(fixture.main),
        "feature adds local filename",
    );
    fixture
        .repository
        .reference("refs/heads/feature", feature, true, "fixture")
        .unwrap();
    fs::write(fixture.root.join("local.txt"), "precious local file\n").unwrap();
    let error = checkout::execute(&fixture.root, &fixture.request("feature", false)).unwrap_err();
    assert_eq!(error.kind, CheckoutErrorKind::DirtyWorkingTree);
    fixture.assert_main();
    assert_eq!(
        fs::read_to_string(fixture.root.join("local.txt")).unwrap(),
        "precious local file\n"
    );
}

#[test]
fn hidden_tracked_edits_are_not_overwritten_by_checkout() {
    for assume_unchanged in [true, false] {
        let fixture = Fixture::new();
        let mut index = fixture.repository.index().unwrap();
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
        fs::write(fixture.root.join("note.txt"), "hidden local content\n").unwrap();
        let index_before = fs::read(fixture.repository.path().join("index")).unwrap();
        let error =
            checkout::execute(&fixture.root, &fixture.request("feature", false)).unwrap_err();
        assert_eq!(error.kind, CheckoutErrorKind::DirtyWorkingTree);
        fixture.assert_main();
        assert_eq!(
            fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
            "hidden local content\n"
        );
        assert_eq!(
            fs::read(fixture.repository.path().join("index")).unwrap(),
            index_before
        );
    }
}

#[test]
fn target_matching_local_edits_deletions_and_renames_are_preserved_before_checkout() {
    for state in [
        "modified",
        "staged",
        "deleted",
        "staged-deleted",
        "renamed",
        "ignored",
        "directory",
    ] {
        let fixture = Fixture::new();
        let target = if state.contains("deleted") || state == "renamed" {
            commit(
                &fixture.repository,
                &[(".gitignore", "ignored.txt\n")],
                Some(fixture.main),
                "remove note",
            )
        } else if state == "ignored" {
            commit(
                &fixture.repository,
                &[
                    ("note.txt", "feature\n"),
                    (".gitignore", "ignored.txt\n"),
                    ("ignored.txt", "same local and target\n"),
                ],
                Some(fixture.main),
                "track ignored file",
            )
        } else {
            fixture.feature
        };
        fixture.remote(target);
        fixture.remove_feature();
        let config_path = fixture.repository.commondir().join("config");
        let mut config = fs::read_to_string(&config_path).unwrap();
        config.push_str("\n[includeIf \"onbranch:feature\"]\n\tpath = feature.cfg\n");
        fs::write(&config_path, config).unwrap();
        fs::write(
            fixture.repository.commondir().join("feature.cfg"),
            "[branch \"feature\"]\n\tremote = nonexistent\n\tmerge = refs/heads/feature\n",
        )
        .unwrap();
        if state == "directory" {
            fs::remove_file(fixture.root.join("note.txt")).unwrap();
            fs::create_dir(fixture.root.join("note.txt")).unwrap();
            fs::write(
                fixture.root.join("note.txt/local.txt"),
                "local directory content\n",
            )
            .unwrap();
        } else if state.contains("deleted") {
            fs::remove_file(fixture.root.join("note.txt")).unwrap();
        } else if state == "renamed" {
            fs::rename(
                fixture.root.join("note.txt"),
                fixture.root.join("renamed.txt"),
            )
            .unwrap();
        } else if state == "ignored" {
            fs::write(fixture.root.join("ignored.txt"), "same local and target\n").unwrap();
        } else {
            fs::write(fixture.root.join("note.txt"), "feature\n").unwrap();
        }
        if state == "staged" || state == "staged-deleted" || state == "renamed" {
            let mut index = fixture.repository.index().unwrap();
            if state == "staged" {
                index.add_path(Path::new("note.txt")).unwrap();
            } else {
                index.remove_path(Path::new("note.txt")).unwrap();
                if state == "renamed" {
                    index.add_path(Path::new("renamed.txt")).unwrap();
                }
            }
            index.write().unwrap();
        }
        let before_index = fs::read(fixture.repository.path().join("index")).unwrap();
        let before_config = fs::read(&config_path).unwrap();
        let error =
            checkout::execute(&fixture.root, &fixture.request("origin/feature", true)).unwrap_err();
        fixture.assert_main();
        if state == "directory" {
            assert!(fixture.root.join("note.txt").is_dir());
            assert_eq!(
                fs::read_to_string(fixture.root.join("note.txt/local.txt")).unwrap(),
                "local directory content\n"
            );
        } else if state.contains("deleted") || state == "renamed" {
            assert!(!fixture.root.join("note.txt").exists(), "{state}");
        } else {
            assert_eq!(
                fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
                if state == "ignored" {
                    "main\n"
                } else {
                    "feature\n"
                },
                "{state}"
            );
        }
        if state == "renamed" {
            assert_eq!(
                fs::read_to_string(fixture.root.join("renamed.txt")).unwrap(),
                "main\n"
            );
        }
        if state == "ignored" {
            assert_eq!(
                fs::read_to_string(fixture.root.join("ignored.txt")).unwrap(),
                "same local and target\n"
            );
        }
        assert_eq!(
            fs::read(fixture.repository.path().join("index")).unwrap(),
            before_index,
            "{state}"
        );
        assert_eq!(fs::read(&config_path).unwrap(), before_config, "{state}");
        assert!(
            fixture
                .repository
                .find_branch("feature", BranchType::Local)
                .is_err()
        );
        assert_eq!(
            error.kind,
            CheckoutErrorKind::DirtyWorkingTree,
            "{state}: {error}"
        );
    }
}

#[test]
fn overlap_preflight_treats_wildcard_characters_in_filenames_literally() {
    let fixture = Fixture::new();
    let main = commit(
        &fixture.repository,
        &[("note.txt", "main\n"), ("special[1].txt", "base\n")],
        Some(fixture.main),
        "main literal filename",
    );
    let feature = commit(
        &fixture.repository,
        &[("note.txt", "main\n"), ("special[1].txt", "target\n")],
        Some(main),
        "feature literal filename",
    );
    fixture
        .repository
        .reference("refs/heads/main", main, true, "fixture")
        .unwrap();
    fixture
        .repository
        .reference("refs/heads/feature", feature, true, "fixture")
        .unwrap();
    fixture
        .repository
        .checkout_head(Some(CheckoutBuilder::new().safe()))
        .unwrap();
    fs::write(fixture.root.join("special[1].txt"), "target\n").unwrap();
    let index = fs::read(fixture.repository.path().join("index")).unwrap();
    let error = checkout::execute(&fixture.root, &fixture.request("feature", false)).unwrap_err();
    assert_eq!(error.kind, CheckoutErrorKind::DirtyWorkingTree);
    assert!(error.message.contains("special[1].txt"));
    assert_eq!(fixture.repository.head().unwrap().target(), Some(main));
    assert_eq!(
        fs::read_to_string(fixture.root.join("special[1].txt")).unwrap(),
        "target\n"
    );
    assert_eq!(
        fs::read(fixture.repository.path().join("index")).unwrap(),
        index
    );
}

#[test]
fn an_existing_index_lock_blocks_checkout_before_refs_or_files_change() {
    let fixture = Fixture::new();
    let path = fixture.repository.path().join("index.lock");
    fs::write(&path, "external index writer").unwrap();
    let index = fs::read(fixture.repository.path().join("index")).unwrap();
    let error = checkout::execute(&fixture.root, &fixture.request("feature", false)).unwrap_err();
    assert_eq!(error.kind, CheckoutErrorKind::ConcurrentChange);
    fixture.assert_main();
    assert_eq!(
        fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
        "main\n"
    );
    assert_eq!(
        fs::read(fixture.repository.path().join("index")).unwrap(),
        index
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "external index writer");
    fs::remove_file(path).unwrap();
    fixture.clean();
}

#[test]
fn concurrent_external_git_add_is_blocked_until_the_checkout_index_is_published() {
    use std::process::Command;
    let fixture = Fixture::new();
    fs::write(fixture.root.join("local.txt"), "untracked user data\n").unwrap();
    let mut attempted = false;
    let mut progress = |_: Option<&Path>, completed: usize, _: usize| {
        if !attempted && completed > 0 {
            let root = fixture.root.to_string_lossy();
            let root = root.strip_prefix(r"\\?\").unwrap_or(&root);
            let mut command = Command::new("git");
            command
                .args([
                    "-c",
                    "core.hooksPath=",
                    "-C",
                    root,
                    "add",
                    "--",
                    "local.txt",
                ])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env(
                    "GIT_CONFIG_GLOBAL",
                    if cfg!(windows) { "NUL" } else { "/dev/null" },
                )
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
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("index.lock"));
            attempted = true;
        }
    };
    let outcome = checkout::execute_with_progress(
        &fixture.root,
        &fixture.request("feature", false),
        &mut progress,
    )
    .unwrap();
    assert!(attempted);
    assert_eq!(outcome.head, fixture.feature.to_string());
    let repository = Repository::open(&fixture.root).unwrap();
    assert!(
        repository
            .index()
            .unwrap()
            .get_path(Path::new("local.txt"), 0)
            .is_none()
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("local.txt")).unwrap(),
        "untracked user data\n"
    );
    fs::remove_file(fixture.root.join("local.txt")).unwrap();
    fixture.clean();
}

#[test]
fn linked_worktree_checkout_publishes_only_its_own_index_and_preserves_version_four() {
    let fixture = Fixture::new();
    let reference = fixture
        .repository
        .find_reference("refs/heads/feature")
        .unwrap();
    let mut options = WorktreeAddOptions::new();
    options.reference(Some(&reference));
    let linked = fixture.directory.join("linked");
    fixture
        .repository
        .worktree("linked", &linked, Some(&options))
        .unwrap();
    let target = commit(
        &fixture.repository,
        &[("note.txt", "third\n"), (".gitignore", "ignored.txt\n")],
        Some(fixture.feature),
        "third branch",
    );
    fixture
        .repository
        .reference("refs/heads/third", target, false, "fixture")
        .unwrap();
    let main_index = fs::read(fixture.repository.path().join("index")).unwrap();
    let repository = Repository::open(&linked).unwrap();
    let mut index = repository.index().unwrap();
    index.set_version(4).unwrap();
    index.write().unwrap();
    let outcome = checkout::execute(
        &linked,
        &CheckoutRequest {
            branch: "third".into(),
            remote: false,
            expected_branch: Some("feature".into()),
            expected_head: Some(fixture.feature.to_string()),
        },
    )
    .unwrap();
    assert_eq!(outcome.head, target.to_string());
    assert_eq!(
        fs::read(fixture.repository.path().join("index")).unwrap(),
        main_index
    );
    assert!(!repository.path().join("index.lock").exists());
    assert!(!fixture.repository.path().join("index.lock").exists());
    assert_eq!(
        Repository::open(&linked)
            .unwrap()
            .index()
            .unwrap()
            .version(),
        4
    );
    assert_eq!(
        fs::read_to_string(linked.join("note.txt")).unwrap(),
        "third\n"
    );
    fixture.assert_main();
    fixture.clean();
}

#[test]
fn shadow_index_keeps_case_insensitive_checkout_and_case_only_rename_semantics() {
    let fixture = Fixture::new();
    fixture
        .repository
        .config()
        .unwrap()
        .set_bool("core.ignorecase", true)
        .unwrap();
    fixture
        .repository
        .config()
        .unwrap()
        .set_bool("core.filemode", false)
        .unwrap();
    fixture
        .repository
        .config()
        .unwrap()
        .set_bool("core.symlinks", false)
        .unwrap();
    let target = commit(
        &fixture.repository,
        &[
            ("NOTE.txt", "case renamed\n"),
            (".gitignore", "ignored.txt\n"),
        ],
        Some(fixture.main),
        "case-only rename",
    );
    fixture
        .repository
        .reference("refs/heads/feature", target, true, "fixture")
        .unwrap();
    let before_config = fs::read(fixture.repository.commondir().join("config")).unwrap();
    checkout::execute(&fixture.root, &fixture.request("feature", false)).unwrap();
    assert_eq!(
        fs::read_to_string(fixture.root.join("NOTE.txt")).unwrap(),
        "case renamed\n"
    );
    assert_eq!(
        fs::read(fixture.repository.commondir().join("config")).unwrap(),
        before_config
    );
    let repository = Repository::open(&fixture.root).unwrap();
    assert!(
        repository
            .index()
            .unwrap()
            .iter()
            .any(|entry| entry.path == b"NOTE.txt")
    );
    assert!(
        !repository
            .index()
            .unwrap()
            .iter()
            .any(|entry| entry.path == b"note.txt")
    );
    fixture.clean();
}

#[test]
fn ignored_collision_is_preserved_and_no_remote_branch_or_config_is_created() {
    let fixture = Fixture::new();
    let tip = commit(
        &fixture.repository,
        &[
            ("note.txt", "target\n"),
            (".gitignore", "ignored.txt\n"),
            ("ignored.txt", "tracked target\n"),
        ],
        Some(fixture.main),
        "ignored collision",
    );
    fixture.remote(tip);
    fixture.remove_feature();
    fs::write(fixture.root.join("ignored.txt"), "precious ignored file\n").unwrap();
    let before = fs::read(fixture.repository.commondir().join("config")).unwrap();
    assert!(checkout::execute(&fixture.root, &fixture.request("origin/feature", true)).is_err());
    fixture.assert_main();
    assert_eq!(
        fs::read_to_string(fixture.root.join("ignored.txt")).unwrap(),
        "precious ignored file\n"
    );
    assert!(
        fixture
            .repository
            .find_branch("feature", BranchType::Local)
            .is_err()
    );
    assert_eq!(
        fs::read(fixture.repository.commondir().join("config")).unwrap(),
        before
    );
    fixture.clean();
}

#[test]
fn locked_tracking_config_leaves_no_new_branch_and_keeps_the_other_lock() {
    let fixture = Fixture::new();
    fixture.remote(fixture.feature);
    fixture.remove_feature();
    let lock = fixture.repository.commondir().join("config.lock");
    fs::write(&lock, "another process").unwrap();
    let before = fs::read(fixture.repository.commondir().join("config")).unwrap();
    let error =
        checkout::execute(&fixture.root, &fixture.request("origin/feature", true)).unwrap_err();
    assert_eq!(error.kind, CheckoutErrorKind::ConcurrentChange);
    fixture.assert_main();
    assert!(
        fixture
            .repository
            .find_branch("feature", BranchType::Local)
            .is_err()
    );
    assert_eq!(fs::read_to_string(&lock).unwrap(), "another process");
    assert_eq!(
        fs::read(fixture.repository.commondir().join("config")).unwrap(),
        before
    );
    fixture.clean();
}

#[test]
fn a_replaced_index_lock_aborts_publication_and_recovers_files_without_touching_original_index() {
    let fixture = Fixture::new();
    fixture.remote(fixture.feature);
    fixture.remove_feature();
    let lock = fixture.repository.path().join("index.lock");
    let config = fixture.repository.commondir().join("config");
    let before = fs::read(&config).unwrap();
    let index = fs::read(fixture.repository.path().join("index")).unwrap();
    fs::write(
        fixture.root.join("local.txt"),
        "retain unrelated local file\n",
    )
    .unwrap();
    let mut injected = false;
    let mut progress = |_: Option<&Path>, completed: usize, _: usize| {
        if !injected && completed > 0 {
            fs::write(&lock, "another process holds the index").unwrap();
            injected = true;
        }
    };
    let error = checkout::execute_with_progress(
        &fixture.root,
        &fixture.request("origin/feature", true),
        &mut progress,
    )
    .unwrap_err();
    assert!(
        injected,
        "fixture must fail after real checkout wrote a file"
    );
    assert_eq!(error.kind, CheckoutErrorKind::ConcurrentChange);
    fixture.assert_main();
    // A replaced sentinel belongs to the external writer and must be retained.
    // Recovery uses the private index, while the real index remains byte-for-byte
    // unchanged for the entire failed operation.
    assert_eq!(
        fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
        "main\n"
    );
    assert_eq!(
        fs::read(fixture.repository.path().join("index")).unwrap(),
        index
    );
    assert!(
        fixture
            .repository
            .find_branch("feature", BranchType::Local)
            .is_err()
    );
    assert_eq!(fs::read(&config).unwrap(), before);
    assert_eq!(
        fs::read_to_string(&lock).unwrap(),
        "another process holds the index"
    );
    assert!(!fixture.repository.commondir().join("config.lock").exists());
    assert_eq!(
        fs::read_to_string(fixture.root.join("local.txt")).unwrap(),
        "retain unrelated local file\n"
    );
    fs::remove_file(fixture.root.join("local.txt")).unwrap();
    fs::remove_file(lock).unwrap();
    fixture.clean();
}

#[test]
fn conflicting_worktree_config_rolls_back_a_created_branch_and_exact_config_bytes() {
    let fixture = Fixture::new();
    fixture.remote(fixture.feature);
    fixture.remove_feature();
    fixture
        .repository
        .config()
        .unwrap()
        .set_bool("extensions.worktreeConfig", true)
        .unwrap();
    fs::write(
        fixture.repository.path().join("config.worktree"),
        "[branch \"feature\"]\n\tremote = nonexistent-remote\n",
    )
    .unwrap();
    let before = fs::read(fixture.repository.commondir().join("config")).unwrap();
    let error =
        checkout::execute(&fixture.root, &fixture.request("origin/feature", true)).unwrap_err();
    assert_eq!(error.kind, CheckoutErrorKind::BranchConflict);
    fixture.assert_main();
    assert!(
        fixture
            .repository
            .find_branch("feature", BranchType::Local)
            .is_err()
    );
    assert_eq!(
        fs::read(fixture.repository.commondir().join("config")).unwrap(),
        before
    );
    assert!(!fixture.repository.commondir().join("config.lock").exists());
    fixture.clean();
}

#[test]
fn onbranch_include_is_checked_after_head_changes_and_is_fully_rolled_back() {
    let fixture = Fixture::new();
    fixture.remote(fixture.feature);
    fixture.remove_feature();
    fixture
        .repository
        .remote("other", "https://example.invalid/never-connected-either")
        .unwrap();
    fixture
        .repository
        .reference(
            "refs/remotes/other/feature",
            fixture.feature,
            false,
            "fixture",
        )
        .unwrap();
    fixture
        .repository
        .config()
        .unwrap()
        .set_str("branch.feature.description", "keep existing branch section")
        .unwrap();
    let path = fixture.repository.commondir().join("config");
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("\n[includeIf \"onbranch:feature\"]\n\tpath = feature.cfg\n");
    fs::write(&path, text).unwrap();
    fs::write(
        fixture.repository.commondir().join("feature.cfg"),
        "[branch \"feature\"]\n\tremote = other\n\tmerge = refs/heads/feature\n",
    )
    .unwrap();
    let before = fs::read(&path).unwrap();
    fs::write(
        fixture.root.join(".gitignore"),
        "ignored.txt\n# preserve staged local edit\n",
    )
    .unwrap();
    let staged = {
        let mut index = fixture.repository.index().unwrap();
        index.add_path(Path::new(".gitignore")).unwrap();
        index.write().unwrap();
        index.get_path(Path::new(".gitignore"), 0).unwrap().id
    };
    let error =
        checkout::execute(&fixture.root, &fixture.request("origin/feature", true)).unwrap_err();
    assert_eq!(error.kind, CheckoutErrorKind::BranchConflict);
    fixture.assert_main();
    assert_eq!(
        fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
        "main\n"
    );
    assert!(
        fixture
            .repository
            .find_branch("feature", BranchType::Local)
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(!fixture.repository.commondir().join("config.lock").exists());
    assert_eq!(
        fs::read_to_string(fixture.root.join(".gitignore")).unwrap(),
        "ignored.txt\n# preserve staged local edit\n"
    );
    assert_eq!(
        Repository::open(&fixture.root)
            .unwrap()
            .index()
            .unwrap()
            .get_path(Path::new(".gitignore"), 0)
            .unwrap()
            .id,
        staged
    );
    fs::write(fixture.root.join(".gitignore"), "ignored.txt\n").unwrap();
    let mut index = fixture.repository.index().unwrap();
    index.add_path(Path::new(".gitignore")).unwrap();
    index.write().unwrap();
    fixture.clean();
}

#[test]
fn readonly_tracking_config_is_rejected_before_any_branch_or_file_changes() {
    struct RestorePermissions(PathBuf, fs::Permissions);
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            fs::set_permissions(&self.0, self.1.clone()).unwrap();
        }
    }
    let fixture = Fixture::new();
    fixture.remote(fixture.feature);
    fixture.remove_feature();
    let path = fixture.repository.commondir().join("config");
    let before = fs::read(&path).unwrap();
    let permissions = fs::metadata(&path).unwrap().permissions();
    let _restore = RestorePermissions(path.clone(), permissions.clone());
    let mut readonly = permissions;
    readonly.set_readonly(true);
    fs::set_permissions(&path, readonly).unwrap();
    let error =
        checkout::execute(&fixture.root, &fixture.request("origin/feature", true)).unwrap_err();
    assert_eq!(error.kind, CheckoutErrorKind::CheckoutFailed);
    fixture.assert_main();
    assert!(
        fixture
            .repository
            .find_branch("feature", BranchType::Local)
            .is_err()
    );
    assert_eq!(fs::read(path).unwrap(), before);
    assert!(!fixture.repository.commondir().join("config.lock").exists());
    fixture.clean();
}

#[test]
fn a_branch_checked_out_in_another_worktree_is_rejected() {
    let fixture = Fixture::new();
    let reference = fixture
        .repository
        .find_reference("refs/heads/feature")
        .unwrap();
    let mut options = WorktreeAddOptions::new();
    options.reference(Some(&reference));
    fixture
        .repository
        .worktree("linked", &fixture.directory.join("linked"), Some(&options))
        .unwrap();
    let error = checkout::execute(&fixture.root, &fixture.request("feature", false)).unwrap_err();
    assert_eq!(error.kind, CheckoutErrorKind::BranchInUse);
    fixture.assert_main();
    fixture.clean();
}

#[test]
fn linked_worktree_cannot_take_the_main_worktrees_branch() {
    let fixture = Fixture::new();
    let reference = fixture
        .repository
        .find_reference("refs/heads/feature")
        .unwrap();
    let mut options = WorktreeAddOptions::new();
    options.reference(Some(&reference));
    let linked = fixture.directory.join("linked");
    fixture
        .repository
        .worktree("linked", &linked, Some(&options))
        .unwrap();
    let error = checkout::execute(
        &linked,
        &CheckoutRequest {
            branch: "main".into(),
            remote: false,
            expected_branch: Some("feature".into()),
            expected_head: Some(fixture.feature.to_string()),
        },
    )
    .unwrap_err();
    assert_eq!(error.kind, CheckoutErrorKind::BranchInUse);
    fixture.assert_main();
}

#[test]
fn detached_head_can_switch_when_its_captured_identity_matches() {
    let fixture = Fixture::new();
    fixture.repository.set_head_detached(fixture.main).unwrap();
    let outcome = checkout::execute(&fixture.root, &fixture.request("feature", false)).unwrap();
    assert_eq!(outcome.head, fixture.feature.to_string());
    fixture.clean();
}

#[test]
fn stale_branch_or_commit_identity_blocks_switching() {
    let fixture = Fixture::new();
    for (branch, head) in [
        (Some("other".into()), fixture.main),
        (Some("main".into()), fixture.feature),
        (None, fixture.main),
    ] {
        let mut request = fixture.request("feature", false);
        request.expected_branch = branch;
        request.expected_head = Some(head.to_string());
        assert_eq!(
            checkout::execute(&fixture.root, &request).unwrap_err().kind,
            CheckoutErrorKind::ConcurrentChange
        );
        fixture.assert_main();
        fixture.clean();
    }
}

#[test]
fn merge_state_and_target_ref_lock_block_switching_before_files_change() {
    let fixture = Fixture::new();
    let merge = fixture.repository.path().join("MERGE_HEAD");
    fs::write(&merge, format!("{}\n", fixture.feature)).unwrap();
    assert_eq!(
        checkout::execute(&fixture.root, &fixture.request("feature", false))
            .unwrap_err()
            .kind,
        CheckoutErrorKind::InProgress
    );
    fs::remove_file(merge).unwrap();
    let lock = fixture.repository.path().join("refs/heads/feature.lock");
    fs::write(&lock, "another process").unwrap();
    assert_eq!(
        checkout::execute(&fixture.root, &fixture.request("feature", false))
            .unwrap_err()
            .kind,
        CheckoutErrorKind::ConcurrentChange
    );
    assert_eq!(fs::read_to_string(lock).unwrap(), "another process");
    fixture.assert_main();
    fixture.clean();
}

#[test]
fn invalid_names_missing_branches_and_remote_head_are_refused() {
    let fixture = Fixture::new();
    for branch in ["", "bad..branch", "refs/tags/main", "bad\0branch"] {
        assert_eq!(
            checkout::execute(&fixture.root, &fixture.request(branch, false))
                .unwrap_err()
                .kind,
            CheckoutErrorKind::InvalidRequest
        );
    }
    assert_eq!(
        checkout::execute(&fixture.root, &fixture.request("absent", false))
            .unwrap_err()
            .kind,
        CheckoutErrorKind::NotFound
    );
    fixture.remote(fixture.feature);
    fixture
        .repository
        .reference_symbolic(
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/feature",
            false,
            "fixture",
        )
        .unwrap();
    assert_eq!(
        checkout::execute(&fixture.root, &fixture.request("origin/HEAD", true))
            .unwrap_err()
            .kind,
        CheckoutErrorKind::InvalidRequest
    );
    fixture.assert_main();
    fixture.clean();
}

#[test]
fn subdirectories_bare_repositories_and_unborn_heads_are_refused() {
    let fixture = Fixture::new();
    let nested = fixture.root.join("nested");
    fs::create_dir(&nested).unwrap();
    assert_eq!(
        checkout::execute(&nested, &fixture.request("feature", false))
            .unwrap_err()
            .kind,
        CheckoutErrorKind::InvalidRepository
    );
    let bare = fixture.directory.join("bare.git");
    Repository::init_bare(&bare).unwrap();
    assert_eq!(
        checkout::execute(&bare, &fixture.request("feature", false))
            .unwrap_err()
            .kind,
        CheckoutErrorKind::InvalidRepository
    );
    let unborn = fixture.directory.join("unborn");
    Repository::init(&unborn)
        .unwrap()
        .set_head("refs/heads/main")
        .unwrap();
    assert_eq!(
        checkout::execute(
            &unborn,
            &CheckoutRequest {
                branch: "feature".into(),
                remote: false,
                expected_branch: Some("main".into()),
                expected_head: None
            }
        )
        .unwrap_err()
        .kind,
        CheckoutErrorKind::UnbornHead
    );
    fixture.assert_main();
}
