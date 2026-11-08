//! Real local repositories exercise the public native worktree boundary.
//! All mutations stay inside uniquely owned temporary test directories;
//! no installed Git command, desktop window, remote, or user repository is used.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use git2::{
    BranchType, IndexEntry, IndexTime, Oid, Repository, RepositoryInitOptions, Signature, Time,
    WorktreeAddOptions, WorktreeLockStatus, build::CheckoutBuilder,
};
use gitviews_git::{
    Git2Backend, GitBackend, Head, WorktreeAction, WorktreeErrorKind, WorktreeRequest,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    base: PathBuf,
    directory: PathBuf,
    root: PathBuf,
    main: Oid,
    feature: Oid,
}

impl Fixture {
    fn new() -> Self {
        // Creating a sibling worktree must not be rejected as nested in the
        // source checkout, which is itself a Git repository in CI.
        let base = std::env::temp_dir().join("gitviews-worktree-tests");
        fs::create_dir_all(&base).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = base.join(format!(
            "{}-{stamp}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let root = directory.join("主 仓库");
        let mut options = RepositoryInitOptions::new();
        options.initial_head("main");
        let repository = Repository::init_opts(&root, &options).unwrap();
        repository
            .config()
            .unwrap()
            .set_bool("core.autocrlf", false)
            .unwrap();
        let main = commit(&repository, "main\n", None);
        let feature = commit(&repository, "feature\n", Some(main));
        repository
            .reference("refs/heads/main", main, false, "fixture")
            .unwrap();
        repository
            .reference("refs/heads/feature", feature, false, "fixture")
            .unwrap();
        repository.set_head("refs/heads/main").unwrap();
        repository
            .checkout_head(Some(CheckoutBuilder::new().safe().disable_filters(true)))
            .unwrap();
        Self {
            base,
            directory,
            root,
            main,
            feature,
        }
    }

    fn repository(&self) -> Repository {
        Repository::open(&self.root).unwrap()
    }

    fn request(&self, action: WorktreeAction) -> WorktreeRequest {
        request_at(&self.root, action)
    }

    fn linked(&self, name: &str) -> PathBuf {
        let repository = self.repository();
        let reference = repository.find_reference("refs/heads/feature").unwrap();
        let mut options = WorktreeAddOptions::new();
        options.reference(Some(&reference));
        let path = self.directory.join(name);
        repository.worktree(name, &path, Some(&options)).unwrap();
        path
    }

    fn registration(&self, path: &Path) -> PathBuf {
        Repository::open(path).unwrap().path().to_owned()
    }

    fn assert_main(&self) {
        let repository = self.repository();
        assert_eq!(
            repository.head().unwrap().name().unwrap(),
            "refs/heads/main"
        );
        assert_eq!(repository.head().unwrap().target(), Some(self.main));
        assert_eq!(
            fs::read_to_string(self.root.join("note.txt")).unwrap(),
            "main\n"
        );
        assert!(!repository.path().join("HEAD.lock").exists());
    }

    fn remove_owned_directory(&self, path: &Path) {
        let directory = fs::canonicalize(&self.directory).unwrap();
        let target = fs::canonicalize(path).unwrap();
        assert!(directory.starts_with(&self.base));
        assert!(target.starts_with(&directory));
        assert_ne!(target, directory);
        fs::remove_dir_all(target).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Resolve and verify the exact test-owned directory before recursion.
        if let Ok(directory) = fs::canonicalize(&self.directory)
            && directory.parent() == Some(self.base.as_path())
            && directory != self.base
        {
            let _ = fs::remove_dir_all(directory);
        }
    }
}

fn commit(repository: &Repository, text: &str, parent: Option<Oid>) -> Oid {
    let mut builder = repository.treebuilder(None).unwrap();
    for (path, text) in [("note.txt", text), (".gitignore", "ignored.txt\n")] {
        builder
            .insert(path, repository.blob(text.as_bytes()).unwrap(), 0o100644)
            .unwrap();
    }
    let tree = repository.find_tree(builder.write().unwrap()).unwrap();
    let signature = Signature::new(
        "Worktree Fixture",
        "worktree@example.invalid",
        &Time::new(1_700_000_000, 0),
    )
    .unwrap();
    let parent = parent.map(|id| repository.find_commit(id).unwrap());
    let parents: Vec<_> = parent.iter().collect();
    repository
        .commit(None, &signature, &signature, "fixture", &tree, &parents)
        .unwrap()
}

fn request_at(root: &Path, action: WorktreeAction) -> WorktreeRequest {
    let repository = Repository::open(root).unwrap();
    let head = repository.head().unwrap();
    WorktreeRequest {
        expected_head: if head.is_branch() {
            Head::Branch(head.shorthand().unwrap().to_owned())
        } else {
            Head::Detached(head.target().unwrap().to_string())
        },
        expected_commit: head.target().map(|id| id.to_string()),
        action,
    }
}

fn branch_head(path: &Path) -> (String, Oid) {
    let repository = Repository::open(path).unwrap();
    let head = repository.head().unwrap();
    (head.name().unwrap().to_owned(), head.target().unwrap())
}

fn gitfile_target(path: &Path) -> PathBuf {
    let text = fs::read_to_string(path.join(".git")).unwrap();
    let target = PathBuf::from(text.trim().strip_prefix("gitdir: ").unwrap());
    fs::canonicalize(if target.is_absolute() {
        target
    } else {
        path.join(target)
    })
    .unwrap()
}

fn register_module(parent: &Path, name: &str, child_head: Option<Oid>, configured: bool) {
    let repository = Repository::open(parent).unwrap();
    let mut index = repository.index().unwrap();
    if configured {
        fs::write(
            parent.join(".gitmodules"),
            format!(
                "[submodule \"{name}\"]\n\tpath = {name}\n\turl = https://example.invalid/{name}.git\n"
            ),
        )
        .unwrap();
        index.add_path(Path::new(".gitmodules")).unwrap();
    }
    if let Some(id) = child_head {
        index
            .add(&IndexEntry {
                ctime: IndexTime::new(0, 0),
                mtime: IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: 0o160000,
                uid: 0,
                gid: 0,
                file_size: 0,
                id,
                flags: 0,
                flags_extended: 0,
                path: name.as_bytes().to_vec(),
            })
            .unwrap();
    }
    index.write().unwrap();
    let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
    let parent_commit = repository.head().unwrap().peel_to_commit().unwrap();
    let signature = Signature::new(
        "Worktree Fixture",
        "worktree@example.invalid",
        &Time::new(1_700_000_001, 0),
    )
    .unwrap();
    repository
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "register local submodule fixture",
            &tree,
            &[&parent_commit],
        )
        .unwrap();
}

/// Build the absorbed layout Git uses for a submodule in a linked worktree,
/// without cloning, contacting a remote, or requiring an installed Git command.
fn initialized_module(parent: &Path, name: &str, absorbed: bool) -> PathBuf {
    let path = parent.join(name);
    let mut options = RepositoryInitOptions::new();
    options.initial_head("main");
    let repository = Repository::init_opts(&path, &options).unwrap();
    repository
        .config()
        .unwrap()
        .set_bool("core.autocrlf", false)
        .unwrap();
    let head = commit(&repository, "submodule fixture\n", None);
    repository
        .reference("refs/heads/main", head, false, "fixture")
        .unwrap();
    repository.set_head("refs/heads/main").unwrap();
    repository
        .checkout_head(Some(CheckoutBuilder::new().safe().disable_filters(true)))
        .unwrap();
    if absorbed {
        let gitdir = Repository::open(parent)
            .unwrap()
            .path()
            .join("modules")
            .join(name);
        repository
            .config()
            .unwrap()
            .set_str("core.worktree", path.to_str().unwrap())
            .unwrap();
        drop(repository);
        fs::create_dir_all(gitdir.parent().unwrap()).unwrap();
        fs::rename(path.join(".git"), &gitdir).unwrap();
        fs::write(
            path.join(".git"),
            format!("gitdir: {}\n", gitdir.to_str().unwrap()),
        )
        .unwrap();
    }
    register_module(parent, name, Some(head), true);
    assert!(
        Repository::open(&path)
            .unwrap()
            .statuses(None)
            .unwrap()
            .is_empty()
    );
    path
}

fn stores_containing_object(directory: &Path, object: Oid) -> Vec<PathBuf> {
    let mut pending = vec![directory.to_owned()];
    let mut stores = Vec::new();
    while let Some(path) = pending.pop() {
        if path.join("objects").is_dir()
            && let Ok(repository) = Repository::open_bare(&path)
            && repository.find_object(object, None).is_ok()
        {
            stores.push(path.clone());
        }
        for entry in fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() && entry.file_name() != "objects" {
                pending.push(entry.path());
            }
        }
    }
    stores
}

#[test]
fn create_new_branch_checks_out_seed_without_switching_the_main_worktree() {
    let fixture = Fixture::new();
    let path = fixture.directory.join("中文 新工作树");
    let outcome = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Create {
                path: path.clone(),
                branch: "topic/新功能".into(),
                create_branch: true,
            }),
        )
        .unwrap();
    assert!(!outcome.message.is_empty());
    assert_eq!(
        branch_head(&path),
        ("refs/heads/topic/新功能".into(), fixture.main)
    );
    assert_eq!(fs::read_to_string(path.join("note.txt")).unwrap(), "main\n");
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
    assert!(
        Git2Backend
            .details(&fixture.root)
            .unwrap()
            .worktrees
            .iter()
            .any(|entry| !entry.is_main && entry.branch.as_deref() == Some("topic/新功能"))
    );
    fixture.assert_main();
}

#[test]
fn create_existing_branch_uses_its_own_tip_and_a_unicode_destination() {
    let fixture = Fixture::new();
    let path = fixture.directory.join("已有分支 工作树");
    Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Create {
                path: path.clone(),
                branch: "feature".into(),
                create_branch: false,
            }),
        )
        .unwrap();
    assert_eq!(
        branch_head(&path),
        ("refs/heads/feature".into(), fixture.feature)
    );
    assert_eq!(
        fs::read_to_string(path.join("note.txt")).unwrap(),
        "feature\n"
    );
    fixture.assert_main();
}

#[test]
fn create_from_a_linked_worktree_uses_that_worktrees_head() {
    let fixture = Fixture::new();
    let linked = fixture.linked("linked");
    let path = fixture.directory.join("from-linked");
    Git2Backend
        .worktree(
            &linked,
            &request_at(
                &linked,
                WorktreeAction::Create {
                    path: path.clone(),
                    branch: "from-linked".into(),
                    create_branch: true,
                },
            ),
        )
        .unwrap();
    assert_eq!(
        branch_head(&path),
        ("refs/heads/from-linked".into(), fixture.feature)
    );
    assert_eq!(branch_head(&linked).1, fixture.feature);
    assert_eq!(
        fs::read_to_string(path.join("note.txt")).unwrap(),
        "feature\n"
    );
    fixture.assert_main();
}

#[test]
fn create_refuses_existing_directory_and_keeps_its_contents_and_refs() {
    let fixture = Fixture::new();
    let path = fixture.directory.join("existing");
    fs::create_dir(&path).unwrap();
    fs::write(path.join("precious.txt"), "do not touch").unwrap();
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Create {
                path: path.clone(),
                branch: "new-topic".into(),
                create_branch: true,
            }),
        )
        .unwrap_err();
    assert_eq!(error.kind, WorktreeErrorKind::InvalidRequest);
    assert_eq!(
        fs::read_to_string(path.join("precious.txt")).unwrap(),
        "do not touch"
    );
    assert!(
        fixture
            .repository()
            .find_branch("new-topic", BranchType::Local)
            .is_err()
    );
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
    fixture.assert_main();
}

#[test]
fn occupied_branches_and_conflicting_new_names_are_refused() {
    let fixture = Fixture::new();
    let linked = fixture.linked("linked");
    for (branch, create_branch) in [("main", false), ("feature", false), ("feature", true)] {
        let destination = fixture.directory.join("blocked");
        let error = Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Create {
                    path: destination.clone(),
                    branch: branch.into(),
                    create_branch,
                }),
            )
            .unwrap_err();
        assert_eq!(error.kind, WorktreeErrorKind::InvalidRequest, "{branch}");
        assert!(!destination.exists());
        assert_eq!(branch_head(&linked).1, fixture.feature);
    }
    fixture.assert_main();
}

#[test]
fn invalid_branch_names_are_refused_without_creating_files_or_registrations() {
    let fixture = Fixture::new();
    for branch in [
        "",
        "bad..branch",
        "bad branch",
        "-option",
        "refs/tags/x",
        "nul\0branch",
    ] {
        let path = fixture.directory.join("invalid");
        let error = Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Create {
                    path: path.clone(),
                    branch: branch.into(),
                    create_branch: true,
                }),
            )
            .unwrap_err();
        assert_eq!(error.kind, WorktreeErrorKind::InvalidRequest, "{branch:?}");
        assert!(!path.exists());
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
    }
    fixture.assert_main();
}

#[test]
fn missing_destination_parent_is_refused_without_creating_a_branch_or_directories() {
    let fixture = Fixture::new();
    let parent = fixture.directory.join("missing-parent");
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Create {
                path: parent.join("new-tree"),
                branch: "missing-parent-topic".into(),
                create_branch: true,
            }),
        )
        .unwrap_err();
    assert_eq!(error.kind, WorktreeErrorKind::InvalidRequest);
    assert!(!parent.exists());
    assert!(
        fixture
            .repository()
            .find_branch("missing-parent-topic", BranchType::Local)
            .is_err()
    );
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
    fixture.assert_main();
}

#[test]
fn clean_removal_deletes_only_the_selected_worktree_and_keeps_its_branch() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let registration = fixture.registration(&path);
    let neighbor = fixture.directory.join("linked-extra");
    fs::create_dir(&neighbor).unwrap();
    fs::write(neighbor.join("precious.txt"), "keep neighbor").unwrap();
    Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap();
    assert!(!path.exists());
    assert!(!registration.exists());
    assert_eq!(
        fs::read_to_string(neighbor.join("precious.txt")).unwrap(),
        "keep neighbor"
    );
    assert_eq!(
        fixture
            .repository()
            .refname_to_id("refs/heads/feature")
            .unwrap(),
        fixture.feature
    );
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
    fixture.assert_main();
}

#[cfg(any(unix, windows))]
fn directory_symlink(target: &Path, link: &Path) {
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(target, link).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(any(unix, windows))]
fn file_symlink(target: &Path, link: &Path) {
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(target, link).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[cfg(any(unix, windows))]
#[test]
#[cfg_attr(
    windows,
    ignore = "requires Windows symlink creation privilege; run explicitly when validating link removal"
)]
fn force_removal_unlinks_leaf_symlinks_without_following_targets_cycles_or_nested_modules() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let child = initialized_module(&path, "child", true);
    let outside = fixture.directory.join("precious external target");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("precious.txt"), "keep external contents").unwrap();
    // An unrelated repository at the target must not be examined or removed.
    fs::create_dir(outside.join(".git")).unwrap();
    fs::write(outside.join(".git/precious"), "keep external metadata").unwrap();
    directory_symlink(&outside, &path.join("directory-link"));
    directory_symlink(&outside, &child.join("node_modules"));
    file_symlink(&outside.join("precious.txt"), &path.join("file-link"));
    file_symlink(&outside.join("missing"), &path.join("dangling-link"));
    directory_symlink(&path, &path.join("cycle-link"));

    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap_err();
    assert!(matches!(
        error.kind,
        WorktreeErrorKind::Dirty | WorktreeErrorKind::InvalidRequest
    ));
    assert!(path.join("directory-link").exists());
    let outcome = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: true,
            }),
        )
        .unwrap();
    assert!(!path.exists());
    assert!(outcome.message.contains("保留"));
    assert_eq!(
        fs::read_to_string(outside.join("precious.txt")).unwrap(),
        "keep external contents"
    );
    assert_eq!(
        fs::read_to_string(outside.join(".git/precious")).unwrap(),
        "keep external metadata"
    );
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
    fixture.assert_main();
}

#[cfg(any(unix, windows))]
#[test]
#[cfg_attr(
    windows,
    ignore = "requires Windows symlink creation privilege; run explicitly when validating link boundaries"
)]
fn force_removal_keeps_root_gitfile_metadata_and_module_root_link_boundaries() {
    for boundary in ["root", "gitfile", "metadata", "module"] {
        let fixture = Fixture::new();
        let path = fixture.linked("linked");
        let admin = fixture.registration(&path);
        let outside = fixture.directory.join("precious external target");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("precious.txt"), "keep boundary target").unwrap();
        let mut requested_path = path.clone();
        let link = match boundary {
            "root" => {
                requested_path = fixture.directory.join("root-link");
                directory_symlink(&path, &requested_path);
                requested_path.clone()
            }
            "gitfile" => {
                let saved = outside.join("saved-gitfile");
                fs::rename(path.join(".git"), &saved).unwrap();
                let link = path.join(".git");
                file_symlink(&saved, &link);
                link
            }
            "metadata" => {
                let link = admin.join("metadata-link");
                directory_symlink(&outside, &link);
                link
            }
            "module" => {
                register_module(&path, "module-link", Some(fixture.main), true);
                let link = path.join("module-link");
                directory_symlink(&outside, &link);
                link
            }
            _ => unreachable!(),
        };
        let error = Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: requested_path,
                    force: true,
                }),
            )
            .unwrap_err();
        assert_eq!(error.kind, WorktreeErrorKind::InvalidRequest, "{boundary}");
        assert!(
            error
                .message
                .contains(link.file_name().unwrap().to_string_lossy().as_ref()),
            "{boundary}: {}",
            error.message
        );
        assert!(path.exists());
        assert!(admin.exists());
        assert_eq!(
            fs::read_to_string(outside.join("precious.txt")).unwrap(),
            "keep boundary target"
        );
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
        fixture.assert_main();
    }
}

#[cfg(windows)]
#[test]
#[ignore = "creates Windows junctions; run explicitly when validating no-follow worktree removal"]
fn force_removal_unlinks_windows_junctions_and_keeps_the_main_worktree_targets() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let child = initialized_module(&path, "child", true);
    let target = fixture.root.join("shared node_modules");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("precious.txt"), "main worktree dependencies").unwrap();
    for link in [path.join("node_modules"), child.join("node_modules")] {
        let output = std::process::Command::new("cmd.exe")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction fixture setup failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(fs::read_link(&link).is_ok());
    }
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap_err();
    assert!(matches!(
        error.kind,
        WorktreeErrorKind::Dirty | WorktreeErrorKind::InvalidRequest
    ));
    Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: true,
            }),
        )
        .unwrap();
    assert!(!path.exists());
    assert_eq!(
        fs::read_to_string(target.join("precious.txt")).unwrap(),
        "main worktree dependencies"
    );
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
    fixture.assert_main();
}

#[test]
fn uninitialized_registered_modules_and_unconfigured_gitlinks_allow_removal() {
    for (configured, gitlink) in [(true, true), (true, false), (false, true)] {
        let fixture = Fixture::new();
        let path = fixture.linked("linked");
        let registration = fixture.registration(&path);
        register_module(
            &path,
            "not-initialized",
            gitlink.then_some(fixture.main),
            configured,
        );
        assert!(!path.join("not-initialized").exists());
        Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: false,
                }),
            )
            .unwrap();
        assert!(!path.exists(), "configured={configured}, gitlink={gitlink}");
        assert!(!registration.exists());
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
        fixture.assert_main();
    }
}

#[test]
fn clean_initialized_module_removal_preserves_all_local_objects_outside_deleted_directories() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let registration = fixture.registration(&path);
    let child = initialized_module(&path, "child", true);
    let repository = Repository::open(&child).unwrap();
    let child_head = repository.head().unwrap().target().unwrap();
    let dangling = repository
        .blob(b"precious local object unreachable from a submodule ref")
        .unwrap();
    assert!(repository.path().starts_with(&registration));
    drop(repository);
    Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap();
    assert!(!path.exists());
    assert!(!registration.exists());
    let common = fixture.repository().commondir().to_owned();
    for object in [child_head, dangling] {
        let stores = stores_containing_object(&common, object);
        assert!(!stores.is_empty(), "submodule object {object} was deleted");
        assert!(
            stores
                .iter()
                .all(|store| { !store.starts_with(&path) && !store.starts_with(&registration) })
        );
    }
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
    fixture.assert_main();
}

#[test]
fn module_names_matching_git_metadata_directories_allow_clean_removal() {
    for name in ["worktrees", "commondir"] {
        let fixture = Fixture::new();
        let path = fixture.linked("linked");
        let registration = fixture.registration(&path);
        let child = initialized_module(&path, name, true);
        let precious = Repository::open(&child)
            .unwrap()
            .blob(format!("local object in the {name} submodule").as_bytes())
            .unwrap();
        Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: false,
                }),
            )
            .unwrap();
        assert!(!path.exists(), "{name}");
        assert!(!registration.exists(), "{name}");
        assert!(
            !stores_containing_object(fixture.repository().commondir(), precious).is_empty(),
            "the {name} submodule object database was deleted"
        );
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
        fixture.assert_main();
    }
}

#[test]
fn deinitialized_module_object_database_survives_removal_of_its_parent_worktree() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let registration = fixture.registration(&path);
    let child = initialized_module(&path, "child", true);
    let precious = Repository::open(&child)
        .unwrap()
        .blob(b"local object in a deinitialized module")
        .unwrap();
    fixture.remove_owned_directory(&child);
    assert!(registration.join("modules/child/objects").is_dir());
    Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap();
    assert!(!path.exists());
    assert!(!registration.exists());
    assert!(
        !stores_containing_object(fixture.repository().commondir(), precious).is_empty(),
        "deinitialized module object database was deleted"
    );
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
    fixture.assert_main();
}

#[test]
fn uninitialized_module_directory_with_opaque_files_is_preserved() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    register_module(&path, "not-initialized", Some(fixture.main), true);
    let child = path.join("not-initialized");
    fs::create_dir(&child).unwrap();
    fs::write(child.join("precious.txt"), "opaque local files\n").unwrap();
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap_err();
    assert!(matches!(
        error.kind,
        WorktreeErrorKind::Dirty | WorktreeErrorKind::Unsupported
    ));
    assert_eq!(
        fs::read_to_string(child.join("precious.txt")).unwrap(),
        "opaque local files\n"
    );
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
    fixture.assert_main();
}

#[test]
fn dirty_module_files_are_preserved_even_when_submodule_ignore_is_all() {
    for state in ["modified", "staged", "untracked", "ignored"] {
        let fixture = Fixture::new();
        let path = fixture.linked("linked");
        let registration = fixture.registration(&path);
        let child = initialized_module(&path, "child", true);
        // The superproject's ordinary status may hide all child changes.
        Repository::open(&path)
            .unwrap()
            .config()
            .unwrap()
            .set_str("submodule.child.ignore", "all")
            .unwrap();
        let file = match state {
            "untracked" => "untracked.txt",
            "ignored" => "ignored.txt",
            _ => "note.txt",
        };
        fs::write(child.join(file), "precious child content\n").unwrap();
        let repository = Repository::open(&child).unwrap();
        if state == "staged" {
            let mut index = repository.index().unwrap();
            index.add_path(Path::new(file)).unwrap();
            index.write().unwrap();
        }
        let child_index = repository.path().join("index");
        let index_before = fs::read(&child_index).unwrap();
        drop(repository);
        let parent_index = fs::read(registration.join("index")).unwrap();
        let error = Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: false,
                }),
            )
            .unwrap_err();
        assert_eq!(error.kind, WorktreeErrorKind::Dirty, "{state}");
        assert_eq!(
            fs::read_to_string(child.join(file)).unwrap(),
            "precious child content\n",
            "{state}"
        );
        assert_eq!(fs::read(&child_index).unwrap(), index_before, "{state}");
        assert_eq!(fs::read(registration.join("index")).unwrap(), parent_index);
        assert!(child.join(".git").is_file());
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
        fixture.assert_main();
    }
}

#[test]
fn nested_modules_are_checked_recursively_before_any_directory_is_deleted() {
    for state in ["modified", "staged", "untracked", "ignored"] {
        let fixture = Fixture::new();
        let path = fixture.linked("linked");
        let child = initialized_module(&path, "child", true);
        let nested = initialized_module(&child, "nested", true);
        let child_head = Repository::open(&child)
            .unwrap()
            .head()
            .unwrap()
            .target()
            .unwrap();
        register_module(&path, "child", Some(child_head), true);
        for (parent, name) in [(&path, "child"), (&child, "nested")] {
            Repository::open(parent)
                .unwrap()
                .config()
                .unwrap()
                .set_str(&format!("submodule.{name}.ignore"), "all")
                .unwrap();
        }
        let file = match state {
            "untracked" => "untracked.txt",
            "ignored" => "ignored.txt",
            _ => "note.txt",
        };
        fs::write(nested.join(file), "precious nested content\n").unwrap();
        let repository = Repository::open(&nested).unwrap();
        if state == "staged" {
            let mut index = repository.index().unwrap();
            index.add_path(Path::new(file)).unwrap();
            index.write().unwrap();
        }
        let index_path = repository.path().join("index");
        let index_before = fs::read(&index_path).unwrap();
        drop(repository);
        let error = Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: false,
                }),
            )
            .unwrap_err();
        assert_eq!(error.kind, WorktreeErrorKind::Dirty, "{state}");
        assert_eq!(
            fs::read_to_string(nested.join(file)).unwrap(),
            "precious nested content\n",
            "{state}"
        );
        assert_eq!(fs::read(index_path).unwrap(), index_before, "{state}");
        assert!(child.join(".git").is_file());
        assert!(nested.join(".git").is_file());
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
        fixture.assert_main();
    }
}

#[test]
fn clean_nested_module_objects_survive_parent_worktree_removal() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let registration = fixture.registration(&path);
    let child = initialized_module(&path, "child", true);
    let nested = initialized_module(&child, "nested", true);
    let child_head = Repository::open(&child)
        .unwrap()
        .head()
        .unwrap()
        .target()
        .unwrap();
    register_module(&path, "child", Some(child_head), true);
    let precious = Repository::open(&nested)
        .unwrap()
        .blob(b"local object in a nested child module")
        .unwrap();
    Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap();
    assert!(!path.exists());
    assert!(!registration.exists());
    assert!(
        !stores_containing_object(fixture.repository().commondir(), precious).is_empty(),
        "nested module object database was deleted"
    );
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
    fixture.assert_main();
}

#[test]
fn embedded_module_git_directory_is_refused_without_losing_its_metadata_or_working_files() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let child = initialized_module(&path, "embedded", false);
    let repository = Repository::open(&child).unwrap();
    let head = repository.head().unwrap().target().unwrap();
    let precious = repository.blob(b"unreachable embedded object").unwrap();
    let config = fs::read(child.join(".git/config")).unwrap();
    drop(repository);
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap_err();
    assert!(matches!(
        error.kind,
        WorktreeErrorKind::Unsupported | WorktreeErrorKind::InvalidRequest
    ));
    let forced = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: true,
            }),
        )
        .unwrap_err();
    assert!(matches!(
        forced.kind,
        WorktreeErrorKind::Unsupported | WorktreeErrorKind::InvalidRequest
    ));
    assert_eq!(fs::read(child.join(".git/config")).unwrap(), config);
    assert_eq!(
        fs::read_to_string(child.join("note.txt")).unwrap(),
        "submodule fixture\n"
    );
    let repository = Repository::open(&child).unwrap();
    assert!(repository.find_commit(head).is_ok());
    assert!(repository.find_blob(precious).is_ok());
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
    fixture.assert_main();
}

#[test]
fn module_gitdir_outside_selected_registration_is_refused_without_rewriting_shared_config() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let child = initialized_module(&path, "child", true);
    let gitdir = gitfile_target(&child);
    let outside = fixture.directory.join("shared-child-gitdir");
    fs::rename(&gitdir, &outside).unwrap();
    fs::write(
        child.join(".git"),
        format!("gitdir: {}\n", outside.to_str().unwrap()),
    )
    .unwrap();
    let config = fs::read(outside.join("config")).unwrap();
    let index = fs::read(outside.join("index")).unwrap();
    let gitfile = fs::read(child.join(".git")).unwrap();
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap_err();
    assert_eq!(error.kind, WorktreeErrorKind::Unsupported);
    assert_eq!(
        Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: true
                }),
            )
            .unwrap_err()
            .kind,
        WorktreeErrorKind::Unsupported
    );
    assert_eq!(fs::read(outside.join("config")).unwrap(), config);
    assert_eq!(fs::read(outside.join("index")).unwrap(), index);
    assert_eq!(fs::read(child.join(".git")).unwrap(), gitfile);
    assert_eq!(
        fs::read_to_string(child.join("note.txt")).unwrap(),
        "submodule fixture\n"
    );
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
    fixture.assert_main();
}

#[test]
fn module_with_its_own_linked_worktree_is_refused_without_breaking_the_other_worktree() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let child = initialized_module(&path, "child", true);
    let repository = Repository::open(&child).unwrap();
    let head = repository.head().unwrap().target().unwrap();
    let reference = repository
        .reference("refs/heads/other-child", head, false, "fixture")
        .unwrap();
    let other = fixture.directory.join("other-child-worktree");
    let mut options = WorktreeAddOptions::new();
    options.reference(Some(&reference));
    repository
        .worktree("other-child", &other, Some(&options))
        .unwrap();
    let config_path = repository.path().join("config");
    let config_before = fs::read(&config_path).unwrap();
    let other_gitfile = fs::read(other.join(".git")).unwrap();
    drop(reference);
    drop(repository);
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap_err();
    assert_eq!(error.kind, WorktreeErrorKind::Unsupported);
    assert_eq!(
        Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: true
                }),
            )
            .unwrap_err()
            .kind,
        WorktreeErrorKind::Unsupported
    );
    assert_eq!(fs::read(&config_path).unwrap(), config_before);
    assert_eq!(fs::read(other.join(".git")).unwrap(), other_gitfile);
    assert_eq!(branch_head(&other), ("refs/heads/other-child".into(), head));
    assert!(child.join("note.txt").is_file());
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
    fixture.assert_main();
}

#[test]
fn module_with_relative_object_alternates_is_refused_without_rewriting_the_dependency() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let child = initialized_module(&path, "child", true);
    let gitdir = gitfile_target(&child);
    let dependency = gitdir.parent().unwrap().join("dependency/objects");
    fs::create_dir_all(&dependency).unwrap();
    let alternates = gitdir.join("objects/info/alternates");
    fs::write(&alternates, "../../dependency/objects\n").unwrap();
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap_err();
    assert_eq!(error.kind, WorktreeErrorKind::Unsupported);
    assert_eq!(
        Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: true
                }),
            )
            .unwrap_err()
            .kind,
        WorktreeErrorKind::Unsupported
    );
    assert_eq!(
        fs::read_to_string(&alternates).unwrap(),
        "../../dependency/objects\n"
    );
    assert!(dependency.is_dir());
    assert!(child.join("note.txt").is_file());
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
    fixture.assert_main();
}

#[test]
fn child_git_locks_block_removal_without_consuming_another_process_lock() {
    for lock_name in ["index.lock", "HEAD.lock"] {
        let fixture = Fixture::new();
        let path = fixture.linked("linked");
        let child = initialized_module(&path, "child", true);
        let lock = Repository::open(&child).unwrap().path().join(lock_name);
        fs::write(&lock, "another process owns this child lock").unwrap();
        let error = Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: false,
                }),
            )
            .unwrap_err();
        assert_eq!(error.kind, WorktreeErrorKind::Busy, "{lock_name}");
        assert_eq!(
            Git2Backend
                .worktree(
                    &fixture.root,
                    &fixture.request(WorktreeAction::Remove {
                        path: path.clone(),
                        force: true
                    }),
                )
                .unwrap_err()
                .kind,
            WorktreeErrorKind::Busy,
            "{lock_name}"
        );
        assert_eq!(
            fs::read_to_string(&lock).unwrap(),
            "another process owns this child lock"
        );
        assert!(child.join("note.txt").is_file());
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
        fixture.assert_main();
    }
}

#[test]
fn deinitialized_module_locks_block_removal_without_deleting_metadata_or_parent_files() {
    for lock_name in ["index.lock", "HEAD.lock"] {
        let fixture = Fixture::new();
        let path = fixture.linked("linked");
        let registration = fixture.registration(&path);
        let child = initialized_module(&path, "child", true);
        let lock = gitfile_target(&child).join(lock_name);
        fixture.remove_owned_directory(&child);
        fs::write(&lock, "a writer still owns the deinitialized module").unwrap();
        let parent_index = fs::read(registration.join("index")).unwrap();
        let error = Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: false,
                }),
            )
            .unwrap_err();
        assert_eq!(error.kind, WorktreeErrorKind::Busy, "{lock_name}");
        assert_eq!(
            Git2Backend
                .worktree(
                    &fixture.root,
                    &fixture.request(WorktreeAction::Remove {
                        path: path.clone(),
                        force: true
                    }),
                )
                .unwrap_err()
                .kind,
            WorktreeErrorKind::Busy,
            "{lock_name}"
        );
        assert_eq!(
            fs::read_to_string(&lock).unwrap(),
            "a writer still owns the deinitialized module"
        );
        assert_eq!(fs::read(registration.join("index")).unwrap(), parent_index);
        assert_eq!(
            fs::read_to_string(path.join("note.txt")).unwrap(),
            "feature\n"
        );
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
        fixture.assert_main();
    }
}

#[test]
fn module_head_different_from_gitlink_blocks_removal_even_when_submodule_ignore_is_all() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let child = initialized_module(&path, "child", true);
    Repository::open(&path)
        .unwrap()
        .config()
        .unwrap()
        .set_str("submodule.child.ignore", "all")
        .unwrap();
    let repository = Repository::open(&child).unwrap();
    let old_head = repository.head().unwrap().target().unwrap();
    let new_head = commit(&repository, "submodule fixture\n", Some(old_head));
    repository.set_head_detached(new_head).unwrap();
    assert!(repository.statuses(None).unwrap().is_empty());
    let head_path = repository.path().join("HEAD");
    let head_before = fs::read(&head_path).unwrap();
    drop(repository);
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap_err();
    assert_eq!(error.kind, WorktreeErrorKind::Dirty);
    assert_eq!(fs::read(&head_path).unwrap(), head_before);
    assert_eq!(
        Repository::open(&child).unwrap().head().unwrap().target(),
        Some(new_head)
    );
    assert!(child.join("note.txt").is_file());
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
    fixture.assert_main();
}

#[test]
fn staged_parent_gitlink_change_blocks_removal_when_it_matches_a_clean_ignored_child() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let child = initialized_module(&path, "child", true);
    let repository = Repository::open(&child).unwrap();
    let old_head = repository.head().unwrap().target().unwrap();
    let new_head = commit(&repository, "submodule fixture\n", Some(old_head));
    repository.set_head_detached(new_head).unwrap();
    assert!(repository.statuses(None).unwrap().is_empty());
    drop(repository);
    let parent = Repository::open(&path).unwrap();
    parent
        .config()
        .unwrap()
        .set_str("submodule.child.ignore", "all")
        .unwrap();
    let mut index = parent.index().unwrap();
    let mut gitlink = index.get_path(Path::new("child"), 0).unwrap();
    gitlink.id = new_head;
    index.add(&gitlink).unwrap();
    index.write().unwrap();
    let index_path = parent.path().join("index");
    let index_before = fs::read(&index_path).unwrap();
    assert!(
        parent
            .statuses(None)
            .unwrap()
            .iter()
            .any(|entry| entry.status().contains(git2::Status::INDEX_MODIFIED))
    );
    drop(index);
    drop(parent);
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap_err();
    assert_eq!(error.kind, WorktreeErrorKind::Dirty);
    assert_eq!(fs::read(index_path).unwrap(), index_before);
    assert_eq!(
        Repository::open(&child).unwrap().head().unwrap().target(),
        Some(new_head)
    );
    assert!(child.join("note.txt").is_file());
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
    fixture.assert_main();
}

#[test]
fn child_index_flags_that_hide_edits_block_removal_without_altering_child_index() {
    for assume_unchanged in [true, false] {
        let fixture = Fixture::new();
        let path = fixture.linked("linked");
        let child = initialized_module(&path, "child", true);
        Repository::open(&path)
            .unwrap()
            .config()
            .unwrap()
            .set_str("submodule.child.ignore", "all")
            .unwrap();
        let repository = Repository::open(&child).unwrap();
        let mut index = repository.index().unwrap();
        let mut entry = index.get_path(Path::new("note.txt"), 0).unwrap();
        if assume_unchanged {
            entry.flags |= 0x8000;
        } else {
            entry.flags |= 0x4000;
            entry.flags_extended |= 0x4000;
        }
        index.add(&entry).unwrap();
        index.write().unwrap();
        let index_path = repository.path().join("index");
        let index_before = fs::read(&index_path).unwrap();
        drop(index);
        drop(repository);
        fs::write(child.join("note.txt"), "precious hidden child edit\n").unwrap();
        let error = Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: false,
                }),
            )
            .unwrap_err();
        assert_eq!(error.kind, WorktreeErrorKind::Unsupported);
        assert_eq!(fs::read(index_path).unwrap(), index_before);
        assert_eq!(
            fs::read_to_string(child.join("note.txt")).unwrap(),
            "precious hidden child edit\n"
        );
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
        fixture.assert_main();
    }
}

#[test]
fn modified_staged_untracked_and_ignored_files_each_block_removal() {
    for state in ["modified", "staged", "untracked", "ignored"] {
        let fixture = Fixture::new();
        let path = fixture.linked("linked");
        let file = match state {
            "untracked" => "untracked.txt",
            "ignored" => "ignored.txt",
            _ => "note.txt",
        };
        fs::write(path.join(file), "precious content\n").unwrap();
        let repository = Repository::open(&path).unwrap();
        if state == "staged" {
            let mut index = repository.index().unwrap();
            index.add_path(Path::new(file)).unwrap();
            index.write().unwrap();
        }
        let index_before = fs::read(repository.path().join("index")).unwrap();
        drop(repository);
        let error = Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: false,
                }),
            )
            .unwrap_err();
        assert_eq!(error.kind, WorktreeErrorKind::Dirty, "{state}");
        assert_eq!(
            fs::read_to_string(path.join(file)).unwrap(),
            "precious content\n",
            "{state}"
        );
        let reopened = Repository::open(&path).unwrap();
        assert_eq!(
            fs::read(reopened.path().join("index")).unwrap(),
            index_before,
            "{state}"
        );
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
        fixture.assert_main();
    }
}

#[test]
fn explicit_force_discards_each_kind_of_parent_worktree_change_and_keeps_branch_objects() {
    for state in ["modified", "staged", "untracked", "ignored"] {
        let fixture = Fixture::new();
        let path = fixture.linked("linked");
        let registration = fixture.registration(&path);
        let file = match state {
            "untracked" => "untracked.txt",
            "ignored" => "ignored.txt",
            _ => "note.txt",
        };
        fs::write(path.join(file), "explicitly discarded parent content\n").unwrap();
        let repository = Repository::open(&path).unwrap();
        let staged_blob = if state == "staged" {
            let mut index = repository.index().unwrap();
            index.add_path(Path::new(file)).unwrap();
            index.write().unwrap();
            Some(index.get_path(Path::new(file), 0).unwrap().id)
        } else {
            None
        };
        drop(repository);
        Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: true,
                }),
            )
            .unwrap();
        assert!(!path.exists(), "{state}");
        assert!(!registration.exists(), "{state}");
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
        assert_eq!(
            fixture
                .repository()
                .refname_to_id("refs/heads/feature")
                .unwrap(),
            fixture.feature,
            "{state}"
        );
        if let Some(id) = staged_blob {
            assert_eq!(
                fixture.repository().find_blob(id).unwrap().content(),
                b"explicitly discarded parent content\n"
            );
        }
        fixture.assert_main();
    }
}

#[test]
fn explicit_force_discards_dirty_absorbed_and_nested_module_files_but_retains_their_indexes() {
    for nested in [false, true] {
        for state in ["modified", "staged", "untracked", "ignored"] {
            let fixture = Fixture::new();
            let path = fixture.linked("linked");
            let registration = fixture.registration(&path);
            let child = initialized_module(&path, "child", true);
            let dirty_root = if nested {
                let nested = initialized_module(&child, "nested", true);
                let child_head = Repository::open(&child)
                    .unwrap()
                    .head()
                    .unwrap()
                    .target()
                    .unwrap();
                register_module(&path, "child", Some(child_head), true);
                nested
            } else {
                child.clone()
            };
            for (parent, name) in [(&path, "child"), (&child, "nested")] {
                Repository::open(parent)
                    .unwrap()
                    .config()
                    .unwrap()
                    .set_str(&format!("submodule.{name}.ignore"), "all")
                    .unwrap();
            }
            let file = match state {
                "untracked" => "untracked.txt",
                "ignored" => "ignored.txt",
                _ => "note.txt",
            };
            fs::write(
                dirty_root.join(file),
                "explicitly discarded module content\n",
            )
            .unwrap();
            let repository = Repository::open(&dirty_root).unwrap();
            if state == "staged" {
                let mut index = repository.index().unwrap();
                index.add_path(Path::new(file)).unwrap();
                index.write().unwrap();
            }
            let saved_head = repository.head().unwrap().target().unwrap();
            let marker = repository.blob(b"retained dirty module object").unwrap();
            let index_before = fs::read(repository.path().join("index")).unwrap();
            drop(repository);
            let parent_branch = branch_head(&path).1;
            Git2Backend
                .worktree(
                    &fixture.root,
                    &fixture.request(WorktreeAction::Remove {
                        path: path.clone(),
                        force: true,
                    }),
                )
                .unwrap();
            assert!(!path.exists(), "nested={nested}, {state}");
            assert!(!registration.exists());
            let stores = stores_containing_object(fixture.repository().commondir(), marker);
            assert_eq!(stores.len(), 1, "nested={nested}, {state}");
            assert_eq!(fs::read(stores[0].join("index")).unwrap(), index_before);
            let archived = Repository::open_bare(&stores[0]).unwrap();
            assert!(archived.find_commit(saved_head).is_ok());
            assert_eq!(
                archived.refname_to_id("refs/heads/main").unwrap(),
                saved_head
            );
            assert_eq!(
                fixture
                    .repository()
                    .refname_to_id("refs/heads/feature")
                    .unwrap(),
                parent_branch
            );
            assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
            fixture.assert_main();
        }
    }
}

#[test]
fn explicit_force_removes_opaque_uninitialized_module_files_and_plain_file_replacements() {
    for directory in [false, true] {
        let fixture = Fixture::new();
        let path = fixture.linked("linked");
        let registration = fixture.registration(&path);
        register_module(&path, "not-initialized", Some(fixture.main), true);
        let child = path.join("not-initialized");
        if directory {
            fs::create_dir(&child).unwrap();
            fs::write(child.join("local.txt"), "explicitly discarded opaque files").unwrap();
        } else {
            fs::write(&child, "explicitly discarded gitlink file replacement").unwrap();
        }
        Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: true,
                }),
            )
            .unwrap();
        assert!(!path.exists());
        assert!(!registration.exists());
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
        fixture.assert_main();
    }
}

#[test]
fn explicit_force_preserves_diverged_child_head_and_branches_with_or_without_staged_gitlink() {
    for staged_gitlink in [false, true] {
        let fixture = Fixture::new();
        let path = fixture.linked("linked");
        let child = initialized_module(&path, "child", true);
        let repository = Repository::open(&child).unwrap();
        let old_head = repository.head().unwrap().target().unwrap();
        let new_head = commit(&repository, "submodule fixture\n", Some(old_head));
        repository.set_head_detached(new_head).unwrap();
        let head_before = fs::read(repository.path().join("HEAD")).unwrap();
        drop(repository);
        if staged_gitlink {
            let parent = Repository::open(&path).unwrap();
            let mut index = parent.index().unwrap();
            let mut gitlink = index.get_path(Path::new("child"), 0).unwrap();
            gitlink.id = new_head;
            index.add(&gitlink).unwrap();
            index.write().unwrap();
        }
        Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: true,
                }),
            )
            .unwrap();
        assert!(!path.exists());
        let stores = stores_containing_object(fixture.repository().commondir(), new_head);
        assert_eq!(stores.len(), 1);
        assert_eq!(fs::read(stores[0].join("HEAD")).unwrap(), head_before);
        let archived = Repository::open_bare(&stores[0]).unwrap();
        assert_eq!(archived.head().unwrap().target(), Some(new_head));
        assert_eq!(archived.refname_to_id("refs/heads/main").unwrap(), old_head);
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
        fixture.assert_main();
    }
}

#[test]
fn explicit_force_retains_absorbed_module_metadata_after_its_gitlink_is_staged_for_deletion() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let registration = fixture.registration(&path);
    let child = initialized_module(&path, "child", true);
    let repository = Repository::open(&child).unwrap();
    let marker = repository
        .blob(b"staged removed gitlink module marker")
        .unwrap();
    let child_index = fs::read(repository.path().join("index")).unwrap();
    drop(repository);
    let parent = Repository::open(&path).unwrap();
    let mut index = parent.index().unwrap();
    index.remove_path(Path::new("child")).unwrap();
    index.write().unwrap();
    drop(index);
    drop(parent);
    fs::write(
        child.join("local.txt"),
        "explicitly discarded after gitlink deletion",
    )
    .unwrap();
    Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: true,
            }),
        )
        .unwrap();
    assert!(!path.exists());
    assert!(!registration.exists());
    let stores = stores_containing_object(fixture.repository().commondir(), marker);
    assert_eq!(stores.len(), 1);
    assert_eq!(fs::read(stores[0].join("index")).unwrap(), child_index);
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
    fixture.assert_main();
}

#[test]
fn explicit_force_discards_parent_and_child_edits_hidden_by_index_flags() {
    for assume_unchanged in [true, false] {
        let fixture = Fixture::new();
        let path = fixture.linked("linked");
        let child = initialized_module(&path, "child", true);
        let mut marker = None;
        let mut child_index = Vec::new();
        for root in [&path, &child] {
            let repository = Repository::open(root).unwrap();
            let mut index = repository.index().unwrap();
            let mut entry = index.get_path(Path::new("note.txt"), 0).unwrap();
            if assume_unchanged {
                entry.flags |= 0x8000;
            } else {
                entry.flags |= 0x4000;
                entry.flags_extended |= 0x4000;
            }
            index.add(&entry).unwrap();
            index.write().unwrap();
            fs::write(root.join("note.txt"), "explicitly discarded hidden edit\n").unwrap();
            if root == &child {
                marker = Some(repository.blob(b"module with hidden edit").unwrap());
                child_index = fs::read(repository.path().join("index")).unwrap();
            }
        }
        Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: true,
                }),
            )
            .unwrap();
        assert!(!path.exists());
        let stores = stores_containing_object(fixture.repository().commondir(), marker.unwrap());
        assert_eq!(stores.len(), 1);
        assert_eq!(fs::read(stores[0].join("index")).unwrap(), child_index);
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
        fixture.assert_main();
    }
}

#[test]
fn ignored_nested_repository_is_preserved_when_removal_is_refused() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let nested = path.join("nested-project");
    let repository = Repository::open(&path).unwrap();
    fs::write(
        repository.commondir().join("info/exclude"),
        "/nested-project/\n",
    )
    .unwrap();
    drop(repository);
    Repository::init(&nested).unwrap();
    fs::write(nested.join("precious.txt"), "uncommitted nested repository").unwrap();
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap_err();
    assert!(matches!(
        error.kind,
        WorktreeErrorKind::Dirty | WorktreeErrorKind::InvalidRequest
    ));
    let forced = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: true,
            }),
        )
        .unwrap_err();
    assert!(matches!(
        forced.kind,
        WorktreeErrorKind::Unsupported | WorktreeErrorKind::InvalidRequest
    ));
    assert_eq!(
        fs::read_to_string(nested.join("precious.txt")).unwrap(),
        "uncommitted nested repository"
    );
    assert!(nested.join(".git/HEAD").is_file());
    assert_eq!(branch_head(&path).1, fixture.feature);
    fixture.assert_main();
}

#[test]
fn locked_worktree_refuses_removal_and_move_until_unlocked() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Lock {
                path: path.clone(),
                reason: "正在审查 中文".into(),
            }),
        )
        .unwrap();
    assert_eq!(
        fixture
            .repository()
            .find_worktree("linked")
            .unwrap()
            .is_locked()
            .unwrap(),
        WorktreeLockStatus::Locked(Some("正在审查 中文".into()))
    );
    let destination = fixture.directory.join("moved");
    for action in [
        WorktreeAction::Remove {
            path: path.clone(),
            force: false,
        },
        WorktreeAction::Remove {
            path: path.clone(),
            force: true,
        },
        WorktreeAction::Move {
            path: path.clone(),
            destination: destination.clone(),
        },
    ] {
        assert_eq!(
            Git2Backend
                .worktree(&fixture.root, &fixture.request(action))
                .unwrap_err()
                .kind,
            WorktreeErrorKind::Locked
        );
        assert!(path.exists());
        assert!(!destination.exists());
    }
    Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Unlock { path: path.clone() }),
        )
        .unwrap();
    assert_eq!(
        fixture
            .repository()
            .find_worktree("linked")
            .unwrap()
            .is_locked()
            .unwrap(),
        WorktreeLockStatus::Unlocked
    );
    Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap();
    assert!(!path.exists());
    fixture.assert_main();
}

#[test]
fn prune_missing_worktree_removes_only_its_registration() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let registration = fixture.registration(&path);
    fixture.remove_owned_directory(&path);
    let unrelated = fixture.directory.join("unrelated");
    fs::create_dir(&unrelated).unwrap();
    fs::write(unrelated.join("keep"), "still here").unwrap();
    Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Prune { path: path.clone() }),
        )
        .unwrap();
    assert!(!registration.exists());
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
    assert_eq!(
        fixture
            .repository()
            .refname_to_id("refs/heads/feature")
            .unwrap(),
        fixture.feature
    );
    assert_eq!(
        fs::read_to_string(unrelated.join("keep")).unwrap(),
        "still here"
    );
    fixture.assert_main();
}

#[test]
fn prune_missing_parent_worktree_preserves_deinitialized_module_objects() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let registration = fixture.registration(&path);
    let child = initialized_module(&path, "child", true);
    let precious = Repository::open(&child)
        .unwrap()
        .blob(b"local object preserved while pruning a missing parent worktree")
        .unwrap();
    fixture.remove_owned_directory(&path);
    assert!(registration.join("modules/child/objects").is_dir());
    Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Prune { path: path.clone() }),
        )
        .unwrap();
    assert!(!path.exists());
    assert!(!registration.exists());
    assert!(
        !stores_containing_object(fixture.repository().commondir(), precious).is_empty(),
        "pruning deleted the module object database"
    );
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
    fixture.assert_main();
}

#[test]
fn prune_refuses_present_or_locked_missing_worktrees() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    assert_eq!(
        Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Prune { path: path.clone() })
            )
            .unwrap_err()
            .kind,
        WorktreeErrorKind::InvalidRequest
    );
    fixture
        .repository()
        .find_worktree("linked")
        .unwrap()
        .lock(Some("keep registration"))
        .unwrap();
    fixture.remove_owned_directory(&path);
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Prune { path }),
        )
        .unwrap_err();
    assert_eq!(error.kind, WorktreeErrorKind::Locked);
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
    fixture.assert_main();
}

#[test]
fn move_updates_both_links_and_keeps_files_index_branch_and_registration_identity() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let registration = fixture.registration(&path);
    let index = fs::read(registration.join("index")).unwrap();
    let destination = fixture.directory.join("移动后 中文工作树");
    Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Move {
                path: path.clone(),
                destination: destination.clone(),
            }),
        )
        .unwrap();
    assert!(!path.exists());
    assert_eq!(
        branch_head(&destination),
        ("refs/heads/feature".into(), fixture.feature)
    );
    assert_eq!(
        fs::read_to_string(destination.join("note.txt")).unwrap(),
        "feature\n"
    );
    assert_eq!(fs::read(registration.join("index")).unwrap(), index);
    assert_eq!(
        gitfile_target(&destination),
        fs::canonicalize(&registration).unwrap()
    );
    let back_link = fs::read_to_string(registration.join("gitdir")).unwrap();
    assert_eq!(
        fs::canonicalize(back_link.trim()).unwrap(),
        fs::canonicalize(destination.join(".git")).unwrap()
    );
    fixture
        .repository()
        .find_worktree("linked")
        .unwrap()
        .validate()
        .unwrap();
    fixture.assert_main();
}

#[test]
fn move_refuses_existing_destination_without_touching_either_directory() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let destination = fixture.directory.join("already-exists");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("precious"), "destination data").unwrap();
    let registration = fixture.registration(&path);
    let gitdir = fs::read(registration.join("gitdir")).unwrap();
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Move {
                path: path.clone(),
                destination: destination.clone(),
            }),
        )
        .unwrap_err();
    assert_eq!(error.kind, WorktreeErrorKind::InvalidRequest);
    assert_eq!(
        fs::read_to_string(destination.join("precious")).unwrap(),
        "destination data"
    );
    assert_eq!(fs::read(registration.join("gitdir")).unwrap(), gitdir);
    assert_eq!(branch_head(&path).1, fixture.feature);
    fixture.assert_main();
}

#[test]
fn main_and_current_worktrees_cannot_be_removed_or_moved() {
    let fixture = Fixture::new();
    let linked = fixture.linked("linked");
    let destination = fixture.directory.join("forbidden");
    for (root, path) in [
        (&fixture.root, &fixture.root),
        (&linked, &linked),
        (&linked, &fixture.root),
    ] {
        for action in [
            WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            },
            WorktreeAction::Remove {
                path: path.clone(),
                force: true,
            },
            WorktreeAction::Move {
                path: path.clone(),
                destination: destination.clone(),
            },
        ] {
            let error = Git2Backend
                .worktree(root, &request_at(root, action))
                .unwrap_err();
            assert_eq!(error.kind, WorktreeErrorKind::InvalidRequest);
            assert!(!destination.exists());
            assert!(path.exists());
        }
    }
    fixture.assert_main();
}

#[test]
fn repair_external_rename_restores_exact_gitfile_and_registration_backlink() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let registration = fixture.registration(&path);
    let index = fs::read(registration.join("index")).unwrap();
    let destination = fixture.directory.join("手动重命名 后");
    fs::rename(&path, &destination).unwrap();
    Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Repair {
                path: path.clone(),
                destination: destination.clone(),
            }),
        )
        .unwrap();
    assert!(!path.exists());
    assert_eq!(
        branch_head(&destination),
        ("refs/heads/feature".into(), fixture.feature)
    );
    assert_eq!(
        gitfile_target(&destination),
        fs::canonicalize(&registration).unwrap()
    );
    let gitdir = fs::read_to_string(registration.join("gitdir")).unwrap();
    assert_eq!(
        fs::canonicalize(gitdir.trim()).unwrap(),
        fs::canonicalize(destination.join(".git")).unwrap()
    );
    assert_eq!(fs::read(registration.join("index")).unwrap(), index);
    fixture
        .repository()
        .find_worktree("linked")
        .unwrap()
        .validate()
        .unwrap();
    fixture.assert_main();
}

#[test]
fn repair_refuses_a_foreign_repository_destination_without_modifying_its_gitfile() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let registration = fixture.registration(&path);
    fixture.remove_owned_directory(&path);
    let other = Fixture::new();
    let destination = other.linked("foreign");
    let gitfile = fs::read(destination.join(".git")).unwrap();
    let gitdir = fs::read(registration.join("gitdir")).unwrap();
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Repair {
                path,
                destination: destination.clone(),
            }),
        )
        .unwrap_err();
    assert_eq!(error.kind, WorktreeErrorKind::InvalidRequest);
    assert_eq!(fs::read(destination.join(".git")).unwrap(), gitfile);
    assert_eq!(fs::read(registration.join("gitdir")).unwrap(), gitdir);
    assert_eq!(branch_head(&destination).1, other.feature);
    fixture.assert_main();
    other.assert_main();
}

#[test]
fn unregistered_targets_and_parent_directories_are_never_mutated() {
    let fixture = Fixture::new();
    let other = Fixture::new();
    let foreign = other.linked("foreign");
    for path in [
        &foreign,
        &fixture.directory,
        &fixture.base,
        &fixture.root.join(".git"),
    ] {
        for force in [false, true] {
            let action = WorktreeAction::Remove {
                path: path.clone(),
                force,
            };
            let error = Git2Backend
                .worktree(&fixture.root, &fixture.request(action))
                .unwrap_err();
            assert_eq!(error.kind, WorktreeErrorKind::InvalidRequest);
            assert!(path.exists());
        }
    }
    assert_eq!(branch_head(&foreign).1, other.feature);
    fixture.assert_main();
    other.assert_main();
}

#[test]
fn create_and_move_refuse_destinations_nested_inside_registered_worktrees() {
    let fixture = Fixture::new();
    let linked = fixture.linked("linked");
    for destination in [
        fixture.root.join("nested"),
        linked.join("nested"),
        fixture.root.join(".git/unsafe"),
    ] {
        for action in [
            WorktreeAction::Create {
                path: destination.clone(),
                branch: "nested-topic".into(),
                create_branch: true,
            },
            WorktreeAction::Move {
                path: linked.clone(),
                destination: destination.clone(),
            },
        ] {
            let error = Git2Backend
                .worktree(&fixture.root, &fixture.request(action))
                .unwrap_err();
            assert_eq!(error.kind, WorktreeErrorKind::InvalidRequest);
            assert!(!destination.exists());
            assert!(linked.exists());
        }
    }
    assert!(
        fixture
            .repository()
            .find_branch("nested-topic", BranchType::Local)
            .is_err()
    );
    fixture.assert_main();
}

#[test]
fn stale_branch_or_commit_identity_blocks_mutations_before_new_paths_are_created() {
    let fixture = Fixture::new();
    let destination = fixture.directory.join("stale");
    for stale_branch in [false, true] {
        let mut request = fixture.request(WorktreeAction::Create {
            path: destination.clone(),
            branch: "stale-topic".into(),
            create_branch: true,
        });
        if stale_branch {
            request.expected_head = Head::Branch("another-branch".into());
        } else {
            request.expected_commit = Some(fixture.feature.to_string());
        }
        let error = Git2Backend.worktree(&fixture.root, &request).unwrap_err();
        assert_eq!(error.kind, WorktreeErrorKind::Changed);
        assert!(!destination.exists());
        assert!(
            fixture
                .repository()
                .find_branch("stale-topic", BranchType::Local)
                .is_err()
        );
    }
    fixture.assert_main();
}

#[test]
fn held_target_index_lock_blocks_removal_and_preserves_the_lock() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let lock = fixture.registration(&path).join("index.lock");
    fs::write(&lock, "another process owns this lock").unwrap();
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            }),
        )
        .unwrap_err();
    assert_eq!(error.kind, WorktreeErrorKind::Busy);
    assert_eq!(
        Git2Backend
            .worktree(
                &fixture.root,
                &fixture.request(WorktreeAction::Remove {
                    path: path.clone(),
                    force: true
                }),
            )
            .unwrap_err()
            .kind,
        WorktreeErrorKind::Busy
    );
    assert_eq!(
        fs::read_to_string(lock).unwrap(),
        "another process owns this lock"
    );
    assert_eq!(branch_head(&path).1, fixture.feature);
    fixture.assert_main();
}

#[test]
fn details_handles_non_utf8_lock_reason_without_panicking_or_losing_the_worktree() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let lock = fixture.registration(&path).join("locked");
    let reason = [0xff, 0xfe, b'\n'];
    fs::write(&lock, reason).unwrap();
    let details = Git2Backend.details(&fixture.root).unwrap();
    let worktree = details
        .worktrees
        .iter()
        .find(|worktree| !worktree.is_main)
        .unwrap();
    assert!(worktree.locked);
    assert_eq!(worktree.branch.as_deref(), Some("feature"));
    assert_eq!(details.worktrees.len(), 2);
    assert_eq!(fs::read(lock).unwrap(), reason);
    fixture.assert_main();
}

#[test]
fn held_existing_branch_ref_lock_blocks_create_before_checkout_or_registration() {
    let fixture = Fixture::new();
    let lock = fixture.repository().path().join("refs/heads/feature.lock");
    fs::write(&lock, "another process owns this branch").unwrap();
    let destination = fixture.directory.join("blocked-existing-branch");
    let error = Git2Backend
        .worktree(
            &fixture.root,
            &fixture.request(WorktreeAction::Create {
                path: destination.clone(),
                branch: "feature".into(),
                create_branch: false,
            }),
        )
        .unwrap_err();
    assert_eq!(error.kind, WorktreeErrorKind::Busy);
    assert!(!destination.exists());
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 0);
    assert_eq!(
        fixture
            .repository()
            .refname_to_id("refs/heads/feature")
            .unwrap(),
        fixture.feature
    );
    assert_eq!(
        fs::read_to_string(lock).unwrap(),
        "another process owns this branch"
    );
    fixture.assert_main();
}

#[test]
fn held_managed_head_lock_blocks_remove_and_move_and_preserves_the_other_process_lock() {
    let fixture = Fixture::new();
    let path = fixture.linked("linked");
    let lock = fixture.registration(&path).join("HEAD.lock");
    fs::write(&lock, "another process owns this HEAD").unwrap();
    let destination = fixture.directory.join("blocked-move");
    for action in [
        WorktreeAction::Remove {
            path: path.clone(),
            force: false,
        },
        WorktreeAction::Remove {
            path: path.clone(),
            force: true,
        },
        WorktreeAction::Move {
            path: path.clone(),
            destination: destination.clone(),
        },
    ] {
        let error = Git2Backend
            .worktree(&fixture.root, &fixture.request(action))
            .unwrap_err();
        assert_eq!(error.kind, WorktreeErrorKind::Busy);
        assert_eq!(
            fs::read_to_string(&lock).unwrap(),
            "another process owns this HEAD"
        );
        assert!(!destination.exists());
        assert_eq!(branch_head(&path).1, fixture.feature);
        assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
    }
    fixture.assert_main();
}

#[test]
fn index_flags_that_hide_working_files_block_removal_and_move_without_losing_hidden_edits() {
    for assume_unchanged in [true, false] {
        let fixture = Fixture::new();
        let path = fixture.linked("linked");
        let repository = Repository::open(&path).unwrap();
        let mut index = repository.index().unwrap();
        let mut entry = index.get_path(Path::new("note.txt"), 0).unwrap();
        if assume_unchanged {
            entry.flags |= 0x8000;
        } else {
            entry.flags |= 0x4000;
            entry.flags_extended |= 0x4000;
        }
        index.add(&entry).unwrap();
        index.write().unwrap();
        index.read(true).unwrap();
        let stored = index.get_path(Path::new("note.txt"), 0).unwrap();
        assert!(if assume_unchanged {
            stored.flags & 0x8000 != 0
        } else {
            stored.flags_extended & 0x4000 != 0
        });
        let index_path = repository.path().join("index");
        drop(index);
        drop(repository);
        fs::write(
            path.join("note.txt"),
            "precious edit hidden from ordinary status\n",
        )
        .unwrap();
        let index_before = fs::read(&index_path).unwrap();
        let destination = fixture.directory.join("must-not-move-hidden-edits");
        for action in [
            WorktreeAction::Remove {
                path: path.clone(),
                force: false,
            },
            WorktreeAction::Move {
                path: path.clone(),
                destination: destination.clone(),
            },
        ] {
            let error = Git2Backend
                .worktree(&fixture.root, &fixture.request(action))
                .unwrap_err();
            assert_eq!(error.kind, WorktreeErrorKind::Unsupported);
            assert_eq!(fs::read(&index_path).unwrap(), index_before);
            assert_eq!(
                fs::read_to_string(path.join("note.txt")).unwrap(),
                "precious edit hidden from ordinary status\n"
            );
            assert!(!destination.exists());
            assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
        }
        fixture.assert_main();
    }
}

#[cfg(windows)]
#[test]
fn non_unicode_request_path_is_rejected_without_lossily_targeting_a_valid_worktree() {
    use std::os::windows::ffi::OsStringExt;

    let fixture = Fixture::new();
    let valid = fixture.linked("\u{fffd}");
    // This isolated surrogate is an in-memory OsString only. Converting it
    // lossily would alias the real, valid replacement-character worktree.
    let invalid = fixture
        .directory
        .join(std::ffi::OsString::from_wide(&[0xd800]));
    let destination = fixture.directory.join("must-not-move");
    for action in [
        WorktreeAction::Create {
            path: invalid.clone(),
            branch: "non-unicode-topic".into(),
            create_branch: true,
        },
        WorktreeAction::Remove {
            path: invalid.clone(),
            force: false,
        },
        WorktreeAction::Move {
            path: invalid.clone(),
            destination: destination.clone(),
        },
    ] {
        let error = Git2Backend
            .worktree(&fixture.root, &fixture.request(action))
            .unwrap_err();
        assert_eq!(error.kind, WorktreeErrorKind::InvalidRequest);
        assert_eq!(branch_head(&valid).1, fixture.feature);
        assert_eq!(
            fs::read_to_string(valid.join("note.txt")).unwrap(),
            "feature\n"
        );
        assert!(!destination.exists());
    }
    assert!(
        fixture
            .repository()
            .find_branch("non-unicode-topic", BranchType::Local)
            .is_err()
    );
    assert_eq!(fixture.repository().worktrees().unwrap().len(), 1);
    fixture.assert_main();
}
