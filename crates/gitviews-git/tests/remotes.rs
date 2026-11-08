//! Real repositories and the native local transport; no Git executable is used.
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use git2::{
    Oid, Repository, RepositoryInitOptions, Signature,
    build::{CheckoutBuilder, CloneLocal, RepoBuilder},
};
use gitviews_git::{
    Git2Backend, GitBackend, RemoteAction, RemoteControl, RemoteErrorKind, RemotePhase,
    RemoteRequest,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    base: PathBuf,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/git-remote-tests");
        fs::create_dir_all(&base).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = base.join(format!(
            "{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self { base, root }
    }

    fn repositories(&self) -> (Repository, Repository, Repository) {
        let seed = init(&self.root.join("seed"));
        commit(
            &seed,
            &[("file.txt", "base\n"), (".gitignore", "protected.txt\n")],
        );
        let bare = Repository::init_bare(self.root.join("remote.git")).unwrap();
        seed.remote("origin", bare.path().to_str().unwrap())
            .unwrap()
            .push(&["refs/heads/main:refs/heads/main"], None)
            .unwrap();
        bare.set_head("refs/heads/main").unwrap();
        let clone = |name: &str| {
            let mut checkout = CheckoutBuilder::new();
            checkout.disable_filters(true);
            let repository = RepoBuilder::new()
                .clone_local(CloneLocal::None)
                .with_checkout(checkout)
                .clone(bare.path().to_str().unwrap(), &self.root.join(name))
                .unwrap();
            repository
                .config()
                .unwrap()
                .set_bool("core.autocrlf", false)
                .unwrap();
            repository
        };
        let local = clone("local");
        let writer = clone("writer");
        (bare, local, writer)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Verify this resolved absolute target is the unique test directory.
        if let Ok(root) = fs::canonicalize(&self.root)
            && root != self.base
            && root.starts_with(&self.base)
        {
            let _ = fs::remove_dir_all(root);
        }
    }
}

fn init(path: &Path) -> Repository {
    let mut options = RepositoryInitOptions::new();
    options.initial_head("main");
    let repo = Repository::init_opts(path, &options).unwrap();
    repo.config()
        .unwrap()
        .set_bool("core.autocrlf", false)
        .unwrap();
    repo
}

fn commit(repo: &Repository, files: &[(&str, &str)]) -> Oid {
    let root = repo.workdir().unwrap();
    let mut index = repo.index().unwrap();
    index.read(true).unwrap();
    for (path, text) in files {
        let destination = root.join(path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(destination, text).unwrap();
        index.add_path(Path::new(path)).unwrap();
    }
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let signature = Signature::now("GitViews Tests", "gitviews@example.invalid").unwrap();
    let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
    let parents: Vec<_> = parent.iter().collect();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "fixture commit",
        &tree,
        &parents,
    )
    .unwrap()
}

fn head(repo: &Repository) -> Oid {
    repo.head().unwrap().target().unwrap()
}

fn request(repo: &Repository, action: RemoteAction) -> RemoteRequest {
    RemoteRequest {
        action,
        expected_branch: Some("main".into()),
        expected_head: Some(head(repo).to_string()),
    }
}

fn run(
    repo: &Repository,
    action: RemoteAction,
) -> Result<gitviews_git::RemoteOutcome, gitviews_git::RemoteError> {
    Git2Backend.remote(
        repo.workdir().unwrap(),
        &request(repo, action),
        &RemoteControl::default(),
    )
}

fn advance(writer: &Repository, text: &str) -> Oid {
    let oid = commit(writer, &[("file.txt", text)]);
    run(writer, RemoteAction::Push).unwrap();
    oid
}

#[test]
fn fetch_only_updates_tracking_refs_and_prefers_the_branch_remote() {
    let fixture = Fixture::new();
    let (_bare, local, writer) = fixture.repositories();
    let before = head(&local);
    let after = advance(&writer, "remote update\n");
    let original_url = local
        .find_remote("origin")
        .unwrap()
        .url()
        .unwrap()
        .to_owned();
    local.remote("preferred", &original_url).unwrap();
    local
        .config()
        .unwrap()
        .set_str("branch.main.remote", "preferred")
        .unwrap();
    let request = RemoteRequest {
        action: RemoteAction::Fetch,
        expected_branch: None,
        expected_head: None,
    };
    let outcome = Git2Backend
        .remote(
            local.workdir().unwrap(),
            &request,
            &RemoteControl::default(),
        )
        .unwrap();
    assert_eq!(outcome.remote, "preferred");
    assert_eq!(head(&local), before);
    assert_eq!(
        local.refname_to_id("refs/remotes/preferred/main").unwrap(),
        after
    );
    assert_eq!(
        fs::read_to_string(local.workdir().unwrap().join("file.txt")).unwrap(),
        "base\n"
    );
}

#[test]
fn pull_fast_forwards_only_the_current_branch_and_clean_worktree() {
    let fixture = Fixture::new();
    let (_bare, local, writer) = fixture.repositories();
    let before = head(&local);
    local
        .branch("other", &local.find_commit(before).unwrap(), false)
        .unwrap();
    let after = advance(&writer, "fast forward\n");
    let outcome = run(&local, RemoteAction::Pull).unwrap();
    assert_eq!(outcome.head_before, Some(before.to_string()));
    assert_eq!(outcome.head_after, Some(after.to_string()));
    assert_eq!(head(&local), after);
    assert_eq!(local.refname_to_id("refs/heads/other").unwrap(), before);
    assert_eq!(
        fs::read_to_string(local.workdir().unwrap().join("file.txt")).unwrap(),
        "fast forward\n"
    );
    assert!(local.statuses(None).unwrap().is_empty());
}

#[test]
fn pull_refuses_divergence_without_a_merge_reset_or_stash() {
    let fixture = Fixture::new();
    let (_bare, local, writer) = fixture.repositories();
    let local_id = commit(&local, &[("local.txt", "keep local commit\n")]);
    advance(&writer, "different remote line\n");
    assert_eq!(
        run(&local, RemoteAction::Pull).unwrap_err().kind,
        RemoteErrorKind::NonFastForward
    );
    assert_eq!(head(&local), local_id);
    assert_eq!(
        fs::read_to_string(local.workdir().unwrap().join("file.txt")).unwrap(),
        "base\n"
    );
    assert_eq!(
        fs::read_to_string(local.workdir().unwrap().join("local.txt")).unwrap(),
        "keep local commit\n"
    );
    assert_eq!(local.state(), git2::RepositoryState::Clean);
}

#[test]
fn pull_protects_working_index_and_untracked_changes() {
    for stage in [false, true] {
        let fixture = Fixture::new();
        let (_bare, local, writer) = fixture.repositories();
        advance(&writer, "remote data\n");
        let before = head(&local);
        fs::write(local.workdir().unwrap().join("file.txt"), "must survive\n").unwrap();
        if stage {
            let mut index = local.index().unwrap();
            index.add_path(Path::new("file.txt")).unwrap();
            index.write().unwrap();
        }
        assert_eq!(
            run(&local, RemoteAction::Pull).unwrap_err().kind,
            RemoteErrorKind::DirtyWorkingTree
        );
        assert_eq!(head(&local), before);
        assert_eq!(
            fs::read_to_string(local.workdir().unwrap().join("file.txt")).unwrap(),
            "must survive\n"
        );
    }
    let fixture = Fixture::new();
    let (_bare, local, writer) = fixture.repositories();
    advance(&writer, "remote data\n");
    fs::write(
        local.workdir().unwrap().join("untracked.txt"),
        "untracked data\n",
    )
    .unwrap();
    assert_eq!(
        run(&local, RemoteAction::Pull).unwrap_err().kind,
        RemoteErrorKind::DirtyWorkingTree
    );
    assert_eq!(
        fs::read_to_string(local.workdir().unwrap().join("untracked.txt")).unwrap(),
        "untracked data\n"
    );
}

#[test]
fn pull_does_not_overwrite_an_ignored_file_or_an_in_progress_operation() {
    let fixture = Fixture::new();
    let (_bare, local, writer) = fixture.repositories();
    commit(&writer, &[("protected.txt", "tracked remote content\n")]);
    run(&writer, RemoteAction::Push).unwrap();
    let before = head(&local);
    fs::write(
        local.workdir().unwrap().join("protected.txt"),
        "private ignored content\n",
    )
    .unwrap();
    let error = run(&local, RemoteAction::Pull).unwrap_err();
    assert_eq!(error.kind, RemoteErrorKind::DirtyWorkingTree);
    assert_eq!(head(&local), before);
    assert_eq!(
        fs::read_to_string(local.workdir().unwrap().join("protected.txt")).unwrap(),
        "private ignored content\n"
    );
    fs::write(local.path().join("MERGE_HEAD"), format!("{before}\n")).unwrap();
    assert_eq!(
        run(&local, RemoteAction::Pull).unwrap_err().kind,
        RemoteErrorKind::InProgress
    );
    assert_eq!(head(&local), before);
}

#[test]
fn push_ignores_other_branches_and_configured_push_refspecs() {
    let fixture = Fixture::new();
    let (bare, local, _writer) = fixture.repositories();
    let before = head(&local);
    local
        .branch("other", &local.find_commit(before).unwrap(), false)
        .unwrap();
    local
        .remote_add_push("origin", "refs/heads/other:refs/heads/other")
        .unwrap();
    let after = commit(&local, &[("file.txt", "only main\n")]);
    run(&local, RemoteAction::Push).unwrap();
    assert_eq!(bare.refname_to_id("refs/heads/main").unwrap(), after);
    assert!(bare.find_reference("refs/heads/other").is_err());
    assert_eq!(local.refname_to_id("refs/heads/other").unwrap(), before);
    let mut wrong = request(&local, RemoteAction::Push);
    wrong.expected_branch = Some("other".into());
    assert_eq!(
        Git2Backend
            .remote(local.workdir().unwrap(), &wrong, &RemoteControl::default())
            .unwrap_err()
            .kind,
        RemoteErrorKind::ConcurrentChange
    );
    assert_eq!(bare.refname_to_id("refs/heads/main").unwrap(), after);
}

#[test]
fn push_requires_a_same_named_upstream_and_never_forces_diverged_remote() {
    let fixture = Fixture::new();
    let (bare, local, writer) = fixture.repositories();
    let remote_id = advance(&writer, "writer advance\n");
    commit(&local, &[("local.txt", "different local commit\n")]);
    assert_eq!(
        run(&local, RemoteAction::Push).unwrap_err().kind,
        RemoteErrorKind::NonFastForward
    );
    assert_eq!(bare.refname_to_id("refs/heads/main").unwrap(), remote_id);
    local
        .config()
        .unwrap()
        .set_str("branch.main.merge", "refs/heads/different")
        .unwrap();
    assert_eq!(
        run(&local, RemoteAction::Push).unwrap_err().kind,
        RemoteErrorKind::NoUpstream
    );
    assert!(bare.find_reference("refs/heads/different").is_err());
    local
        .config()
        .unwrap()
        .remove("branch.main.remote")
        .unwrap();
    assert_eq!(
        run(&local, RemoteAction::Pull).unwrap_err().kind,
        RemoteErrorKind::NoUpstream
    );
}

#[test]
fn cancellation_is_checked_before_transfer_and_not_halfway_through_checkout() {
    let fixture = Fixture::new();
    let (_bare, local, writer) = fixture.repositories();
    let before = head(&local);
    let after = advance(&writer, "new working files\n");
    let cancelled = RemoteControl::default();
    cancelled.cancel.store(true, Ordering::Release);
    assert_eq!(
        Git2Backend
            .remote(
                local.workdir().unwrap(),
                &request(&local, RemoteAction::Pull),
                &cancelled
            )
            .unwrap_err()
            .kind,
        RemoteErrorKind::Cancelled
    );
    assert_eq!(head(&local), before);
    let mut control = RemoteControl::default();
    let cancel = Arc::clone(&control.cancel);
    control.progress = Some(Arc::new(move |progress| {
        if progress.phase == RemotePhase::CheckingOut {
            cancel.store(true, Ordering::Release);
        }
    }));
    Git2Backend
        .remote(
            local.workdir().unwrap(),
            &request(&local, RemoteAction::Pull),
            &control,
        )
        .unwrap();
    assert!(control.cancel.load(Ordering::Acquire));
    assert_eq!(head(&local), after);
    assert!(local.statuses(None).unwrap().is_empty());
}

#[test]
fn pull_reports_partial_checkout_if_the_final_index_write_is_locked() {
    let fixture = Fixture::new();
    let (_bare, local, writer) = fixture.repositories();
    let before = head(&local);
    let fetched = advance(&writer, "new content before index write\n");
    let index_lock = local.path().join("index.lock");
    let callback_lock = index_lock.clone();
    let injected = Arc::new(AtomicBool::new(false));
    let callback_injected = Arc::clone(&injected);
    let control = RemoteControl {
        progress: Some(Arc::new(move |progress| {
            if progress.phase == RemotePhase::CheckingOut
                && progress.received_objects > 0
                && !callback_injected.swap(true, Ordering::AcqRel)
            {
                // Checkout progress runs after updating a file, before the
                // final git_index_write. Model a competing index writer.
                fs::write(&callback_lock, "external index writer\n").unwrap();
            }
        })),
        ..Default::default()
    };
    let error = Git2Backend
        .remote(
            local.workdir().unwrap(),
            &request(&local, RemoteAction::Pull),
            &control,
        )
        .unwrap_err();
    assert!(injected.load(Ordering::Acquire));
    assert_eq!(error.kind, RemoteErrorKind::ConcurrentChange);
    assert!(error.message.contains("可能已部分更新"));
    assert!(error.message.contains("尚未快进分支引用"));
    assert_eq!(head(&local), before);
    assert_eq!(
        local.refname_to_id("refs/remotes/origin/main").unwrap(),
        fetched
    );
    assert_eq!(
        fs::read_to_string(local.workdir().unwrap().join("file.txt")).unwrap(),
        "new content before index write\n"
    );
    assert_eq!(
        fs::read_to_string(&index_lock).unwrap(),
        "external index writer\n"
    );
    assert!(!local.path().join("HEAD.lock").exists());
    assert!(!local.path().join("refs/heads/main.lock").exists());
    fs::remove_file(index_lock).unwrap();
}

#[test]
fn fetch_cancellation_during_object_transfer_preserves_local_head_and_files() {
    let fixture = Fixture::new();
    let (_bare, local, writer) = fixture.repositories();
    let before = head(&local);
    advance(&writer, "remote objects to cancel\n");
    let mut control = RemoteControl::default();
    let cancel = Arc::clone(&control.cancel);
    control.progress = Some(Arc::new(move |progress| {
        if progress.phase == RemotePhase::Receiving {
            cancel.store(true, Ordering::Release);
        }
    }));
    assert_eq!(
        Git2Backend
            .remote(
                local.workdir().unwrap(),
                &request(&local, RemoteAction::Fetch),
                &control
            )
            .unwrap_err()
            .kind,
        RemoteErrorKind::Cancelled
    );
    assert_eq!(head(&local), before);
    assert_eq!(
        fs::read_to_string(local.workdir().unwrap().join("file.txt")).unwrap(),
        "base\n"
    );
}

#[test]
fn fetch_uses_a_unique_remote_without_upstream_and_rejects_ambiguous_or_nonroot_requests() {
    let fixture = Fixture::new();
    let (_bare, local, writer) = fixture.repositories();
    let after = advance(&writer, "remote update\n");
    let url = local
        .find_remote("origin")
        .unwrap()
        .url()
        .unwrap()
        .to_owned();
    local
        .config()
        .unwrap()
        .remove("branch.main.remote")
        .unwrap();
    local.remote_delete("origin").unwrap();
    local.remote("only", &url).unwrap();
    let request = RemoteRequest {
        action: RemoteAction::Fetch,
        expected_branch: None,
        expected_head: None,
    };
    let result = Git2Backend
        .remote(
            local.workdir().unwrap(),
            &request,
            &RemoteControl::default(),
        )
        .unwrap();
    assert_eq!(result.remote, "only");
    assert_eq!(
        local.refname_to_id("refs/remotes/only/main").unwrap(),
        after
    );
    local.remote("another", &url).unwrap();
    assert_eq!(
        Git2Backend
            .remote(
                local.workdir().unwrap(),
                &request,
                &RemoteControl::default()
            )
            .unwrap_err()
            .kind,
        RemoteErrorKind::NoRemote
    );
    let subdirectory = local.workdir().unwrap().join("ordinary directory");
    fs::create_dir(&subdirectory).unwrap();
    assert_eq!(
        Git2Backend
            .remote(&subdirectory, &request, &RemoteControl::default())
            .unwrap_err()
            .kind,
        RemoteErrorKind::InvalidRepository
    );
}

#[test]
fn a_head_change_during_fetch_prevents_pull_from_updating_the_new_branch() {
    let fixture = Fixture::new();
    let (_bare, local, writer) = fixture.repositories();
    let before = head(&local);
    let other = commit(&local, &[("file.txt", "other commit\n")]);
    local
        .reset(
            &local.find_object(before, None).unwrap(),
            git2::ResetType::Hard,
            None,
        )
        .unwrap();
    advance(&writer, "remote advance\n");
    let frozen = request(&local, RemoteAction::Pull);
    let directory = local.workdir().unwrap().to_owned();
    let once = AtomicBool::new(false);
    let control = RemoteControl {
        progress: Some(Arc::new(move |progress| {
            if progress.phase == RemotePhase::Connecting && !once.swap(true, Ordering::AcqRel) {
                Repository::open(&directory)
                    .unwrap()
                    .find_reference("refs/heads/main")
                    .unwrap()
                    .set_target(other, "concurrent update")
                    .unwrap();
            }
        })),
        ..Default::default()
    };
    assert_eq!(
        Git2Backend
            .remote(local.workdir().unwrap(), &frozen, &control)
            .unwrap_err()
            .kind,
        RemoteErrorKind::ConcurrentChange
    );
    assert_eq!(head(&local), other);
    assert_eq!(
        fs::read_to_string(local.workdir().unwrap().join("file.txt")).unwrap(),
        "base\n"
    );
}

#[test]
fn push_rechecks_head_during_negotiation_before_sending_any_ref_update() {
    let fixture = Fixture::new();
    let (bare, local, _writer) = fixture.repositories();
    let before = head(&local);
    local
        .branch("other", &local.find_commit(before).unwrap(), false)
        .unwrap();
    commit(&local, &[("file.txt", "planned main commit\n")]);
    let frozen = request(&local, RemoteAction::Push);
    let metadata = local.path().to_owned();
    let once = AtomicBool::new(false);
    let control = RemoteControl {
        progress: Some(Arc::new(move |progress| {
            if progress.phase == RemotePhase::Negotiating && !once.swap(true, Ordering::AcqRel) {
                // Model an external writer bypassing normal HEAD.lock protection.
                fs::write(metadata.join("HEAD"), "ref: refs/heads/other\n").unwrap();
            }
        })),
        ..Default::default()
    };
    assert_eq!(
        Git2Backend
            .remote(local.workdir().unwrap(), &frozen, &control)
            .unwrap_err()
            .kind,
        RemoteErrorKind::ConcurrentChange
    );
    assert_eq!(bare.refname_to_id("refs/heads/main").unwrap(), before);
    assert!(bare.find_reference("refs/heads/other").is_err());
}

#[test]
fn cancellation_after_server_acceptance_does_not_report_a_successful_push_as_cancelled() {
    let fixture = Fixture::new();
    let (bare, local, _writer) = fixture.repositories();
    let after = commit(&local, &[("file.txt", "accepted before cancellation\n")]);
    let mut control = RemoteControl::default();
    let cancel = Arc::clone(&control.cancel);
    control.progress = Some(Arc::new(move |progress| {
        if progress.phase == RemotePhase::UpdatingReferences {
            cancel.store(true, Ordering::Release);
        }
    }));
    Git2Backend
        .remote(
            local.workdir().unwrap(),
            &request(&local, RemoteAction::Push),
            &control,
        )
        .unwrap();
    assert!(control.cancel.load(Ordering::Acquire));
    assert_eq!(bare.refname_to_id("refs/heads/main").unwrap(), after);
}

#[test]
fn pull_uses_the_fetched_commit_even_if_an_external_fetch_changes_the_tracking_ref() {
    let fixture = Fixture::new();
    let (_bare, local, writer) = fixture.repositories();
    let before = head(&local);
    let other = commit(&local, &[("other.txt", "unrelated local commit\n")]);
    local
        .reset(
            &local.find_object(before, None).unwrap(),
            git2::ResetType::Hard,
            None,
        )
        .unwrap();
    let fetched = advance(&writer, "actual fetched content\n");
    let directory = local.workdir().unwrap().to_owned();
    let once = AtomicBool::new(false);
    let control = RemoteControl {
        progress: Some(Arc::new(move |progress| {
            if progress.phase == RemotePhase::Negotiating && !once.swap(true, Ordering::AcqRel) {
                Repository::open(&directory)
                    .unwrap()
                    .find_reference("refs/remotes/origin/main")
                    .unwrap()
                    .set_target(other, "concurrent tracking update")
                    .unwrap();
            }
        })),
        ..Default::default()
    };
    Git2Backend
        .remote(
            local.workdir().unwrap(),
            &request(&local, RemoteAction::Pull),
            &control,
        )
        .unwrap();
    assert_eq!(head(&local), fetched);
    assert_eq!(
        local.refname_to_id("refs/remotes/origin/main").unwrap(),
        other
    );
    assert_eq!(
        fs::read_to_string(local.workdir().unwrap().join("file.txt")).unwrap(),
        "actual fetched content\n"
    );
}
