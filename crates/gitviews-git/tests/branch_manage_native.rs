//! Isolated native fixtures: no network or user repositories.
#[allow(dead_code)]
#[path = "../src/branch_manage.rs"]
mod branch_manage;

use branch_manage::{BranchAction, BranchErrorKind, BranchRequest};
use git2::{Oid, Repository, Signature, WorktreeAddOptions};
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
    repository: Repository,
    initial: Oid,
    feature: Oid,
}
impl Fixture {
    fn new() -> Self {
        let directory = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "gitviews-branches-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir(&directory).unwrap();
        let root = directory.join("中文 分支仓库");
        let repository = Repository::init(&root).unwrap();
        repository
            .config()
            .unwrap()
            .set_bool("core.autocrlf", false)
            .unwrap();
        let initial = commit(&repository, "initial", None);
        let feature = commit(&repository, "feature", Some(initial));
        repository
            .reference("refs/heads/main", initial, true, "fixture")
            .unwrap();
        repository
            .reference("refs/heads/feature", feature, true, "fixture")
            .unwrap();
        repository
            .reference("refs/heads/merged", initial, true, "fixture")
            .unwrap();
        repository.set_head("refs/heads/main").unwrap();
        repository.checkout_head(None).unwrap();
        Self {
            directory,
            root,
            repository,
            initial,
            feature,
        }
    }
    fn run(
        &self,
        action: BranchAction,
    ) -> Result<branch_manage::BranchOutcome, branch_manage::BranchError> {
        branch_manage::execute(
            &self.root,
            &BranchRequest {
                expected_head: Some(self.initial.to_string()),
                action,
            },
        )
    }
    fn delete(
        &self,
        name: &str,
        force: bool,
    ) -> Result<branch_manage::BranchOutcome, branch_manage::BranchError> {
        self.run(BranchAction::Delete {
            name: name.into(),
            force,
        })
    }
    fn oid(&self, name: &str) -> Oid {
        self.repository
            .refname_to_id(&format!("refs/heads/{name}"))
            .unwrap()
    }
    fn no_locks(&self) {
        assert!(!self.repository.path().join("HEAD.lock").exists());
        assert!(!self.repository.commondir().join("config.lock").exists());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let resolved = fs::canonicalize(&self.directory).unwrap();
        assert_eq!(
            resolved.parent(),
            Some(fs::canonicalize(std::env::temp_dir()).unwrap().as_path())
        );
        assert!(
            resolved
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("gitviews-branches-")
        );
        // Windows sandboxes can deny removal of Git's read-only object files.
        // Cleanup must not obscure an assertion failure by panicking in Drop.
        let _ = fs::remove_dir_all(resolved);
    }
}
fn commit(repository: &Repository, text: &str, parent: Option<Oid>) -> Oid {
    let blob = repository.blob(text.as_bytes()).unwrap();
    let mut builder = repository.treebuilder(None).unwrap();
    builder.insert("note.txt", blob, 0o100644).unwrap();
    let tree = repository.find_tree(builder.write().unwrap()).unwrap();
    let signature = Signature::now("Fixture", "fixture@example.invalid").unwrap();
    let parent = parent.map(|oid| repository.find_commit(oid).unwrap());
    repository
        .commit(
            None,
            &signature,
            &signature,
            text,
            &tree,
            &parent.iter().collect::<Vec<_>>(),
        )
        .unwrap()
}

#[test]
fn create_from_head_or_explicit_commit_never_switches_or_edits_files() {
    let f = Fixture::new();
    fs::write(f.root.join("note.txt"), "unstaged").unwrap();
    let before_index = fs::read(f.repository.path().join("index")).unwrap();
    f.run(BranchAction::Create {
        name: "中文/新分支".into(),
        start_point: None,
    })
    .unwrap();
    f.run(BranchAction::Create {
        name: "from-feature".into(),
        start_point: Some("feature".into()),
    })
    .unwrap();
    assert_eq!(f.oid("中文/新分支"), f.initial);
    assert_eq!(f.oid("from-feature"), f.feature);
    assert_eq!(f.repository.head().unwrap().shorthand().unwrap(), "main");
    assert_eq!(
        fs::read_to_string(f.root.join("note.txt")).unwrap(),
        "unstaged"
    );
    assert_eq!(
        fs::read(f.repository.path().join("index")).unwrap(),
        before_index
    );
    assert!(
        !f.repository
            .reflog("refs/heads/from-feature")
            .unwrap()
            .is_empty()
    );
    f.no_locks();
}

#[test]
fn rejects_duplicate_invalid_names_and_stale_head_without_modification() {
    let f = Fixture::new();
    assert_eq!(
        f.run(BranchAction::Create {
            name: "feature".into(),
            start_point: None
        })
        .unwrap_err()
        .kind,
        BranchErrorKind::AlreadyExists
    );
    for name in [
        "",
        "refs/heads/oops",
        "-option",
        "HEAD",
        "a..b",
        " space",
        "x\0z",
        "bad.lock",
    ] {
        assert_eq!(
            f.run(BranchAction::Create {
                name: name.into(),
                start_point: None
            })
            .unwrap_err()
            .kind,
            BranchErrorKind::InvalidRequest,
            "{name:?}"
        );
    }
    assert_eq!(
        branch_manage::execute(
            &f.root,
            &BranchRequest {
                expected_head: Some(f.feature.to_string()),
                action: BranchAction::Delete {
                    name: "feature".into(),
                    force: true
                }
            }
        )
        .unwrap_err()
        .kind,
        BranchErrorKind::ConcurrentChange
    );
    assert_eq!(f.oid("feature"), f.feature);
    f.no_locks();
}

#[test]
fn normal_delete_protects_unmerged_and_force_keeps_tip_and_reflog() {
    let f = Fixture::new();
    assert_eq!(
        f.delete("feature", false).unwrap_err().kind,
        BranchErrorKind::NotMerged
    );
    f.repository
        .reference_ensure_log("refs/heads/feature")
        .unwrap();
    let mut log = f.repository.reflog("refs/heads/feature").unwrap();
    log.append(
        f.feature,
        &Signature::now("Fixture", "fixture@example.invalid").unwrap(),
        Some("feature important history"),
    )
    .unwrap();
    log.write().unwrap();
    let outcome = f.delete("feature", true).unwrap();
    let recovery = outcome.recovery_reference.unwrap();
    assert!(f.repository.find_reference("refs/heads/feature").is_err());
    assert_eq!(f.repository.refname_to_id(&recovery).unwrap(), f.feature);
    let retained = f.repository.reflog(&recovery).unwrap();
    assert!(
        retained
            .iter()
            .any(|e| e.message().unwrap() == Some("feature important history"))
    );
    assert!(retained.iter().any(|e| {
        e.message()
            .unwrap()
            .is_some_and(|m| m.contains("refs/heads/feature"))
    }));
    f.no_locks();
}

#[test]
fn merged_delete_succeeds_and_cleans_only_its_own_configuration() {
    let f = Fixture::new();
    let mut config = f.repository.config().unwrap();
    config.set_str("branch.merged.remote", ".").unwrap();
    config
        .set_str("branch.merged.merge", "refs/heads/main")
        .unwrap();
    config
        .set_str("branch.merged.child.remote", "origin")
        .unwrap();
    f.delete("merged", false).unwrap();
    let config = f.repository.config().unwrap();
    assert!(config.get_string("branch.merged.remote").is_err());
    assert_eq!(
        config.get_string("branch.merged.child.remote").unwrap(),
        "origin"
    );
    f.no_locks();
}

#[test]
fn current_or_linked_worktree_branch_cannot_be_deleted_or_renamed_even_forced() {
    let f = Fixture::new();
    assert_eq!(
        f.delete("main", true).unwrap_err().kind,
        BranchErrorKind::BranchInUse
    );
    assert_eq!(
        f.run(BranchAction::Rename {
            old: "main".into(),
            new: "main2".into()
        })
        .unwrap_err()
        .kind,
        BranchErrorKind::BranchInUse
    );
    let branch = f.repository.find_reference("refs/heads/feature").unwrap();
    let mut options = WorktreeAddOptions::new();
    options.reference(Some(&branch));
    f.repository
        .worktree("other", &f.directory.join("other"), Some(&options))
        .unwrap();
    assert_eq!(
        f.delete("feature", true).unwrap_err().kind,
        BranchErrorKind::BranchInUse
    );
    assert_eq!(
        f.run(BranchAction::Rename {
            old: "feature".into(),
            new: "renamed".into()
        })
        .unwrap_err()
        .kind,
        BranchErrorKind::BranchInUse
    );
    f.no_locks();
}

#[test]
fn offline_registered_worktree_still_reserves_branch() {
    let f = Fixture::new();
    let branch = f.repository.find_reference("refs/heads/feature").unwrap();
    let mut options = WorktreeAddOptions::new();
    options.reference(Some(&branch));
    let target = f.directory.join("other");
    f.repository
        .worktree("other", &target, Some(&options))
        .unwrap();
    let resolved = fs::canonicalize(&target).unwrap();
    assert_eq!(
        resolved.parent(),
        Some(fs::canonicalize(&f.directory).unwrap().as_path())
    );
    fs::remove_dir_all(resolved).unwrap();
    assert_eq!(
        f.delete("feature", true).unwrap_err().kind,
        BranchErrorKind::BranchInUse
    );
    f.no_locks();
}

#[test]
fn rename_preserves_tracking_multivalue_config_and_original_reflog() {
    let f = Fixture::new();
    let mut config = f.repository.config().unwrap();
    config.set_str("branch.feature.remote", ".").unwrap();
    config
        .set_str("branch.feature.merge", "refs/heads/main")
        .unwrap();
    config
        .set_str("branch.feature.description", "中文说明")
        .unwrap();
    config
        .set_str("branch.feature.merge", "refs/heads/main")
        .unwrap();
    config
        .set_multivar("branch.feature.merge", "a^", "refs/heads/merged")
        .unwrap();
    f.repository
        .reference_ensure_log("refs/heads/feature")
        .unwrap();
    let mut log = f.repository.reflog("refs/heads/feature").unwrap();
    log.append(
        f.feature,
        &Signature::now("Fixture", "fixture@example.invalid").unwrap(),
        Some("retained original log"),
    )
    .unwrap();
    log.write().unwrap();
    let outcome = f
        .run(BranchAction::Rename {
            old: "feature".into(),
            new: "renamed/中文".into(),
        })
        .unwrap();
    assert_eq!(f.oid("renamed/中文"), f.feature);
    assert!(f.repository.find_reference("refs/heads/feature").is_err());
    let config = f.repository.config().unwrap();
    assert_eq!(
        config.get_string("branch.renamed/中文.remote").unwrap(),
        "."
    );
    assert_eq!(
        config
            .get_string("branch.renamed/中文.description")
            .unwrap(),
        "中文说明"
    );
    assert!(config.get_string("branch.feature.description").is_err());
    let mut values = config.multivar("branch.renamed/中文.merge", None).unwrap();
    let mut count = 0;
    while let Some(e) = values.next() {
        e.unwrap();
        count += 1;
    }
    assert_eq!(count, 2);
    assert!(
        f.repository
            .reflog("refs/heads/renamed/中文")
            .unwrap()
            .iter()
            .any(|e| e.message().unwrap() == Some("retained original log"))
    );
    assert_eq!(
        f.repository
            .refname_to_id(&outcome.recovery_reference.unwrap())
            .unwrap(),
        f.feature
    );
    f.no_locks();
}

#[test]
fn set_change_and_unset_upstream_respects_custom_remote_fetch_mapping() {
    let f = Fixture::new();
    f.repository
        .remote("upstream", "https://example.invalid/unused")
        .unwrap();
    f.repository
        .config()
        .unwrap()
        .set_str(
            "remote.upstream.fetch",
            "+refs/heads/*:refs/remotes/company/*",
        )
        .unwrap();
    f.repository
        .reference(
            "refs/remotes/company/server-branch",
            f.initial,
            false,
            "fixture",
        )
        .unwrap();
    f.run(BranchAction::SetUpstream {
        name: "feature".into(),
        upstream: Some("company/server-branch".into()),
    })
    .unwrap();
    let config = f.repository.config().unwrap();
    assert_eq!(
        config.get_string("branch.feature.remote").unwrap(),
        "upstream"
    );
    assert_eq!(
        config.get_string("branch.feature.merge").unwrap(),
        "refs/heads/server-branch"
    );
    f.run(BranchAction::SetUpstream {
        name: "feature".into(),
        upstream: Some("main".into()),
    })
    .unwrap();
    assert_eq!(
        f.repository
            .find_branch("feature", git2::BranchType::Local)
            .unwrap()
            .upstream()
            .unwrap()
            .name()
            .unwrap(),
        Some("main")
    );
    f.run(BranchAction::SetUpstream {
        name: "feature".into(),
        upstream: None,
    })
    .unwrap();
    assert!(
        f.repository
            .find_branch("feature", git2::BranchType::Local)
            .unwrap()
            .upstream()
            .is_err()
    );
    f.no_locks();
}

#[test]
fn ref_head_and_config_locks_block_without_partial_edits() {
    let f = Fixture::new();
    for lock in [
        f.repository.path().join("HEAD.lock"),
        f.repository.commondir().join("config.lock"),
        f.repository.commondir().join("refs/heads/feature.lock"),
    ] {
        fs::write(&lock, "owned by other Git").unwrap();
        let result = f.run(BranchAction::Rename {
            old: "feature".into(),
            new: "renamed".into(),
        });
        assert_eq!(result.unwrap_err().kind, BranchErrorKind::ConcurrentChange);
        assert_eq!(fs::read_to_string(&lock).unwrap(), "owned by other Git");
        assert_eq!(f.oid("feature"), f.feature);
        assert!(f.repository.find_reference("refs/heads/renamed").is_err());
        fs::remove_file(lock).unwrap();
    }
    f.no_locks();
}

#[test]
fn include_configuration_is_not_modified_by_branch_rename_or_upstream_change() {
    let f = Fixture::new();
    let included = f.directory.join("included.config");
    fs::write(
        &included,
        "[branch \"feature\"]\nremote = .\nmerge = refs/heads/main\n",
    )
    .unwrap();
    f.repository
        .config()
        .unwrap()
        .set_str("include.path", included.to_str().unwrap())
        .unwrap();
    let before = fs::read(&included).unwrap();
    for action in [
        BranchAction::Rename {
            old: "feature".into(),
            new: "renamed".into(),
        },
        BranchAction::SetUpstream {
            name: "feature".into(),
            upstream: None,
        },
    ] {
        assert_eq!(
            f.run(action).unwrap_err().kind,
            BranchErrorKind::Configuration
        );
        assert_eq!(fs::read(&included).unwrap(), before);
        assert_eq!(f.oid("feature"), f.feature);
    }
    f.no_locks();
}

#[test]
fn unborn_creation_requires_explicit_existing_start_and_preserves_unborn_head() {
    let f = Fixture::new();
    f.repository.set_head("refs/heads/unborn").unwrap();
    let request = |start_point| BranchRequest {
        expected_head: None,
        action: BranchAction::Create {
            name: "created".into(),
            start_point,
        },
    };
    assert_eq!(
        branch_manage::execute(&f.root, &request(None))
            .unwrap_err()
            .kind,
        BranchErrorKind::InvalidRequest
    );
    branch_manage::execute(&f.root, &request(Some(f.initial.to_string()))).unwrap();
    assert_eq!(f.oid("created"), f.initial);
    assert_eq!(
        f.repository
            .find_reference("HEAD")
            .unwrap()
            .symbolic_target()
            .unwrap(),
        Some("refs/heads/unborn")
    );
    assert_eq!(
        branch_manage::execute(
            &f.root,
            &BranchRequest {
                expected_head: None,
                action: BranchAction::Create {
                    name: "unborn".into(),
                    start_point: Some(f.initial.to_string())
                }
            }
        )
        .unwrap_err()
        .kind,
        BranchErrorKind::BranchInUse
    );
    f.no_locks();
}

#[test]
fn refuses_nested_paths_and_bare_repositories() {
    let f = Fixture::new();
    let nested = f.root.join("folder");
    fs::create_dir(&nested).unwrap();
    let request = BranchRequest {
        expected_head: Some(f.initial.to_string()),
        action: BranchAction::Create {
            name: "bad".into(),
            start_point: None,
        },
    };
    assert_eq!(
        branch_manage::execute(&nested, &request).unwrap_err().kind,
        BranchErrorKind::InvalidRepository
    );
    let bare = f.directory.join("bare");
    Repository::init_bare(&bare).unwrap();
    assert_eq!(
        branch_manage::execute(&bare, &request).unwrap_err().kind,
        BranchErrorKind::InvalidRepository
    );
}

#[test]
fn interrupted_multi_ref_rename_restores_original_refs() {
    let f = Fixture::new();
    f.repository
        .reference("refs/heads/renamed", f.feature, false, "partial rename")
        .unwrap();
    f.repository
        .find_reference("refs/heads/feature")
        .unwrap()
        .delete()
        .unwrap();
    branch_manage::rollback_for_test(
        &f.root,
        &[
            ("refs/heads/feature".into(), Some(f.feature), None),
            ("refs/heads/renamed".into(), None, Some(f.feature)),
        ],
    )
    .unwrap();
    assert_eq!(f.oid("feature"), f.feature);
    assert!(f.repository.find_reference("refs/heads/renamed").is_err());
}

#[test]
fn rollback_never_overwrites_an_external_ref_update() {
    let f = Fixture::new();
    f.repository
        .reference(
            "refs/heads/renamed",
            f.initial,
            false,
            "external writer changed new ref",
        )
        .unwrap();
    assert_eq!(
        branch_manage::rollback_for_test(
            &f.root,
            &[
                ("refs/heads/feature".into(), Some(f.feature), None),
                ("refs/heads/renamed".into(), None, Some(f.feature)),
            ]
        )
        .unwrap_err()
        .kind,
        BranchErrorKind::ConcurrentChange
    );
    assert_eq!(f.oid("feature"), f.feature);
    assert_eq!(f.oid("renamed"), f.initial);
}

#[test]
fn merge_state_refuses_branch_mutation() {
    let f = Fixture::new();
    fs::write(
        f.repository.path().join("MERGE_HEAD"),
        format!("{}\n", f.feature),
    )
    .unwrap();
    assert_eq!(
        f.delete("feature", true).unwrap_err().kind,
        BranchErrorKind::InProgress
    );
    assert_eq!(f.oid("feature"), f.feature);
    f.no_locks();
}

#[test]
fn detached_worktree_rebase_and_bisect_keep_original_branch_reserved() {
    let f = Fixture::new();
    let branch = f.repository.find_reference("refs/heads/feature").unwrap();
    let mut options = WorktreeAddOptions::new();
    options.reference(Some(&branch));
    let worktree = f
        .repository
        .worktree("other", &f.directory.join("other"), Some(&options))
        .unwrap();
    let other = Repository::open_from_worktree(&worktree).unwrap();
    other.set_head_detached(f.feature).unwrap();
    let rebase = other.path().join("rebase-merge");
    fs::create_dir(&rebase).unwrap();
    fs::write(rebase.join("head-name"), "refs/heads/feature\n").unwrap();
    assert_eq!(
        f.delete("feature", true).unwrap_err().kind,
        BranchErrorKind::BranchInUse
    );
    fs::remove_file(rebase.join("head-name")).unwrap();
    fs::remove_dir(rebase).unwrap();
    fs::write(other.path().join("BISECT_START"), "feature\n").unwrap();
    assert_eq!(
        f.run(BranchAction::Rename {
            old: "feature".into(),
            new: "renamed".into()
        })
        .unwrap_err()
        .kind,
        BranchErrorKind::BranchInUse
    );
    f.no_locks();
}
