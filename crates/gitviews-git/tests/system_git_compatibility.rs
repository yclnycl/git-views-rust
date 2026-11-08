//! Local-only compatibility fixtures. No personal keys, settings or remote hosts.
use git2::{Repository, RepositoryInitOptions, Signature};
use gitviews_git::*;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    directory: PathBuf,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/system-git-tests");
        fs::create_dir_all(&parent).unwrap();
        let directory = parent.join(format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let directory = fs::canonicalize(directory).unwrap();
        let root = directory.join("中文 仓库");
        let mut options = RepositoryInitOptions::new();
        options.initial_head("main");
        let repo = Repository::init_opts(&root, &options).unwrap();
        let mut config = repo.config().unwrap();
        config
            .set_str("user.name", "Compatibility Fixture")
            .unwrap();
        config
            .set_str("user.email", "fixture@example.invalid")
            .unwrap();
        config.set_bool("commit.gpgsign", false).unwrap();
        config.set_bool("core.autocrlf", false).unwrap();
        config.set_str("core.hooksPath", ".fixture-hooks").unwrap();
        config.set_str("credential.helper", "").unwrap();
        config.set_str("core.askPass", "true").unwrap();
        fs::create_dir(root.join(".fixture-hooks")).unwrap();
        fs::write(repo.path().join("info/exclude"), ".fixture-hooks/\n*-ran\n").unwrap();
        Self { directory, root }
    }
    fn repo(&self) -> Repository {
        Repository::open(&self.root).unwrap()
    }
    fn stage(&self) {
        fs::write(self.root.join("note.txt"), "fixture content\n").unwrap();
        let repo = self.repo();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("note.txt")).unwrap();
        index.write().unwrap();
    }
    fn head(&self) -> Option<String> {
        self.repo()
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|id| id.to_string())
    }
    fn seed(&self) -> String {
        self.stage();
        let repo = self.repo();
        let mut index = repo.index().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = Signature::now("Fixture", "fixture@example.invalid").unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap()
            .to_string()
    }
    fn hook(&self, name: &str, contents: &str) {
        let path = self.root.join(".fixture-hooks").join(name);
        fs::write(&path, format!("#!/bin/sh\n{contents}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    fn system(&self) -> SystemGit {
        SystemGit::new(git_executable())
    }
    fn commit(&self) -> WriteRequest {
        WriteRequest {
            expected_head: self.head(),
            action: WriteAction::Commit {
                summary: "fixture commit".into(),
                description: String::new(),
            },
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}
fn git_executable() -> PathBuf {
    let name = if cfg!(windows) { "git.exe" } else { "git" };
    std::env::split_paths(
        &std::env::var_os("PATH").expect("Git must be on PATH for compatibility tests"),
    )
    .map(|path| path.join(name))
    .find(|path| path.is_file())
    .expect("Git must be on PATH for compatibility tests")
}

#[test]
fn native_commit_refuses_active_custom_hook_instead_of_bypassing_it() {
    let fixture = Fixture::new();
    fixture.stage();
    fixture.hook("pre-commit", "echo called > hook-ran\nexit 0");
    let error = Git2Backend
        .write(&fixture.root, &fixture.commit())
        .unwrap_err();
    assert_eq!(error.kind, WriteErrorKind::Unsupported);
    assert!(error.message.contains("pre-commit"));
    assert!(fixture.head().is_none());
    assert!(!fixture.root.join("hook-ran").exists());
}

#[test]
fn system_commit_obeys_rejecting_hook_without_moving_head() {
    let fixture = Fixture::new();
    fixture.stage();
    fixture.hook(
        "pre-commit",
        "echo called > hook-ran\necho fixture-hook-rejected >&2\nexit 1",
    );
    let error = fixture
        .system()
        .write(&fixture.root, &fixture.commit())
        .unwrap_err();
    assert!(error.message.contains("fixture-hook-rejected"));
    assert!(fixture.head().is_none());
    assert!(fixture.root.join("hook-ran").exists());
}

#[test]
fn system_commit_obeys_message_and_post_commit_hooks() {
    let fixture = Fixture::new();
    fixture.stage();
    fixture.hook(
        "commit-msg",
        "printf '\\nFixture-Hook: passed\\n' >> \"$1\"",
    );
    fixture.hook("post-commit", "echo called > post-commit-ran");
    let outcome = fixture
        .system()
        .write(&fixture.root, &fixture.commit())
        .unwrap();
    let repo = fixture.repo();
    let commit = repo
        .find_commit(git2::Oid::from_str(outcome.commit_id.as_deref().unwrap()).unwrap())
        .unwrap();
    assert!(commit.message().unwrap().contains("Fixture-Hook: passed"));
    assert!(fixture.root.join("post-commit-ran").exists());
}

#[test]
fn native_checkout_refuses_hook_and_system_checkout_executes_it() {
    let fixture = Fixture::new();
    let initial = fixture.seed();
    let repo = fixture.repo();
    repo.branch(
        "topic",
        &repo
            .find_commit(git2::Oid::from_str(&initial).unwrap())
            .unwrap(),
        false,
    )
    .unwrap();
    fixture.hook("post-checkout", "echo called > post-checkout-ran");
    let request = CheckoutRequest {
        branch: "topic".into(),
        remote: false,
        expected_branch: Some("main".into()),
        expected_head: Some(initial),
    };
    assert!(
        Git2Backend
            .checkout(&fixture.root, &request)
            .unwrap_err()
            .message
            .contains("post-checkout")
    );
    fixture.system().checkout(&fixture.root, &request).unwrap();
    assert_eq!(fixture.repo().head().unwrap().shorthand().unwrap(), "topic");
    assert!(fixture.root.join("post-checkout-ran").exists());
}

#[test]
fn system_git_creates_ssh_signed_commit_with_temporary_key() {
    let fixture = Fixture::new();
    fixture.stage();
    let key = fixture.directory.join("temporary-signing-key");
    let keygen = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&key)
        .output();
    let Ok(keygen) = keygen else {
        eprintln!("ssh-keygen unavailable; SSH signing fixture skipped");
        return;
    };
    assert!(
        keygen.status.success(),
        "temporary ssh-keygen failed: {}",
        String::from_utf8_lossy(&keygen.stderr)
    );
    let repo = fixture.repo();
    let mut config = repo.config().unwrap();
    config.set_str("gpg.format", "ssh").unwrap();
    config
        .set_str("user.signingkey", &display_path(&key))
        .unwrap();
    config.set_bool("commit.gpgsign", true).unwrap();
    assert_eq!(
        Git2Backend
            .write(&fixture.root, &fixture.commit())
            .unwrap_err()
            .kind,
        WriteErrorKind::Unsupported
    );
    let outcome = fixture
        .system()
        .write(&fixture.root, &fixture.commit())
        .unwrap();
    let commit = repo
        .find_commit(git2::Oid::from_str(outcome.commit_id.as_deref().unwrap()).unwrap())
        .unwrap();
    assert!(
        String::from_utf8_lossy(commit.raw_header_bytes())
            .contains("gpgsig -----BEGIN SSH SIGNATURE-----")
    );
}

#[test]
fn system_push_obeys_pre_push_hook_and_native_refuses_to_bypass_it() {
    let fixture = Fixture::new();
    let initial = fixture.seed();
    let remote = Repository::init_bare(fixture.directory.join("remote.git")).unwrap();
    let repo = fixture.repo();
    repo.remote("origin", &display_path(remote.path())).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("branch.main.remote", "origin").unwrap();
    config
        .set_str("branch.main.merge", "refs/heads/main")
        .unwrap();
    fixture.hook("pre-push", "echo called > pre-push-ran\nexit 1");
    let request = RemoteRequest {
        action: RemoteAction::Push,
        expected_branch: Some("main".into()),
        expected_head: Some(initial.clone()),
    };
    assert_eq!(
        Git2Backend
            .remote(&fixture.root, &request, &RemoteControl::default())
            .unwrap_err()
            .kind,
        RemoteErrorKind::Unsupported
    );
    assert!(
        fixture
            .system()
            .remote(&fixture.root, &request, &RemoteControl::default())
            .is_err()
    );
    assert!(fixture.root.join("pre-push-ran").exists());
    assert!(remote.find_reference("refs/heads/main").is_err());
    fs::remove_file(fixture.root.join(".fixture-hooks/pre-push")).unwrap();
    fixture
        .system()
        .remote(&fixture.root, &request, &RemoteControl::default())
        .unwrap();
    assert_eq!(
        remote
            .find_reference("refs/heads/main")
            .unwrap()
            .target()
            .unwrap()
            .to_string(),
        initial
    );
    fixture
        .system()
        .remote(
            &fixture.root,
            &RemoteRequest {
                action: RemoteAction::Fetch,
                expected_branch: None,
                expected_head: None,
            },
            &RemoteControl::default(),
        )
        .unwrap();
    assert!(repo.find_reference("refs/remotes/origin/main").is_ok());
}

#[test]
fn system_remote_uses_configured_credential_helper_and_redacts_auth_errors() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::{Duration, Instant},
    };
    let fixture = Fixture::new();
    fixture.seed();
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    server.set_nonblocking(true).unwrap();
    let address = server.local_addr().unwrap();
    let (stop, stopped) = std::sync::mpsc::channel();
    let server_thread = std::thread::spawn(move || {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) && stopped.try_recv().is_err() {
            match server.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                    let mut request = [0u8; 8192];
                    let _ = stream.read(&mut request);
                    let _ = stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(_) => break,
            }
        }
    });
    let repo = fixture.repo();
    repo.remote("origin", &format!("http://{address}/fixture.git"))
        .unwrap();
    let mut config = repo.config().unwrap();
    config.set_multivar("credential.helper", "^never-match$", "!f() { echo called > credential-helper-ran; printf 'username=fixture\\npassword=fixture-secret\\n'; }; f").unwrap();
    let result = fixture.system().remote(
        &fixture.root,
        &RemoteRequest {
            action: RemoteAction::Fetch,
            expected_branch: None,
            expected_head: None,
        },
        &RemoteControl::default(),
    );
    let _ = stop.send(());
    server_thread.join().unwrap();
    let error = result.unwrap_err();
    assert!(
        fixture.root.join("credential-helper-ran").exists(),
        "credential helper was not delegated to Git"
    );
    assert_eq!(error.kind, RemoteErrorKind::AuthRequired);
    assert!(!error.message.contains("fixture-secret"));
    assert!(!error.message.contains(&address.to_string()));
}

#[test]
fn native_index_edit_refuses_post_index_hook_and_system_git_runs_it() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("new.txt"), "new\n").unwrap();
    fixture.hook("post-index-change", "echo called > index-hook-ran");
    let request = WriteRequest {
        expected_head: None,
        action: WriteAction::StageAll,
    };
    assert_eq!(
        Git2Backend.write(&fixture.root, &request).unwrap_err().kind,
        WriteErrorKind::Unsupported
    );
    assert!(fixture.repo().index().unwrap().is_empty());
    fixture.system().write(&fixture.root, &request).unwrap();
    assert!(fixture.root.join("index-hook-ran").exists());
    assert!(
        fixture
            .repo()
            .index()
            .unwrap()
            .get_path(Path::new("new.txt"), 0)
            .is_some()
    );
}

#[test]
fn system_checkout_preserves_ignored_file_collision() {
    let fixture = Fixture::new();
    let initial = fixture.seed();
    let repo = fixture.repo();
    let mut index = repo.index().unwrap();
    fs::write(fixture.root.join("secret.txt"), "tracked target\n").unwrap();
    index.add_path(Path::new("secret.txt")).unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = Signature::now("Fixture", "fixture@example.invalid").unwrap();
    repo.commit(
        Some("refs/heads/topic"),
        &signature,
        &signature,
        "target",
        &tree,
        &[&repo
            .find_commit(git2::Oid::from_str(&initial).unwrap())
            .unwrap()],
    )
    .unwrap();
    index.remove_path(Path::new("secret.txt")).unwrap();
    index.write().unwrap();
    fs::write(
        repo.path().join("info/exclude"),
        ".fixture-hooks/\nsecret.txt\n",
    )
    .unwrap();
    fs::write(fixture.root.join("secret.txt"), "private ignored bytes\n").unwrap();
    let request = CheckoutRequest {
        branch: "topic".into(),
        remote: false,
        expected_branch: Some("main".into()),
        expected_head: Some(initial.clone()),
    };
    assert!(fixture.system().checkout(&fixture.root, &request).is_err());
    assert_eq!(fixture.head().as_deref(), Some(initial.as_str()));
    assert_eq!(
        fs::read_to_string(fixture.root.join("secret.txt")).unwrap(),
        "private ignored bytes\n"
    );
}

#[test]
fn system_branch_rename_and_delete_retain_recovery_references() {
    let fixture = Fixture::new();
    let initial = fixture.seed();
    let repo = fixture.repo();
    repo.branch(
        "topic",
        &repo
            .find_commit(git2::Oid::from_str(&initial).unwrap())
            .unwrap(),
        false,
    )
    .unwrap();
    for action in [
        BranchAction::Rename {
            old: "topic".into(),
            new: "renamed".into(),
        },
        BranchAction::Delete {
            name: "renamed".into(),
            force: true,
        },
    ] {
        let outcome = fixture
            .system()
            .manage_branch(
                &fixture.root,
                &BranchRequest {
                    expected_head: Some(initial.clone()),
                    action,
                },
            )
            .unwrap();
        let reference = outcome.recovery_reference.unwrap();
        assert!(reference.starts_with("refs/gitviews/deleted-branches/"));
        assert_eq!(
            repo.find_reference(&reference)
                .unwrap()
                .target()
                .unwrap()
                .to_string(),
            initial
        );
    }
    assert!(repo.find_branch("topic", git2::BranchType::Local).is_err());
    assert!(
        repo.find_branch("renamed", git2::BranchType::Local)
            .is_err()
    );
}

#[test]
fn system_pull_fast_forwards_and_executes_post_merge_hook() {
    let fixture = Fixture::new();
    let initial = fixture.seed();
    let remote = Repository::init_bare(fixture.directory.join("remote.git")).unwrap();
    remote.set_head("refs/heads/main").unwrap();
    let repo = fixture.repo();
    repo.remote("origin", &display_path(remote.path())).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("branch.main.remote", "origin").unwrap();
    config
        .set_str("branch.main.merge", "refs/heads/main")
        .unwrap();
    fixture
        .system()
        .remote(
            &fixture.root,
            &RemoteRequest {
                action: RemoteAction::Push,
                expected_branch: Some("main".into()),
                expected_head: Some(initial.clone()),
            },
            &RemoteControl::default(),
        )
        .unwrap();
    let other_root = fixture.directory.join("other");
    let other = Repository::clone(&display_path(remote.path()), &other_root).unwrap();
    fs::write(other_root.join("note.txt"), "advanced upstream\n").unwrap();
    let mut index = other.index().unwrap();
    index.add_path(Path::new("note.txt")).unwrap();
    index.write().unwrap();
    let tree = other.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = Signature::now("Fixture", "fixture@example.invalid").unwrap();
    let next = other
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "advance",
            &tree,
            &[&other
                .find_commit(git2::Oid::from_str(&initial).unwrap())
                .unwrap()],
        )
        .unwrap();
    other
        .find_remote("origin")
        .unwrap()
        .push(&["refs/heads/main:refs/heads/main"], None)
        .unwrap();
    fixture.hook("post-merge", "echo called > post-merge-ran");
    fixture
        .system()
        .remote(
            &fixture.root,
            &RemoteRequest {
                action: RemoteAction::Pull,
                expected_branch: Some("main".into()),
                expected_head: Some(initial),
            },
            &RemoteControl::default(),
        )
        .unwrap();
    assert_eq!(fixture.head().unwrap(), next.to_string());
    assert!(fixture.root.join("post-merge-ran").exists());
    assert_eq!(
        fs::read_to_string(fixture.root.join("note.txt")).unwrap(),
        "advanced upstream\n"
    );
}

#[test]
fn system_unstages_both_paths_of_a_rename_without_changing_working_bytes() {
    let fixture = Fixture::new();
    let initial = fixture.seed();
    fs::rename(
        fixture.root.join("note.txt"),
        fixture.root.join("renamed.txt"),
    )
    .unwrap();
    fixture
        .system()
        .write(
            &fixture.root,
            &WriteRequest {
                expected_head: Some(initial.clone()),
                action: WriteAction::StageAll,
            },
        )
        .unwrap();
    fixture
        .system()
        .write(
            &fixture.root,
            &WriteRequest {
                expected_head: Some(initial),
                action: WriteAction::UnstagePaths {
                    paths: vec!["renamed.txt".into()],
                },
            },
        )
        .unwrap();
    let index = fixture.repo().index().unwrap();
    assert!(index.get_path(Path::new("note.txt"), 0).is_some());
    assert!(index.get_path(Path::new("renamed.txt"), 0).is_none());
    assert!(!fixture.root.join("note.txt").exists());
    assert_eq!(
        fs::read_to_string(fixture.root.join("renamed.txt")).unwrap(),
        "fixture content\n"
    );
}

#[test]
fn repository_override_environment_child_process() {
    let Some(root) = std::env::var_os("GITVIEWS_OVERRIDE_TEST_ROOT") else {
        return;
    };
    SystemGit::new(git_executable())
        .write(
            Path::new(&root),
            &WriteRequest {
                expected_head: None,
                action: WriteAction::Commit {
                    summary: "isolated environment fixture".into(),
                    description: String::new(),
                },
            },
        )
        .unwrap();
}

#[test]
fn system_runner_clears_inherited_repository_overrides_without_touching_other_repo() {
    let selected = Fixture::new();
    selected.stage();
    let external = Fixture::new();
    let original_head = external.seed();
    fs::write(external.root.join("note.txt"), "external staged bytes\n").unwrap();
    let external_repo = external.repo();
    let mut index = external_repo.index().unwrap();
    index.add_path(Path::new("note.txt")).unwrap();
    index.write().unwrap();
    let original_index = fs::read(external_repo.path().join("index")).unwrap();
    let shallow = external.directory.join("external-shallow-marker");
    fs::write(&shallow, "external marker\n").unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "repository_override_environment_child_process",
            "--nocapture",
        ])
        .env("GITVIEWS_OVERRIDE_TEST_ROOT", &selected.root)
        .env("GIT_DIR", external_repo.path())
        .env("GIT_WORK_TREE", &external.root)
        .env("GIT_COMMON_DIR", external_repo.path())
        .env("GIT_INDEX_FILE", external_repo.path().join("index"))
        .env("GIT_OBJECT_DIRECTORY", external_repo.path().join("objects"))
        .env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            external_repo.path().join("objects"),
        )
        .env("GIT_NAMESPACE", "external-namespace")
        .env("GIT_SHALLOW_FILE", &shallow)
        .env("GIT_QUARANTINE_PATH", external_repo.path().join("objects"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child fixture failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(selected.head().is_some());
    assert_eq!(external.head().as_deref(), Some(original_head.as_str()));
    assert_eq!(
        fs::read(external_repo.path().join("index")).unwrap(),
        original_index
    );
    assert_eq!(
        fs::read_to_string(external.root.join("note.txt")).unwrap(),
        "external staged bytes\n"
    );
    assert_eq!(fs::read_to_string(shallow).unwrap(), "external marker\n");
}
