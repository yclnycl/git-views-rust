//! Native libgit2 implementation; no Git executable is invoked.

use std::{
    collections::{BTreeMap, BTreeSet, BinaryHeap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use git2::{
    BranchType, Delta, DiffFindOptions, DiffOptions, ErrorCode, Index, Oid, Patch, Repository,
    Status, StatusOptions, SubmoduleIgnore, Tree, TreeWalkMode, TreeWalkResult,
};

use crate::{
    BranchInfo, CommitDetails, CommitSummary, FileChange, FileDiff, FileTreeEntry, FileTreeKind,
    GitBackend, GitError, Head, HistoryPage, RepositoryDetails, RepositorySnapshot, RepositoryTree,
    SourceFile, SubmoduleInfo, TreeWarning, WorktreeInfo, diff,
};

/// Thread-safe backend that opens a fresh libgit2 repository for each read.
#[derive(Clone, Copy, Debug, Default)]
pub struct Git2Backend;

const MAX_TREE_ENTRIES: usize = 100_000;

impl GitBackend for Git2Backend {
    fn setup_repository(
        &self,
        request: &crate::RepositorySetupRequest,
        control: &crate::RemoteControl,
    ) -> Result<crate::RepositorySetupOutcome, crate::RemoteError> {
        crate::repository_setup::execute(request, control)
    }
    fn graph_history(
        &self,
        root: &Path,
        request: &crate::GraphHistoryRequest,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::GraphHistoryPage, crate::HistoryToolsError> {
        crate::graph_history::read(root, request, cancel)
    }

    fn search_tree_with_cancel(
        &self,
        path: &Path,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<RepositoryTree, GitError> {
        crate::search_tree::read(path, cancel)
    }
    fn conflict_state(&self, root: &Path) -> Result<crate::ConflictState, crate::ConflictError> {
        crate::conflicts::state(root)
    }
    fn conflict_versions(
        &self,
        root: &Path,
        path: &str,
    ) -> Result<crate::ConflictVersions, crate::ConflictError> {
        crate::conflicts::versions(root, path)
    }
    fn conflict(
        &self,
        root: &Path,
        request: &crate::ConflictRequest,
    ) -> Result<crate::ConflictOutcome, crate::ConflictError> {
        if !matches!(request.action, crate::ConflictAction::Resolve { .. }) {
            crate::compatibility::native_workflow_guard(
                root,
                crate::compatibility::MERGE_HOOKS,
                !matches!(request.action, crate::ConflictAction::Abort),
            )
            .map_err(|message| {
                crate::ConflictError::new(crate::ConflictErrorKind::Unsupported, message)
            })?;
        } else {
            crate::compatibility::native_workflow_guard(
                root,
                crate::compatibility::INDEX_HOOKS,
                false,
            )
            .map_err(|message| {
                crate::ConflictError::new(crate::ConflictErrorKind::Unsupported, message)
            })?;
        }
        crate::conflicts::execute(root, request)
    }
    fn history_tools(
        &self,
        root: &Path,
        request: &crate::HistoryToolsRequest,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::HistoryToolsResult, crate::HistoryToolsError> {
        if request.is_mutation() {
            crate::compatibility::native_workflow_guard(
                root,
                crate::compatibility::REFERENCE_HOOKS,
                false,
            )
            .map_err(|message| {
                crate::HistoryToolsError::new(crate::HistoryToolsErrorKind::Unsupported, message)
            })?;
        }
        crate::history_tools::execute(root, request, cancel)
    }
    fn stashes(&self, root: &Path) -> Result<Vec<crate::StashEntry>, crate::StashError> {
        crate::stash::list(root)
    }

    fn stash(
        &self,
        root: &Path,
        request: &crate::StashRequest,
    ) -> Result<crate::StashOutcome, crate::StashError> {
        crate::compatibility::native_workflow_guard(
            root,
            &["reference-transaction", "post-index-change"],
            false,
        )
        .map_err(|message| crate::StashError::new(crate::StashErrorKind::Unsupported, message))?;
        crate::stash::execute(root, request)
    }
    fn write(
        &self,
        root: &Path,
        request: &crate::WriteRequest,
    ) -> Result<crate::WriteOutcome, crate::WriteError> {
        if matches!(request.action, crate::WriteAction::Commit { .. }) {
            crate::compatibility::native_workflow_guard(
                root,
                crate::compatibility::COMMIT_HOOKS,
                true,
            )
            .map_err(|message| {
                crate::WriteError::new(crate::WriteErrorKind::Unsupported, message)
            })?;
        } else {
            crate::compatibility::native_workflow_guard(
                root,
                crate::compatibility::INDEX_HOOKS,
                false,
            )
            .map_err(|message| {
                crate::WriteError::new(crate::WriteErrorKind::Unsupported, message)
            })?;
        }
        crate::write_ops::execute(root, request)
    }

    fn manage_branch(
        &self,
        root: &Path,
        request: &crate::BranchRequest,
    ) -> Result<crate::BranchOutcome, crate::BranchError> {
        crate::compatibility::native_workflow_guard(
            root,
            crate::compatibility::REFERENCE_HOOKS,
            false,
        )
        .map_err(|message| crate::BranchError::new(crate::BranchErrorKind::Unsupported, message))?;
        crate::branch_manage::execute(root, request)
    }
    fn git_settings(&self, root: &Path) -> Result<crate::GitSettings, GitError> {
        crate::git_settings::read(root)
    }

    fn save_git_settings(
        &self,
        root: &Path,
        settings: &crate::GitSettings,
    ) -> Result<(), GitError> {
        crate::git_settings::save(root, settings)
    }

    fn worktree(
        &self,
        root: &Path,
        request: &crate::WorktreeRequest,
    ) -> Result<crate::WorktreeOutcome, crate::WorktreeError> {
        crate::compatibility::native_workflow_guard(
            root,
            crate::compatibility::CHECKOUT_HOOKS,
            false,
        )
        .map_err(|message| {
            crate::WorktreeError::new(crate::WorktreeErrorKind::Unsupported, message)
        })?;
        crate::worktrees::execute(root, request)
    }
    #[tracing::instrument(level = "debug", skip(self))]
    fn repository_root(&self, path: &Path) -> Result<PathBuf, GitError> {
        let started = Instant::now();
        let repo = open(path)?;
        let result = root(&repo);
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            success = result.is_ok(),
            "Resolved repository root without scanning"
        );
        result
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn registered_submodule(
        &self,
        parent: &Path,
        relative: &str,
    ) -> Result<Option<SubmoduleInfo>, GitError> {
        let started = Instant::now();
        let repo = open(parent)?;
        let result = submodule_registration(&repo, &root(&repo)?, relative);
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            success = result.is_ok(),
            "Read selected submodule registration without scanning parent files"
        );
        result
    }

    fn checkout(
        &self,
        root: &Path,
        request: &crate::CheckoutRequest,
    ) -> Result<crate::CheckoutOutcome, crate::CheckoutError> {
        crate::compatibility::native_workflow_guard(
            root,
            crate::compatibility::CHECKOUT_HOOKS,
            false,
        )
        .map_err(|message| {
            crate::CheckoutError::new(crate::CheckoutErrorKind::CheckoutFailed, message)
        })?;
        crate::checkout::execute(root, request)
    }

    fn remote(
        &self,
        root: &Path,
        request: &crate::RemoteRequest,
        control: &crate::RemoteControl,
    ) -> Result<crate::RemoteOutcome, crate::RemoteError> {
        let hooks = match request.action {
            crate::RemoteAction::Push => &["pre-push", "reference-transaction"][..],
            crate::RemoteAction::Pull => {
                &["post-merge", "reference-transaction", "post-index-change"][..]
            }
            crate::RemoteAction::Fetch => crate::compatibility::REFERENCE_HOOKS,
        };
        crate::compatibility::native_workflow_guard(root, hooks, false).map_err(|message| {
            crate::RemoteError::new(crate::RemoteErrorKind::Unsupported, message)
        })?;
        crate::remote::execute(root, request, control)
    }

    fn source(
        &self,
        path: &Path,
        file: &str,
        revision: Option<&str>,
    ) -> Result<SourceFile, GitError> {
        crate::source::read(path, file, revision)
    }
    fn history(&self, path: &Path, skip: usize, limit: usize) -> Result<HistoryPage, GitError> {
        crate::history::page(path, skip, limit)
    }
    fn history_at(
        &self,
        path: &Path,
        anchor: &str,
        skip: usize,
        limit: usize,
    ) -> Result<HistoryPage, GitError> {
        crate::history::page_at(path, anchor, skip, limit)
    }
    fn commit(&self, path: &Path, id: &str) -> Result<CommitDetails, GitError> {
        crate::history::details(path, id)
    }
    fn commit_diff(&self, path: &Path, id: &str, file: &str) -> Result<FileDiff, GitError> {
        crate::history::file_diff(path, id, file)
    }
    #[tracing::instrument(level = "debug", skip(self))]
    fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
        let started = Instant::now();
        let repo = open(path)?;
        let mut snapshot = read_metadata(&repo)?;
        snapshot.recent_commits = read_recent_commits(&repo, &snapshot.head)?;
        let history_ms = started.elapsed().as_millis() as u64;
        let status_started = Instant::now();
        snapshot.changes = changes(&repo)?;
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            history_ms,
            status_ms = status_started.elapsed().as_millis() as u64,
            changes = snapshot.changes.len(),
            "Repository overview ready"
        );
        Ok(snapshot)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    fn inspect_metadata(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
        let started = Instant::now();
        let snapshot = read_metadata(&open(path)?)?;
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Repository metadata ready without working-tree scan"
        );
        Ok(snapshot)
    }

    fn details(&self, path: &Path) -> Result<RepositoryDetails, GitError> {
        let repo = open(path)?;
        let main = Repository::open(repo.commondir()).map_err(native("open main worktree"))?;
        let mut worktrees = vec![worktree_info(&main, true)?];
        for name in repo.worktrees().map_err(native("list worktrees"))?.iter() {
            let Some(name) = name.map_err(native("read worktree name"))? else {
                continue;
            };
            let worktree = repo
                .find_worktree(name)
                .map_err(native("read worktree registration"))?;
            // Keep a moved/missing registration visible so it can be repaired
            // or pruned. Never let one unavailable working directory hide all
            // healthy worktrees and branches.
            let mut info = Repository::open_from_worktree(&worktree)
                .ok()
                .and_then(|linked| worktree_info(&linked, false).ok())
                .unwrap_or_else(|| WorktreeInfo {
                    path: worktree.path().to_owned(),
                    ..WorktreeInfo::default()
                });
            // The lock is a sentinel; its arbitrary reason bytes are not needed
            // for display. git2's reason decoding assumes UTF-8 and can panic.
            let lock_path = repo.commondir().join("worktrees").join(name).join("locked");
            info.locked = match fs::symlink_metadata(&lock_path) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(source) => {
                    return Err(GitError::Io {
                        operation: "read worktree lock metadata",
                        source,
                    });
                }
            };
            info.prunable = worktree
                .is_prunable(None)
                .map_err(native("read worktree state"))?;
            worktrees.push(info);
        }
        let mut branches = Vec::new();
        for branch in repo.branches(None).map_err(native("list branches"))? {
            let (branch, kind) = branch.map_err(native("read branch"))?;
            if branch.get().symbolic_target_bytes().is_some() {
                continue;
            }
            let name = branch
                .name()
                .map_err(native("read branch name"))?
                .unwrap_or_default()
                .to_owned();
            let tip = branch
                .get()
                .peel_to_commit()
                .map_err(native("read branch tip"))?;
            let upstream = if kind == BranchType::Local {
                match branch.upstream() {
                    Ok(upstream) => upstream
                        .name()
                        .map_err(native("read upstream name"))?
                        .map(str::to_owned),
                    Err(error) if error.code() == ErrorCode::NotFound => None,
                    Err(error) => return Err(native("read upstream")(error)),
                }
            } else {
                None
            };
            branches.push(BranchInfo {
                name,
                commit_id: tip.id().to_string(),
                subject: String::from_utf8_lossy(tip.summary_bytes().unwrap_or_default())
                    .into_owned(),
                is_current: branch.is_head(),
                upstream,
                is_remote: kind == BranchType::Remote,
            });
        }
        if let Head::Unborn(name) = read_head(&repo)? {
            branches.push(BranchInfo {
                name,
                commit_id: String::new(),
                subject: String::new(),
                is_current: true,
                upstream: None,
                is_remote: false,
            });
        }
        branches.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(RepositoryDetails {
            worktrees,
            branches,
        })
    }

    fn diff(&self, path: &Path, file: &str, staged: bool) -> Result<FileDiff, GitError> {
        diff::validate_path(file)?;
        let repo = open(path)?;
        let working = diff::checked_working_path(&root(&repo)?, file)?;
        let index = repo.index().map_err(native("read index"))?;
        if (1..=3).any(|stage| index.get_path(Path::new(file), stage).is_some()) {
            return Err(GitError::Unsupported {
                operation: "diff of an unmerged index",
            });
        }
        let head = head_tree(&repo)?;
        let mut options = DiffOptions::new();
        options
            .context_lines(3)
            .disable_pathspec_match(true)
            .update_index(false)
            .include_typechange(true);
        let mut comparison = if staged {
            repo.diff_tree_to_index(head.as_ref(), Some(&index), Some(&mut options))
        } else {
            options
                .pathspec(file)
                .include_untracked(true)
                .recurse_untracked_dirs(true)
                .show_untracked_content(true);
            repo.diff_index_to_workdir(Some(&index), Some(&mut options))
        }
        .map_err(native("read file diff"))?;
        if staged {
            find_similar_bounded(&repo, &mut comparison)?;
        }
        let selected = comparison.deltas().enumerate().find(|(_, delta)| {
            delta.new_file().path() == Some(Path::new(file))
                || (delta.status() == Delta::Deleted
                    && delta.old_file().path() == Some(Path::new(file)))
        });
        let original = selected
            .as_ref()
            .and_then(|(_, delta)| delta.old_file().path().map(Path::to_owned))
            .unwrap_or_else(|| PathBuf::from(file));
        let old = if staged {
            head.as_ref()
                .map(|tree| tree_blob(&repo, tree, &original, file))
                .transpose()?
                .flatten()
        } else {
            index_blob(&repo, &index, file)?
        };
        let new = if staged {
            index_blob(&repo, &index, file)?
        } else if index
            .get_path(Path::new(file), 0)
            .is_some_and(|entry| entry.mode == 0o160000)
        {
            working_gitlink_text(&repo, &working, file)?
        } else {
            diff::read_working(&working, file)?
        };
        if old.is_none() && new.is_none() {
            return Err(GitError::InvalidPath {
                path: file.to_owned(),
                reason: "the file is absent from this comparison",
            });
        }
        let patch = if let Some((position, _)) = selected {
            render_patch(&comparison, position, file)?
        } else {
            String::new()
        };
        Ok(FileDiff {
            path: file.to_owned(),
            old_text: old.unwrap_or_default(),
            new_text: new.unwrap_or_default(),
            lines: diff::parse_lines(&patch)?,
            patch,
        })
    }

    fn tree(&self, path: &Path) -> Result<RepositoryTree, GitError> {
        let repo = open(path)?;
        let root = root(&repo)?;
        read_tree(&repo, &root, None)
    }

    fn tree_with_changes(
        &self,
        path: &Path,
        changes: &[FileChange],
    ) -> Result<RepositoryTree, GitError> {
        let repo = open(path)?;
        let root = root(&repo)?;
        read_tree(&repo, &root, Some(changes))
    }
}

fn read_metadata(repo: &Repository) -> Result<RepositorySnapshot, GitError> {
    let root = root(repo)?;
    let head = read_head(repo)?;
    Ok(RepositorySnapshot {
        name: root.file_name().map_or_else(
            || crate::display_path(&root),
            |name| name.to_string_lossy().into_owned(),
        ),
        root,
        head,
        changes: Vec::new(),
        recent_commits: Vec::new(),
    })
}

fn read_recent_commits(repo: &Repository, head: &Head) -> Result<Vec<CommitSummary>, GitError> {
    let mut recent_commits = Vec::new();
    if !matches!(head, Head::Unborn(_)) {
        // libgit2's TIME and TOPOLOGICAL revwalk modes both prepare the complete
        // graph before yielding. The overview instead visits a recent ancestry
        // frontier: HEAD first, then the newest available parent. Full history
        // paging retains its existing ordering in the history module.
        let tip = repo
            .head()
            .map_err(native("read recent commits"))?
            .peel_to_commit()
            .map_err(native("read recent commits"))?;
        let mut frontier = BinaryHeap::from([(tip.time().seconds(), tip.id())]);
        let mut seen = HashSet::from([tip.id()]);
        while let Some((_, id)) = frontier.pop() {
            let commit = repo
                .find_commit(id)
                .map_err(native("read recent commits"))?;
            recent_commits.push(CommitSummary {
                id: commit.id().to_string(),
                subject: String::from_utf8_lossy(commit.summary_bytes().unwrap_or_default())
                    .into_owned(),
                author: String::from_utf8_lossy(commit.author().name_bytes()).into_owned(),
            });
            if recent_commits.len() == 20 {
                break;
            }
            for parent in commit.parent_ids() {
                if seen.insert(parent) {
                    let parent = repo
                        .find_commit(parent)
                        .map_err(native("read recent commits"))?;
                    frontier.push((parent.time().seconds(), parent.id()));
                }
            }
        }
    }
    Ok(recent_commits)
}

#[tracing::instrument(level = "debug", skip(repo, known_changes), fields(reused_status = known_changes.is_some()))]
pub(super) fn read_tree(
    repo: &Repository,
    root: &Path,
    known_changes: Option<&[FileChange]>,
) -> Result<RepositoryTree, GitError> {
    let started = Instant::now();
    let index = repo.index().map_err(native("read index"))?;
    let head = head_tree(repo)?;
    let mut entries = BTreeMap::<String, FileTreeEntry>::new();
    let mut warnings = Vec::new();
    let mut links = BTreeMap::<String, (Option<Oid>, Option<Oid>)>::new();
    if let Some(head) = &head {
        let mut too_large = false;
        let result = head.walk(TreeWalkMode::PreOrder, |directory, entry| {
            let Ok(name) = std::str::from_utf8(entry.name_bytes()) else {
                warn(
                    &mut warnings,
                    directory,
                    "Skipped a HEAD entry whose path is not valid UTF-8".into(),
                );
                return TreeWalkResult::Skip;
            };
            let path = format!("{directory}{name}");
            insert_entry(
                &mut entries,
                path.clone(),
                mode_kind(entry.filemode() as u32),
            );
            if entry.filemode() == 0o160000 {
                links.entry(path).or_default().0 = Some(entry.id());
            }
            if entries.len() > MAX_TREE_ENTRIES {
                too_large = true;
                return TreeWalkResult::Abort;
            }
            TreeWalkResult::Ok
        });
        if too_large {
            return Err(tree_limit());
        }
        result.map_err(native("read HEAD file tree"))?;
    }
    for entry in index.iter() {
        let Ok(path) = String::from_utf8(entry.path.clone()) else {
            warn(
                &mut warnings,
                "",
                "Skipped an index entry whose path is not valid UTF-8".into(),
            );
            continue;
        };
        insert_entry(&mut entries, path.clone(), mode_kind(entry.mode));
        if entry.mode == 0o160000 {
            links.entry(path).or_default().1 = Some(entry.id);
        }
        if entries.len() > MAX_TREE_ENTRIES {
            return Err(tree_limit());
        }
    }
    for entry in entries.values_mut() {
        if entry.kind != FileTreeKind::Directory
            && index.get_path(Path::new(&entry.path), 0).is_none()
        {
            entry.index_status = 'D';
        }
    }
    let mut submodules = match read_submodules(repo, root, &links) {
        Ok(modules) => modules,
        Err(error) => {
            warn(&mut warnings, ".gitmodules", error.to_string());
            links
                .iter()
                .map(|(path, (head, index))| SubmoduleInfo {
                    name: path.clone(),
                    path: path.clone(),
                    url: None,
                    head_id: head.map(|id| id.to_string()),
                    index_id: index.map(|id| id.to_string()),
                    workdir_id: None,
                    initialized: false,
                    dirty: head != index,
                })
                .collect()
        }
    };
    for module in &submodules {
        insert_entry(&mut entries, module.path.clone(), FileTreeKind::Submodule);
    }
    tracing::debug!(
        phase = "tracked_and_submodule_metadata",
        elapsed_ms = started.elapsed().as_millis() as u64,
        "File-tree phase completed"
    );
    let owned_changes;
    let changes = if let Some(changes) = known_changes {
        changes
    } else {
        owned_changes = match changes(repo) {
            Ok(changes) => changes,
            Err(error) => {
                warn(&mut warnings, "", error.to_string());
                Vec::new()
            }
        };
        &owned_changes
    };
    // Status already enumerates every unignored untracked file. Reading every
    // directory again duplicates that work and performs costly per-path ignore
    // checks on Windows. HEAD/index supply tracked and deleted entries; only
    // status paths need a working-file metadata check.
    let mut checked_directories = BTreeSet::new();
    let mut changed_statuses = BTreeMap::<String, (char, char)>::new();
    for change in changes {
        add_working_change(
            root,
            change,
            &mut entries,
            &mut checked_directories,
            &mut warnings,
        )?;
        let status = changed_statuses
            .entry(change.path.trim_end_matches('/').to_owned())
            .or_insert((' ', ' '));
        // `git rm --cached` leaves both a staged D and an untracked record
        // at the same path. The single tree node must retain both facts.
        for (stored, incoming) in [
            (&mut status.0, change.index_status),
            (&mut status.1, change.worktree_status),
        ] {
            if incoming != ' ' && (incoming != '?' || *stored == ' ') {
                *stored = incoming;
            }
        }
    }
    for (path, (index_status, worktree_status)) in &changed_statuses {
        if let Some(entry) = entries.get_mut(path) {
            entry.index_status = *index_status;
            entry.worktree_status = *worktree_status;
        }
    }
    if changes.iter().any(|change| change.index_status == 'R') {
        let mut options = DiffOptions::new();
        options.include_typechange(true).update_index(false);
        let mut comparison = repo
            .diff_tree_to_index(head.as_ref(), Some(&index), Some(&mut options))
            .map_err(native("read renamed tree paths"))?;
        find_similar_bounded(repo, &mut comparison)?;
        for delta in comparison
            .deltas()
            .filter(|delta| delta.status() == Delta::Renamed)
        {
            if let Some(path) = delta.old_file().path().and_then(Path::to_str)
                && !changed_statuses.contains_key(path)
                && index.get_path(Path::new(path), 0).is_none()
            {
                entries.remove(path);
            }
        }
        // A directory represented only by the old HEAD paths disappears too.
        let parents: HashSet<String> = entries
            .values()
            .filter(|entry| entry.kind != FileTreeKind::Directory)
            .flat_map(|entry| {
                Path::new(&entry.path)
                    .ancestors()
                    .skip(1)
                    .filter(|path| !path.as_os_str().is_empty())
                    .map(|path| path.to_string_lossy().replace('\\', "/"))
            })
            .collect();
        entries
            .retain(|path, entry| entry.kind != FileTreeKind::Directory || parents.contains(path));
    }
    for module in &mut submodules {
        module.dirty |= changes.iter().any(|change| {
            change.path.trim_end_matches('/') == module.path
                && (change.index_status != ' ' || change.worktree_status != ' ')
        });
        if module.dirty
            && let Some(entry) = entries.get_mut(&module.path)
            && entry.index_status == ' '
            && entry.worktree_status == ' '
        {
            entry.worktree_status = 'M';
        }
    }
    submodules.sort_by(|left, right| left.path.cmp(&right.path));
    tracing::debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        entries = entries.len(),
        submodules = submodules.len(),
        warnings = warnings.len(),
        "File tree ready"
    );
    Ok(RepositoryTree {
        entries: entries.into_values().collect(),
        submodules,
        warnings,
    })
}

pub(super) fn native(operation: &'static str) -> impl FnOnce(git2::Error) -> GitError {
    move |source| GitError::Native { operation, source }
}

pub(super) fn open(path: &Path) -> Result<Repository, GitError> {
    let repo = Repository::discover(crate::native_path(path)).map_err(|source| {
        if source.code() == ErrorCode::NotFound {
            GitError::NotRepository {
                path: path.to_owned(),
            }
        } else {
            native("open repository")(source)
        }
    })?;
    if repo.workdir().is_none() {
        return Err(GitError::NotRepository {
            path: path.to_owned(),
        });
    }
    Ok(repo)
}

pub(super) fn root(repo: &Repository) -> Result<PathBuf, GitError> {
    let workdir = repo.workdir().ok_or(GitError::NotRepository {
        path: repo.path().to_owned(),
    })?;
    fs::canonicalize(crate::native_path(workdir)).map_err(|source| GitError::Io {
        operation: "resolve repository root",
        source,
    })
}

pub(super) fn read_head(repo: &Repository) -> Result<Head, GitError> {
    match repo.head() {
        Ok(reference) => {
            reference
                .peel_to_commit()
                .map_err(|error| head_error(repo, error))?;
            if reference.is_branch() {
                Ok(Head::Branch(
                    String::from_utf8_lossy(reference.shorthand_bytes()).into_owned(),
                ))
            } else {
                Ok(Head::Detached(
                    reference
                        .peel_to_commit()
                        .map_err(native("read detached HEAD"))?
                        .id()
                        .to_string(),
                ))
            }
        }
        Err(error) if error.code() == ErrorCode::UnbornBranch => {
            let reference = repo
                .find_reference("HEAD")
                .map_err(native("read unborn HEAD"))?;
            Ok(Head::Unborn(
                reference
                    .symbolic_target()
                    .map_err(native("read unborn branch name"))?
                    .unwrap_or("refs/heads/HEAD")
                    .trim_start_matches("refs/heads/")
                    .to_owned(),
            ))
        }
        Err(error) => Err(native("read HEAD")(error)),
    }
}

fn head_tree(repo: &Repository) -> Result<Option<Tree<'_>>, GitError> {
    match repo.head() {
        Ok(head) => head
            .peel_to_tree()
            .map(Some)
            .map_err(native("read HEAD tree")),
        Err(error) if error.code() == ErrorCode::UnbornBranch => Ok(None),
        Err(error) => Err(native("read HEAD tree")(error)),
    }
}

#[tracing::instrument(level = "debug", skip(repo))]
fn changes(repo: &Repository) -> Result<Vec<FileChange>, GitError> {
    let started = Instant::now();
    let index = repo.index().map_err(native("read index status"))?;
    let head = head_tree(repo)?;
    let mut comparison_options = DiffOptions::new();
    comparison_options
        .include_typechange(true)
        .update_index(false);
    let mut comparison = repo
        .diff_tree_to_index(head.as_ref(), Some(&index), Some(&mut comparison_options))
        .map_err(native("measure status rename candidates"))?;
    let detect_renames = rename_candidates_bounded(repo, &comparison)?;
    let mut copied_paths = HashSet::new();
    if detect_renames && copies_enabled(repo) {
        find_similar_bounded(repo, &mut comparison)?;
        copied_paths.extend(
            comparison
                .deltas()
                .filter(|delta| delta.status() == Delta::Copied)
                .filter_map(|delta| {
                    delta
                        .new_file()
                        .path()
                        .and_then(Path::to_str)
                        .map(str::to_owned)
                }),
        );
    }
    let rename_ms = started.elapsed().as_millis() as u64;
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .include_unreadable(true)
        .renames_head_to_index(detect_renames)
        .renames_index_to_workdir(false)
        .update_index(false)
        .no_refresh(true);
    let statuses = repo
        .statuses(Some(&mut options))
        .map_err(native("read working tree status"))?;
    let mut result = Vec::new();
    for entry in statuses.iter() {
        let status = entry.status();
        let path = entry
            .index_to_workdir()
            .and_then(|delta| delta.new_file().path().map(Path::to_owned))
            .or_else(|| {
                entry
                    .head_to_index()
                    .and_then(|delta| delta.new_file().path().map(Path::to_owned))
            })
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from_utf8_lossy(entry.path_bytes()).into_owned());
        let intent_to_add = index
            .get_path(Path::new(&path), 0)
            .is_some_and(|entry| entry.flags_extended & 0x2000 != 0);
        let (index_status, worktree_status) = if status.is_conflicted() {
            match (
                index.get_path(Path::new(&path), 1).is_some(),
                index.get_path(Path::new(&path), 2).is_some(),
                index.get_path(Path::new(&path), 3).is_some(),
            ) {
                (_, false, false) => ('D', 'D'),
                (true, true, false) => ('U', 'D'),
                (true, false, true) => ('D', 'U'),
                (false, true, false) => ('A', 'U'),
                (false, false, true) => ('U', 'A'),
                (false, true, true) => ('A', 'A'),
                _ => ('U', 'U'),
            }
        } else if intent_to_add {
            (' ', if status.is_wt_deleted() { 'D' } else { 'A' })
        } else {
            let code = |flags: &[(Status, char)]| {
                flags
                    .iter()
                    .find(|(flag, _)| status.contains(*flag))
                    .map_or(' ', |(_, code)| *code)
            };
            (
                code(&[
                    (
                        Status::INDEX_NEW,
                        if copied_paths.contains(&path) {
                            'C'
                        } else {
                            'A'
                        },
                    ),
                    (Status::INDEX_DELETED, 'D'),
                    (Status::INDEX_RENAMED, 'R'),
                    (Status::INDEX_TYPECHANGE, 'T'),
                    (Status::INDEX_MODIFIED, 'M'),
                ]),
                code(&[
                    (Status::WT_DELETED, 'D'),
                    (Status::WT_RENAMED, 'R'),
                    (Status::WT_TYPECHANGE, 'T'),
                    (Status::WT_MODIFIED, 'M'),
                    (Status::WT_UNREADABLE, '!'),
                ]),
            )
        };
        result.push(FileChange {
            path: path.clone(),
            index_status,
            worktree_status,
        });
        if status.is_wt_new() && !intent_to_add {
            if index_status == ' ' {
                let last = result.last_mut().expect("just pushed status");
                last.index_status = '?';
                last.worktree_status = '?';
            } else {
                result.push(FileChange {
                    path,
                    index_status: '?',
                    worktree_status: '?',
                });
            }
        }
    }
    result.sort_by(|left, right| left.path.cmp(&right.path));
    tracing::debug!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        rename_ms,
        detect_renames,
        changes = result.len(),
        "Repository status ready"
    );
    Ok(result)
}

fn worktree_info(repo: &Repository, is_main: bool) -> Result<WorktreeInfo, GitError> {
    let head = read_head(repo)?;
    let (branch, commit, detached) = match head {
        Head::Branch(branch) => (
            Some(branch),
            repo.head()
                .ok()
                .and_then(|head| head.target())
                .map(|id| id.to_string()),
            false,
        ),
        Head::Unborn(branch) => (Some(branch), None, false),
        Head::Detached(id) => (None, Some(id), true),
    };
    Ok(WorktreeInfo {
        path: fs::canonicalize(crate::native_path(repo.workdir().unwrap_or(repo.path()))).map_err(
            |source| GitError::Io {
                operation: "resolve worktree root",
                source,
            },
        )?,
        branch,
        head: commit,
        is_main,
        is_bare: repo.is_bare(),
        is_detached: detached,
        locked: false,
        prunable: false,
    })
}

fn index_blob(repo: &Repository, index: &Index, file: &str) -> Result<Option<String>, GitError> {
    index
        .get_path(Path::new(file), 0)
        .filter(|entry| entry.flags_extended & 0x2000 == 0)
        .map(|entry| object_text(repo, entry.id, entry.mode, file))
        .transpose()
}

fn working_gitlink_text(
    repo: &Repository,
    working: &Path,
    file: &str,
) -> Result<Option<String>, GitError> {
    let child = match Repository::open(working) {
        Ok(child) if child.workdir().is_some() => child,
        _ => return Ok(None),
    };
    if !crate::paths_equal(&root(&child)?, working) {
        return Ok(None);
    }
    let Some(id) = child.head().ok().and_then(|head| head.target()) else {
        return Ok(None);
    };
    let dirty = if let Ok(module) = repo.find_submodule(file) {
        repo.submodule_status(module.name().unwrap_or(file), SubmoduleIgnore::None)
            .map_err(native("read selected submodule changes"))?
            .intersects(
                git2::SubmoduleStatus::WD_INDEX_MODIFIED
                    | git2::SubmoduleStatus::WD_WD_MODIFIED
                    | git2::SubmoduleStatus::WD_UNTRACKED,
            )
    } else {
        !changes(&child)?.is_empty()
    };
    Ok(Some(format!(
        "Subproject commit {id}{}\n",
        if dirty { "-dirty" } else { "" }
    )))
}

fn tree_blob(
    repo: &Repository,
    tree: &Tree<'_>,
    path: &Path,
    file: &str,
) -> Result<Option<String>, GitError> {
    match tree.get_path(path) {
        Ok(entry) => object_text(repo, entry.id(), entry.filemode() as u32, file).map(Some),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(None),
        Err(error) => Err(native("read HEAD file")(error)),
    }
}

pub(super) fn object_text(
    repo: &Repository,
    id: Oid,
    mode: u32,
    file: &str,
) -> Result<String, GitError> {
    if mode == 0o160000 {
        return Ok(format!("Subproject commit {id}\n"));
    }
    let (size, _) = repo
        .odb()
        .map_err(native("read object database"))?
        .read_header(id)
        .map_err(native("measure diff content"))?;
    diff::check_size(size as u64, file)?;
    let blob = repo.find_blob(id).map_err(native("read diff content"))?;
    diff::text(blob.content().to_vec(), file)
}

/// Similarity detection loads candidate blobs before normal preview reads.
/// Refuse that extra work for large candidates, preserving their honest A/D
/// changes instead of letting an unrelated large rename block a small preview.
pub(super) fn find_similar_bounded(
    repo: &Repository,
    comparison: &mut git2::Diff<'_>,
) -> Result<(), GitError> {
    if !rename_candidates_bounded(repo, comparison)? {
        return Ok(());
    }
    comparison
        .find_similar(Some(
            DiffFindOptions::new()
                .renames(true)
                .copies(copies_enabled(repo))
                .rename_limit(200),
        ))
        .map_err(native("detect renames"))
}

fn copies_enabled(repo: &Repository) -> bool {
    repo.config()
        .ok()
        .and_then(|config| config.get_string("status.renames").ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("copies"))
}

fn rename_candidates_bounded(
    repo: &Repository,
    comparison: &git2::Diff<'_>,
) -> Result<bool, GitError> {
    let mut additions = false;
    let mut deletions = false;
    let copies = copies_enabled(repo);
    for delta in comparison.deltas() {
        additions |= matches!(delta.status(), Delta::Added | Delta::Typechange);
        deletions |= matches!(delta.status(), Delta::Deleted | Delta::Typechange)
            || copies && delta.status() == Delta::Modified;
    }
    if !additions || !deletions {
        return Ok(false);
    }
    let odb = repo.odb().map_err(native("measure rename candidates"))?;
    for delta in comparison.deltas().filter(|delta| {
        matches!(
            delta.status(),
            Delta::Added | Delta::Deleted | Delta::Typechange
        ) || copies && delta.status() == Delta::Modified
    }) {
        for file in [delta.old_file(), delta.new_file()] {
            let mode: u32 = file.mode().into();
            if file.id().is_zero() || !matches!(mode & 0o170000, 0o100000 | 0o120000) {
                continue;
            }
            let (bytes, _) = odb
                .read_header(file.id())
                .map_err(native("measure rename candidate"))?;
            if bytes > diff::MAX_TEXT_BYTES {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn head_error(repo: &Repository, error: git2::Error) -> GitError {
    let name = repo
        .find_reference("HEAD")
        .ok()
        .and_then(|reference| {
            reference
                .symbolic_target_bytes()
                .map(|name| String::from_utf8_lossy(name).into_owned())
        })
        .unwrap_or_else(|| "HEAD".to_owned());
    native("read HEAD")(git2::Error::new(
        error.code(),
        error.class(),
        format!("{name}: {}", error.message()),
    ))
}

pub(super) fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn tree_limit() -> GitError {
    GitError::Unsupported {
        operation: "previewing a file tree with more than 100,000 entries",
    }
}

fn mode_kind(mode: u32) -> FileTreeKind {
    match mode & 0o170000 {
        0o040000 => FileTreeKind::Directory,
        0o120000 => FileTreeKind::Symlink,
        0o160000 => FileTreeKind::Submodule,
        _ => FileTreeKind::File,
    }
}

fn insert_entry(entries: &mut BTreeMap<String, FileTreeEntry>, path: String, kind: FileTreeKind) {
    let mut parent = Path::new(&path).parent();
    while let Some(directory) = parent.filter(|path| !path.as_os_str().is_empty()) {
        let path = directory.to_string_lossy().replace('\\', "/");
        entries.entry(path.clone()).or_insert(FileTreeEntry {
            path,
            kind: FileTreeKind::Directory,
            index_status: ' ',
            worktree_status: ' ',
        });
        parent = directory.parent();
    }
    entries
        .entry(path.clone())
        .and_modify(|entry| entry.kind = kind)
        .or_insert(FileTreeEntry {
            path,
            kind,
            index_status: ' ',
            worktree_status: ' ',
        });
}

fn add_working_change(
    root: &Path,
    change: &FileChange,
    entries: &mut BTreeMap<String, FileTreeEntry>,
    checked_directories: &mut BTreeSet<String>,
    warnings: &mut Vec<TreeWarning>,
) -> Result<(), GitError> {
    let path = change.path.trim_end_matches('/');
    if let Err(error) = diff::validate_path(path) {
        warn(warnings, path, error.to_string());
        return Ok(());
    }
    let mut relative = PathBuf::new();
    for component in Path::new(path).components() {
        relative.push(component);
        let current = relative.to_string_lossy().replace('\\', "/");
        if entries
            .get(&current)
            .is_some_and(|entry| entry.kind == FileTreeKind::Submodule)
        {
            return Ok(());
        }
        if checked_directories.contains(&current) {
            continue;
        }
        let metadata = match fs::symlink_metadata(root.join(&relative)) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                warn(warnings, &current, error.to_string());
                return Ok(());
            }
        };
        let kind = if is_link(&metadata) {
            FileTreeKind::Symlink
        } else if metadata.is_dir() {
            FileTreeKind::Directory
        } else {
            FileTreeKind::File
        };
        insert_entry(entries, current.clone(), kind);
        if entries.len() > MAX_TREE_ENTRIES {
            return Err(tree_limit());
        }
        let nested_repository =
            kind == FileTreeKind::Directory && root.join(&relative).join(".git").exists();
        if kind == FileTreeKind::Symlink || nested_repository {
            let prefix = format!("{current}/");
            entries.retain(|path, _| !path.starts_with(&prefix));
            return Ok(());
        }
        if kind == FileTreeKind::Directory {
            checked_directories.insert(current);
        }
    }
    Ok(())
}

fn warn(warnings: &mut Vec<TreeWarning>, path: &str, message: String) {
    if warnings.len() < 256 {
        warnings.push(TreeWarning {
            path: path.to_owned(),
            message,
        });
    } else if warnings.len() == 256 {
        warnings.push(TreeWarning {
            path: String::new(),
            message: "Further file-tree warnings were omitted".into(),
        });
    }
}

pub(super) fn render_patch(
    comparison: &git2::Diff<'_>,
    position: usize,
    file: &str,
) -> Result<String, GitError> {
    let Some(mut patch) =
        Patch::from_diff(comparison, position).map_err(native("render file diff"))?
    else {
        return Err(GitError::BinaryFile {
            path: file.to_owned(),
        });
    };
    // Measure before serialization and bound every callback append. Do not let
    // git_patch_to_buf allocate the entire output before checking its size.
    let measured = patch.size(true, true, true);
    diff::check_patch_size(measured, file)?;
    let mut bytes = Vec::with_capacity(measured);
    let mut line_breaks = 0usize;
    let mut failure = None;
    let printed = patch.print(&mut |_, _, line| {
        let content = line.content();
        let prefix = matches!(line.origin(), '+' | '-' | ' ');
        if let Err(error) = diff::check_patch_size(
            bytes
                .len()
                .saturating_add(content.len())
                .saturating_add(usize::from(prefix)),
            file,
        ) {
            failure = Some(error);
            return false;
        }
        line_breaks =
            line_breaks.saturating_add(content.iter().filter(|&&byte| byte == b'\n').count());
        if line_breaks.saturating_add(usize::from(
            content.last().is_some_and(|&byte| byte != b'\n'),
        )) > diff::MAX_DIFF_LINES
        {
            failure = Some(GitError::LineCountLimit {
                path: file.to_owned(),
                limit_lines: diff::MAX_DIFF_LINES,
            });
            return false;
        }
        if prefix {
            bytes.push(line.origin() as u8);
        }
        bytes.extend_from_slice(content);
        true
    });
    if let Some(error) = failure {
        return Err(error);
    }
    printed.map_err(native("render file diff"))?;
    let patch = String::from_utf8(bytes).map_err(|_| GitError::BinaryFile {
        path: file.to_owned(),
    })?;
    diff::validate_patch(&patch, file)?;
    if patch
        .lines()
        .any(|line| line.starts_with("Binary files ") || line == "GIT binary patch")
    {
        return Err(GitError::BinaryFile {
            path: file.to_owned(),
        });
    }
    Ok(patch)
}

/// Registration lookup is deliberately independent of `tree`: selecting a
/// child must not stat every file or ask every sibling for its working status.
fn submodule_registration(
    repo: &Repository,
    root: &Path,
    relative: &str,
) -> Result<Option<SubmoduleInfo>, GitError> {
    diff::validate_path(relative)?;
    let configured = match repo.find_submodule(relative) {
        Ok(module) if module.path() == Path::new(relative) => Some(module),
        Ok(_) => None,
        Err(error) if matches!(error.code(), ErrorCode::NotFound | ErrorCode::Exists) => None,
        Err(error) => return Err(native("read selected submodule registration")(error)),
    };
    let index = repo.index().map_err(native("read submodule gitlink"))?;
    let index_id = index
        .get_path(Path::new(relative), 0)
        .filter(|entry| entry.mode == 0o160000)
        .map(|entry| entry.id);
    let head_id = if let Some(tree) = head_tree(repo)? {
        match tree.get_path(Path::new(relative)) {
            Ok(entry) if entry.filemode() == 0o160000 => Some(entry.id()),
            Ok(_) => None,
            Err(error) if error.code() == ErrorCode::NotFound => None,
            Err(error) => return Err(native("read committed submodule gitlink")(error)),
        }
    } else {
        None
    };
    if configured.is_none() && index_id.is_none() && head_id.is_none() {
        return Ok(None);
    }
    let mut module = SubmoduleInfo {
        name: configured.as_ref().map_or_else(
            || relative.to_owned(),
            |module| String::from_utf8_lossy(module.name_bytes()).into_owned(),
        ),
        path: relative.to_owned(),
        url: configured
            .as_ref()
            .map(|module| module.url().map(|url| url.map(str::to_owned)))
            .transpose()
            .map_err(native("read selected submodule URL"))?
            .flatten(),
        head_id: head_id.map(|id| id.to_string()),
        index_id: index_id.map(|id| id.to_string()),
        workdir_id: None,
        initialized: false,
        dirty: false,
    };
    let candidate = diff::checked_working_path(root, relative)?;
    if !fs::symlink_metadata(&candidate).is_ok_and(|metadata| is_link(&metadata))
        && let Ok(child) = Repository::open(&candidate)
        && let (Ok(actual), Ok(expected)) = (self::root(&child), fs::canonicalize(&candidate))
        && actual == expected
    {
        module.initialized = true;
        module.workdir_id = child
            .head()
            .ok()
            .and_then(|head| head.target())
            .map(|id| id.to_string());
    }
    module.dirty = module.head_id != module.index_id || module.workdir_id != module.index_id;
    Ok(Some(module))
}

fn read_submodules(
    repo: &Repository,
    root: &Path,
    links: &BTreeMap<String, (Option<Oid>, Option<Oid>)>,
) -> Result<Vec<SubmoduleInfo>, GitError> {
    let configured = match repo.submodules() {
        Ok(modules) => modules,
        Err(error) if error.code() == ErrorCode::NotFound => Vec::new(),
        Err(error) => return Err(native("read submodule configuration")(error)),
    };
    let mut modules = BTreeMap::new();
    // Parent status respects ignore rules. The tree has historically shown
    // actual child dirtiness as well, so only ignored children require a
    // separate read; ordinary children reuse the parent's complete status.
    let parent_ignores_children = repo
        .config()
        .ok()
        .and_then(|config| config.get_string("diff.ignoreSubmodules").ok())
        .is_some_and(|value| !matches!(value.to_ascii_lowercase().as_str(), "none" | "false"));
    let mut needs_dirty_check = BTreeSet::new();
    for module in configured {
        let path = module.path().to_string_lossy().replace('\\', "/");
        diff::validate_path(&path)?;
        if parent_ignores_children
            || matches!(
                module.ignore_rule(),
                SubmoduleIgnore::Untracked | SubmoduleIgnore::Dirty | SubmoduleIgnore::All
            )
        {
            needs_dirty_check.insert(path.clone());
        }
        modules.insert(
            path.clone(),
            SubmoduleInfo {
                name: String::from_utf8_lossy(module.name_bytes()).into_owned(),
                path,
                url: module
                    .url()
                    .map_err(native("read submodule URL"))?
                    .map(str::to_owned),
                head_id: module.head_id().map(|id| id.to_string()),
                index_id: module.index_id().map(|id| id.to_string()),
                workdir_id: None,
                initialized: false,
                dirty: false,
            },
        );
    }
    for (path, (head, index)) in links {
        if !modules.contains_key(path) {
            // An orphan gitlink has no configuration for parent status to use.
            needs_dirty_check.insert(path.clone());
        }
        modules.entry(path.clone()).or_insert(SubmoduleInfo {
            name: path.clone(),
            path: path.clone(),
            url: None,
            head_id: head.map(|id| id.to_string()),
            index_id: index.map(|id| id.to_string()),
            workdir_id: None,
            initialized: false,
            dirty: false,
        });
    }
    for module in modules.values_mut() {
        if fs::symlink_metadata(root.join(&module.path)).is_ok_and(|metadata| is_link(&metadata)) {
            continue;
        }
        let candidate = diff::checked_working_path(root, &module.path)?;
        // Do not discover upwards: an uninitialized directory is not its parent repo.
        if !fs::symlink_metadata(&candidate).is_ok_and(|metadata| is_link(&metadata))
            && let Ok(child) = Repository::open(&candidate)
            && child.workdir().is_some()
        {
            module.initialized = true;
            module.workdir_id = child
                .head()
                .ok()
                .and_then(|head| head.target())
                .map(|id| id.to_string());
            // Parent status already inspected child dirtiness. Repeating child
            // status here costs a full extra traversal for every submodule.
            module.dirty = module.workdir_id != module.index_id;
            if needs_dirty_check.contains(&module.path) {
                module.dirty |= !changes(&child)?.is_empty();
            }
        }
        module.dirty |= module.head_id != module.index_id;
    }
    Ok(modules.into_values().collect())
}

#[cfg(test)]
mod patch_limit_tests {
    use super::*;

    #[test]
    fn bounded_serializer_preserves_native_patch_prefixes_and_missing_newline() {
        let input = b"diff --git a/note.txt b/note.txt\nindex 3367afd..3e75765 100644\n--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n\\ No newline at end of file\n";
        let comparison = git2::Diff::from_buffer(input).unwrap();
        let mut patch = Patch::from_diff(&comparison, 0).unwrap().unwrap();
        let expected = std::str::from_utf8(&patch.to_buf().unwrap())
            .unwrap()
            .to_owned();
        let output = render_patch(&comparison, 0, "note.txt").unwrap();
        assert_eq!(output, expected);
        assert_eq!(diff::parse_lines(&output).unwrap().len(), 4);
    }

    #[test]
    fn real_patch_with_many_short_lines_stops_before_unbounded_row_allocation() {
        let count = diff::MAX_DIFF_LINES + 1;
        let mut input = format!(
            "diff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,{count} @@\n"
        );
        input.push_str(&"+x\n".repeat(count));
        let comparison = git2::Diff::from_buffer(input.as_bytes()).unwrap();
        assert!(matches!(
            render_patch(&comparison, 0, "new.txt"),
            Err(GitError::LineCountLimit {
                limit_lines: diff::MAX_DIFF_LINES,
                ..
            })
        ));
    }
}
