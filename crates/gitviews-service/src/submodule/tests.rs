use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use gitviews_git::{Git2Backend, GitBackend, GitError};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crate::{RepositoryService, RepositorySnapshot, RepositoryTree, ServiceError};

struct Fixture {
    base: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let base =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/service-submodule-tests");
        fs::create_dir_all(&base).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = base.join(format!("{}-{unique}", std::process::id()));
        fs::create_dir(&root).unwrap();
        Self { base, root }
    }

    fn with_module() -> (Self, PathBuf, PathBuf) {
        Self::with_module_at("deps/native lib")
    }

    fn with_module_at(relative: &str) -> (Self, PathBuf, PathBuf) {
        let fixture = Self::new();
        let source = fixture.root.join("source");
        let parent = fixture.root.join("parent");
        init(&source);
        fs::write(source.join("shared.txt"), "child base\n").unwrap();
        commit_all(&source);
        init(&parent);
        fs::write(parent.join("shared.txt"), "parent base\n").unwrap();
        commit_all(&parent);

        // The file transport copies objects instead of making local-clone
        // hardlinks. No network access or link-creation permission is needed.
        let source_path = source.to_string_lossy().replace('\\', "/");
        let url = if cfg!(windows) {
            format!("file:///{}", source_path.trim_start_matches("//?/"))
        } else {
            format!("file://{source_path}")
        };
        git(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
                &url,
                relative,
            ],
        );
        commit_all(&parent);
        let child = parent.join(relative);
        (fixture, parent, child)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Only remove the resolved unique directory beneath this test's base.
        if let Ok(root) = fs::canonicalize(&self.root)
            && root != self.base
            && root.starts_with(&self.base)
        {
            let _ = fs::remove_dir_all(root);
        }
    }
}

fn git(root: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
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
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("LC_ALL", "C");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let output = command
        .output()
        .expect("Git is required for repository fixture setup");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init(root: &Path) {
    fs::create_dir(root).unwrap();
    git(root, &["init", "--quiet", "--initial-branch=main"]);
}

fn commit_all(root: &Path) {
    git(root, &["add", "--all"]);
    git(root, &["commit", "--quiet", "--message", "Fixture commit"]);
}

#[test]
fn initialized_submodule_reads_isolate_same_named_parent_and_child_files() {
    let (_fixture, parent, child) = Fixture::with_module();
    fs::write(parent.join("shared.txt"), "parent working\n").unwrap();
    fs::write(child.join("shared.txt"), "child working\n").unwrap();
    let service = RepositoryService::default();
    let parent_before = service.open(&parent).unwrap();
    let module = service.submodule(&parent, &child).unwrap();
    assert_eq!(module.repository.root, fs::canonicalize(&child).unwrap());
    assert!(
        module
            .repository
            .changes
            .iter()
            .any(|change| change.path == "shared.txt")
    );
    assert!(
        module
            .tree
            .entries
            .iter()
            .any(|entry| entry.path == "shared.txt")
    );
    assert!(
        !module
            .tree
            .entries
            .iter()
            .any(|entry| entry.path == ".gitmodules")
    );
    assert_eq!(
        service
            .source(&module.repository.root, "shared.txt", None)
            .unwrap()
            .text,
        "child working\n"
    );
    assert_eq!(
        service.source(&parent, "shared.txt", None).unwrap().text,
        "parent working\n"
    );
    let diff = service
        .diff(&module.repository.root, "shared.txt", false)
        .unwrap();
    assert_eq!(diff.old_text, "child base\n");
    assert_eq!(diff.new_text, "child working\n");
    let parent_after = service.open(&parent).unwrap();
    assert_eq!(parent_before.root, parent_after.root);
    assert_eq!(parent_before.head, parent_after.head);
    service.validate_root(&child).unwrap();
}

#[test]
fn deinitialized_submodule_never_falls_back_to_its_parent() {
    let (_fixture, parent, child) = Fixture::with_module();
    let service = RepositoryService::default();
    service.submodule(&parent, &child).unwrap();
    git(
        &parent,
        &["submodule", "deinit", "--force", "--", "deps/native lib"],
    );
    // Ordinary opening demonstrates why an exact-root gate is required.
    assert_eq!(
        service.open(&child).unwrap().root,
        fs::canonicalize(&parent).unwrap()
    );
    assert!(matches!(
        service.submodule(&parent, &child),
        Err(ServiceError::Submodule { .. })
    ));
    assert!(matches!(
        service.validate_root(&child),
        Err(ServiceError::Submodule { .. })
    ));
}

#[test]
fn ordinary_directories_nested_repositories_and_escaping_paths_are_rejected() {
    let fixture = Fixture::new();
    let parent = fixture.root.join("parent");
    init(&parent);
    fs::write(parent.join("shared.txt"), "parent\n").unwrap();
    commit_all(&parent);
    let plain = parent.join("ordinary");
    fs::create_dir(&plain).unwrap();
    let nested = parent.join("nested");
    init(&nested);
    let outside = fixture.root.join("outside");
    init(&outside);
    let service = RepositoryService::default();
    for candidate in [&plain, &nested, &outside, &parent, &plain.join("..")] {
        assert!(
            matches!(
                service.submodule(&parent, candidate),
                Err(ServiceError::Submodule { .. })
            ),
            "accepted {}",
            candidate.display()
        );
    }
    assert!(matches!(
        service.validate_root(&plain),
        Err(ServiceError::Submodule { .. })
    ));
}

/// Simulate deinitialization between independently opened backend operations.
struct LoseChildAfterTree {
    child: PathBuf,
}

impl GitBackend for LoseChildAfterTree {
    fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
        Git2Backend.inspect(path)
    }

    fn tree(&self, path: &Path) -> Result<RepositoryTree, GitError> {
        let result = Git2Backend.tree(path)?;
        if fs::canonicalize(path).unwrap() == self.child {
            fs::remove_file(self.child.join(".git")).unwrap();
        }
        Ok(result)
    }
}

#[test]
fn a_child_lost_during_tree_read_is_rejected_before_returning_a_snapshot() {
    let (_fixture, parent, child) = Fixture::with_module();
    let service = RepositoryService::new(LoseChildAfterTree {
        child: fs::canonicalize(&child).unwrap(),
    });
    assert!(matches!(
        service.submodule(&parent, &child),
        Err(ServiceError::Submodule { .. })
    ));
}

#[derive(Default)]
struct ReadCounts {
    inspect: AtomicUsize,
    metadata: AtomicUsize,
    roots: AtomicUsize,
    registration: AtomicUsize,
    trees: AtomicUsize,
}

struct CountedNative {
    child: PathBuf,
    counts: Arc<ReadCounts>,
}

impl GitBackend for CountedNative {
    fn inspect_metadata(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
        assert_eq!(fs::canonicalize(path).unwrap(), self.child);
        self.counts.metadata.fetch_add(1, Ordering::Relaxed);
        Git2Backend.inspect_metadata(path)
    }

    fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
        assert_eq!(fs::canonicalize(path).unwrap(), self.child);
        self.counts.inspect.fetch_add(1, Ordering::Relaxed);
        Git2Backend.inspect(path)
    }

    fn repository_root(&self, path: &Path) -> Result<PathBuf, GitError> {
        self.counts.roots.fetch_add(1, Ordering::Relaxed);
        Git2Backend.repository_root(path)
    }

    fn registered_submodule(
        &self,
        parent: &Path,
        relative: &str,
    ) -> Result<Option<gitviews_git::SubmoduleInfo>, GitError> {
        self.counts.registration.fetch_add(1, Ordering::Relaxed);
        Git2Backend.registered_submodule(parent, relative)
    }

    fn tree(&self, _: &Path) -> Result<RepositoryTree, GitError> {
        panic!("submodule selection must reuse status, never scan the parent's tree");
    }

    fn tree_with_changes(
        &self,
        path: &Path,
        changes: &[gitviews_git::FileChange],
    ) -> Result<RepositoryTree, GitError> {
        assert_eq!(fs::canonicalize(path).unwrap(), self.child);
        assert!(changes.iter().any(|change| change.path == "shared.txt"));
        self.counts.trees.fetch_add(1, Ordering::Relaxed);
        Git2Backend.tree_with_changes(path, changes)
    }
}

#[test]
fn child_metadata_is_available_without_reading_its_status_or_tree() {
    let (_fixture, parent, child) = Fixture::with_module();
    let marker = fs::read_to_string(child.join(".git")).unwrap();
    let gitdir = child.join(marker.trim().strip_prefix("gitdir: ").unwrap());
    fs::write(gitdir.join("index"), b"invalid index fixture").unwrap();
    let counts = Arc::new(ReadCounts::default());
    let service = RepositoryService::new(CountedNative {
        child: fs::canonicalize(&child).unwrap(),
        counts: Arc::clone(&counts),
    });
    let snapshot = service
        .submodule_metadata_with_cancel(&parent, &child, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(snapshot.root, fs::canonicalize(child).unwrap());
    assert!(snapshot.changes.is_empty());
    assert!(snapshot.recent_commits.is_empty());
    assert_eq!(counts.inspect.load(Ordering::Relaxed), 0);
    assert_eq!(counts.metadata.load(Ordering::Relaxed), 1);
    assert_eq!(counts.trees.load(Ordering::Relaxed), 0);
}

#[test]
fn submodule_selection_reads_one_child_overview_and_reuses_its_status() {
    let (_fixture, parent, child) = Fixture::with_module();
    fs::write(child.join("shared.txt"), "child modified\n").unwrap();
    let counts = Arc::new(ReadCounts::default());
    let service = RepositoryService::new(CountedNative {
        child: fs::canonicalize(&child).unwrap(),
        counts: Arc::clone(&counts),
    });
    let snapshot = service.submodule(&parent, &child).unwrap();
    assert!(
        snapshot
            .tree
            .entries
            .iter()
            .any(|entry| { entry.path == "shared.txt" && entry.worktree_status == 'M' })
    );
    assert_eq!(counts.inspect.load(Ordering::Relaxed), 1);
    assert_eq!(counts.registration.load(Ordering::Relaxed), 1);
    assert_eq!(counts.trees.load(Ordering::Relaxed), 1);
    assert_eq!(counts.roots.load(Ordering::Relaxed), 3);
    service.validate_root(&child).unwrap();
    service.validate_root(&child).unwrap();
    assert_eq!(counts.inspect.load(Ordering::Relaxed), 1);
    assert_eq!(counts.roots.load(Ordering::Relaxed), 5);
}

#[test]
fn exact_root_validation_does_not_require_readable_status_or_commit_history() {
    let (_fixture, _, child) = Fixture::with_module();
    let child_repository = gitviews_git::Git2Backend.repository_root(&child).unwrap();
    // Corrupt only this disposable fixture's index. Root validation must still
    // work, while the operation that actually needs status reports the error.
    let git_marker = fs::read_to_string(child_repository.join(".git")).unwrap();
    let gitdir = git_marker.trim().strip_prefix("gitdir: ").unwrap();
    let gitdir = child_repository.join(gitdir);
    fs::write(gitdir.join("index"), b"invalid index fixture").unwrap();
    let service = RepositoryService::default();
    service.validate_root(&child).unwrap();
    assert!(service.open(&child).is_err());
}

#[test]
fn targeted_registration_and_exact_root_checks_support_chinese_submodule_paths() {
    let (_fixture, parent, child) = Fixture::with_module_at("依赖/原生 库");
    fs::write(child.join("shared.txt"), "中文子模块变更\n").unwrap();
    let snapshot = RepositoryService::default()
        .submodule(&parent, &child)
        .unwrap();
    assert_eq!(snapshot.repository.root, fs::canonicalize(child).unwrap());
    assert!(
        snapshot
            .repository
            .changes
            .iter()
            .any(|change| change.path == "shared.txt")
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CancelAfter {
    BeforeStart,
    ParentRoot,
    Registration,
    Overview,
    Tree,
}

struct CancellingBackend {
    phase: CancelAfter,
    cancel: Arc<AtomicBool>,
    counts: Arc<ReadCounts>,
}

impl CancellingBackend {
    fn finish(&self, phase: CancelAfter) {
        if self.phase == phase {
            self.cancel.store(true, Ordering::Release);
        }
    }
}

impl GitBackend for CancellingBackend {
    fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
        self.counts.inspect.fetch_add(1, Ordering::Relaxed);
        self.finish(CancelAfter::Overview);
        Ok(RepositorySnapshot {
            root: path.to_owned(),
            name: "child".into(),
            head: crate::Head::Unborn("main".into()),
            changes: Vec::new(),
            recent_commits: Vec::new(),
        })
    }

    fn repository_root(&self, path: &Path) -> Result<PathBuf, GitError> {
        if self.counts.roots.fetch_add(1, Ordering::Relaxed) == 0 {
            self.finish(CancelAfter::ParentRoot);
        }
        Ok(path.to_owned())
    }

    fn registered_submodule(
        &self,
        _: &Path,
        relative: &str,
    ) -> Result<Option<gitviews_git::SubmoduleInfo>, GitError> {
        self.counts.registration.fetch_add(1, Ordering::Relaxed);
        self.finish(CancelAfter::Registration);
        Ok(Some(gitviews_git::SubmoduleInfo {
            name: relative.into(),
            path: relative.into(),
            url: None,
            head_id: None,
            index_id: None,
            workdir_id: None,
            initialized: true,
            dirty: false,
        }))
    }

    fn tree_with_changes(
        &self,
        _: &Path,
        _: &[gitviews_git::FileChange],
    ) -> Result<RepositoryTree, GitError> {
        self.counts.trees.fetch_add(1, Ordering::Relaxed);
        self.finish(CancelAfter::Tree);
        Ok(RepositoryTree::default())
    }
}

#[test]
fn superseded_submodule_reads_stop_before_the_next_backend_phase() {
    let fixture = Fixture::new();
    let parent = fixture.root.join("parent");
    let child = parent.join("child");
    fs::create_dir_all(&child).unwrap();
    for (phase, expected) in [
        (CancelAfter::BeforeStart, (0, 0, 0, 0)),
        (CancelAfter::ParentRoot, (1, 0, 0, 0)),
        (CancelAfter::Registration, (1, 1, 0, 0)),
        (CancelAfter::Overview, (2, 1, 1, 0)),
        (CancelAfter::Tree, (2, 1, 1, 1)),
    ] {
        let cancel = Arc::new(AtomicBool::new(phase == CancelAfter::BeforeStart));
        let counts = Arc::new(ReadCounts::default());
        let service = RepositoryService::new(CancellingBackend {
            phase,
            cancel: Arc::clone(&cancel),
            counts: Arc::clone(&counts),
        });
        let error = service
            .submodule_with_cancel(&parent, &child, &cancel)
            .unwrap_err();
        assert!(matches!(
            error,
            ServiceError::Cancelled {
                operation: "read submodule"
            }
        ));
        assert_eq!(
            (
                counts.roots.load(Ordering::Relaxed),
                counts.registration.load(Ordering::Relaxed),
                counts.inspect.load(Ordering::Relaxed),
                counts.trees.load(Ordering::Relaxed),
            ),
            expected,
            "unexpected backend work after cancellation at {phase:?}"
        );
    }
}
