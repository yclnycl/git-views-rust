//! Paged commit traversal and comparisons against a commit's first parent.

use crate::{CommitSummary, FileChange, FileDiff, GitError, diff, native};
use git2::{Commit, Delta, Diff, DiffOptions, Oid, Repository, Sort};
use std::path::Path;

const MAX_COMMIT_BYTES: usize = 1024 * 1024;
const MAX_COMMIT_CHANGES: usize = 20_000;
const MAX_CHANGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;

/// A bounded page of HEAD history in topological/time order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistoryPage {
    /// Selected page, newest first; at most 200 commits.
    pub commits: Vec<CommitSummary>,
    /// Whether at least one further commit exists after this page.
    pub has_more: bool,
}

/// A selected commit and its changed paths relative to its first parent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitDetails {
    pub id: String,
    pub subject: String,
    pub message: String,
    pub author: String,
    /// Author timestamp as Unix seconds.
    pub time: i64,
    pub parent_ids: Vec<String>,
    /// For committed changes, index_status holds A/M/D/R/T and worktree_status is a space.
    pub changes: Vec<FileChange>,
}

pub(super) fn page(path: &Path, skip: usize, limit: usize) -> Result<HistoryPage, GitError> {
    page_from(path, None, skip, limit)
}

/// Traverse a captured full commit OID instead of the mutable current HEAD.
/// This keeps page offsets stable while external fetch/commit/checkout occurs.
pub(super) fn page_at(
    path: &Path,
    anchor: &str,
    skip: usize,
    limit: usize,
) -> Result<HistoryPage, GitError> {
    let anchor = if anchor.len() == 40 {
        Oid::from_str(anchor).ok()
    } else {
        None
    }
    .ok_or(GitError::InvalidRequest {
        operation: "read pinned history",
        reason: "history anchor must be a full commit OID",
    })?;
    page_from(path, Some(anchor), skip, limit)
}

fn page_from(
    path: &Path,
    anchor: Option<Oid>,
    skip: usize,
    limit: usize,
) -> Result<HistoryPage, GitError> {
    if !(1..=200).contains(&limit) {
        return Err(GitError::InvalidRequest {
            operation: "read history",
            reason: "page size must be between 1 and 200",
        });
    }
    let repo = native::open(path)?;
    if anchor.is_none() && matches!(native::read_head(&repo)?, crate::Head::Unborn(_)) {
        return Ok(HistoryPage::default());
    }
    let mut walk = repo.revwalk().map_err(native::native("read history"))?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)
        .map_err(native::native("sort history"))?;
    if let Some(anchor) = anchor {
        read_commit(&repo, anchor)?;
        walk.push(anchor)
            .map_err(native::native("read pinned history"))?;
    } else {
        walk.push_head()
            .map_err(native::native("read history HEAD"))?;
    }
    // Iterator::skip would discard Result::Err in the skipped prefix and could
    // misreport a broken object graph as the end of an otherwise valid page.
    for _ in 0..skip {
        if walk
            .next()
            .transpose()
            .map_err(native::native("skip historical commits"))?
            .is_none()
        {
            return Ok(HistoryPage::default());
        }
    }
    let mut commits = Vec::new();
    let mut bytes = 0_usize;
    for id in walk.by_ref().take(limit) {
        let commit = read_commit(&repo, id.map_err(native::native("walk history"))?)?;
        let author = commit.author();
        bytes = bytes
            .saturating_add(std::mem::size_of::<CommitSummary>())
            .saturating_add(40)
            .saturating_add(metadata_length(commit.summary_bytes().unwrap_or_default()))
            .saturating_add(metadata_length(author.name_bytes()));
        if bytes > MAX_PAGE_BYTES {
            return Err(GitError::InvalidRequest {
                operation: "read history",
                reason: "history page metadata exceeds the 4 MiB budget; request a smaller page",
            });
        }
        commits.push(summary(&commit));
    }
    let has_more = walk
        .next()
        .transpose()
        .map_err(native::native("walk history"))?
        .is_some();
    Ok(HistoryPage { commits, has_more })
}

pub(super) fn details(path: &Path, id: &str) -> Result<CommitDetails, GitError> {
    let repo = native::open(path)?;
    let commit = resolve_commit(&repo, id)?;
    let summary = summary(&commit);
    let comparison = comparison(&repo, &commit)?;
    let mut changes = Vec::with_capacity(comparison.deltas().len());
    let mut bytes = 0_usize;
    for delta in comparison.deltas() {
        let path = if delta.status() == Delta::Deleted {
            delta.old_file().path()
        } else {
            delta.new_file().path()
        }
        .unwrap_or_else(|| Path::new(""));
        let path = path.to_string_lossy();
        bytes = bytes
            .saturating_add(std::mem::size_of::<FileChange>())
            .saturating_add(path.len());
        if bytes > MAX_CHANGE_BYTES {
            return Err(GitError::InvalidRequest {
                operation: "read committed changes",
                reason: "committed path metadata exceeds the 4 MiB history budget",
            });
        }
        changes.push(FileChange {
            path: path.into_owned(),
            index_status: match delta.status() {
                Delta::Added => 'A',
                Delta::Deleted => 'D',
                Delta::Renamed => 'R',
                Delta::Copied => 'C',
                Delta::Typechange => 'T',
                _ => 'M',
            },
            worktree_status: ' ',
        });
    }
    Ok(CommitDetails {
        id: summary.id,
        subject: summary.subject,
        message: String::from_utf8_lossy(commit.message_bytes()).into_owned(),
        author: summary.author,
        time: commit.author().when().seconds(),
        parent_ids: commit.parent_ids().map(|id| id.to_string()).collect(),
        changes,
    })
}

pub(super) fn file_diff(path: &Path, id: &str, file: &str) -> Result<FileDiff, GitError> {
    diff::validate_path(file)?;
    let repo = native::open(path)?;
    let commit = resolve_commit(&repo, id)?;
    let comparison = comparison(&repo, &commit)?;
    let selected = comparison.deltas().enumerate().find(|(_, delta)| {
        delta.new_file().path() == Some(Path::new(file))
            || (delta.status() == Delta::Deleted
                && delta.old_file().path() == Some(Path::new(file)))
    });
    let Some((position, delta)) = selected else {
        return Err(GitError::InvalidPath {
            path: file.to_owned(),
            reason: "the file is not changed by this commit",
        });
    };
    let old_file = delta.old_file();
    let new_file = delta.new_file();
    let old_text = if old_file.id().is_zero() {
        String::new()
    } else {
        native::object_text(&repo, old_file.id(), old_file.mode().into(), file)?
    };
    let new_text = if new_file.id().is_zero() {
        String::new()
    } else {
        native::object_text(&repo, new_file.id(), new_file.mode().into(), file)?
    };
    let patch = native::render_patch(&comparison, position, file)?;
    Ok(FileDiff {
        path: file.to_owned(),
        old_text,
        new_text,
        lines: diff::parse_lines(&patch)?,
        patch,
    })
}

pub(super) fn resolve_commit<'repo>(
    repo: &'repo Repository,
    revision: &str,
) -> Result<Commit<'repo>, GitError> {
    if revision.is_empty() || revision.contains('\0') {
        return Err(GitError::InvalidRequest {
            operation: "resolve revision",
            reason: "select a nonempty commit revision",
        });
    }
    if revision.len() == 40
        && let Ok(id) = Oid::from_str(revision)
    {
        return read_commit(repo, id);
    }
    let commit = repo
        .revparse_single(revision)
        .and_then(|object| object.peel_to_commit())
        .map_err(native::native("resolve commit revision"))?;
    check_commit_size(repo, commit.id())?;
    Ok(commit)
}

fn check_commit_size(repo: &Repository, id: Oid) -> Result<(), GitError> {
    let (bytes, _) = repo
        .odb()
        .and_then(|odb| odb.read_header(id))
        .map_err(native::native("read commit object size"))?;
    if bytes > MAX_COMMIT_BYTES {
        return Err(GitError::FileTooLarge {
            path: format!("commit {id}"),
            limit_bytes: MAX_COMMIT_BYTES,
        });
    }
    Ok(())
}

fn read_commit(repo: &Repository, id: Oid) -> Result<Commit<'_>, GitError> {
    check_commit_size(repo, id)?;
    repo.find_commit(id)
        .map_err(native::native("read historical commit"))
}

fn comparison<'repo>(
    repo: &'repo Repository,
    commit: &Commit<'repo>,
) -> Result<Diff<'repo>, GitError> {
    let tree = commit
        .tree()
        .map_err(native::native("read committed tree"))?;
    let parent = if commit.parent_count() == 0 {
        None
    } else {
        let id = commit
            .parent_id(0)
            .map_err(native::native("read parent ID"))?;
        Some(
            read_commit(repo, id)?
                .tree()
                .map_err(native::native("read parent tree"))?,
        )
    };
    let mut options = DiffOptions::new();
    options.context_lines(3).include_typechange(true);
    let mut comparison = repo
        .diff_tree_to_tree(parent.as_ref(), Some(&tree), Some(&mut options))
        .map_err(native::native("compare commit trees"))?;
    if comparison.deltas().len() > MAX_COMMIT_CHANGES {
        return Err(GitError::InvalidRequest {
            operation: "read committed changes",
            reason: "commit changes exceed the 20000-path history budget",
        });
    }
    native::find_similar_bounded(repo, &mut comparison)?;
    Ok(comparison)
}

fn summary(commit: &Commit<'_>) -> CommitSummary {
    CommitSummary {
        id: commit.id().to_string(),
        subject: metadata_text(commit.summary_bytes().unwrap_or_default()),
        author: metadata_text(commit.author().name_bytes()),
    }
}

fn metadata_length(bytes: &[u8]) -> usize {
    bytes
        .utf8_chunks()
        .map(|chunk| chunk.valid().len() + if chunk.invalid().is_empty() { 0 } else { 3 })
        .sum()
}

fn metadata_text(bytes: &[u8]) -> String {
    // Preallocate the exact lossy UTF-8 length used by the page budget. Invalid
    // encodings cannot expand a nominally bounded page after it was checked.
    let mut text = String::with_capacity(metadata_length(bytes));
    for chunk in bytes.utf8_chunks() {
        text.push_str(chunk.valid());
        if !chunk.invalid().is_empty() {
            text.push('\u{fffd}');
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        parent: PathBuf,
        root: PathBuf,
        repo: Repository,
    }

    impl Fixture {
        fn new() -> Self {
            let parent =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/git-history-tests");
            fs::create_dir_all(&parent).unwrap();
            let parent = fs::canonicalize(parent).unwrap();
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = parent.join(format!(
                "gitviews-pinned-history-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let repo = Repository::init(&root).unwrap();
            repo.set_head("refs/heads/main").unwrap();
            Self { parent, root, repo }
        }

        fn commit(&self, parent: Option<Oid>, message: &str) -> Oid {
            let tree = self.repo.treebuilder(None).unwrap().write().unwrap();
            let tree = self.repo.find_tree(tree).unwrap();
            let signature =
                git2::Signature::now("History Fixture", "history@example.invalid").unwrap();
            let parent = parent.map(|id| self.repo.find_commit(id).unwrap());
            let parents: Vec<_> = parent.iter().collect();
            self.repo
                .commit(None, &signature, &signature, message, &tree, &parents)
                .unwrap()
        }

        fn move_head(&self, id: Oid) {
            self.repo
                .reference("refs/heads/main", id, true, "fixture advance")
                .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Ok(root) = fs::canonicalize(&self.root)
                && root.parent() == Some(self.parent.as_path())
                && root.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .starts_with("gitviews-pinned-history-")
                })
            {
                let _ = fs::remove_dir_all(root);
            }
        }
    }

    #[test]
    fn pinned_pages_do_not_repeat_or_skip_commits_after_head_advances() {
        let fixture = Fixture::new();
        let first = fixture.commit(None, "first");
        let second = fixture.commit(Some(first), "second");
        let anchor = fixture.commit(Some(second), "captured HEAD");
        fixture.move_head(anchor);
        let initial = page_at(&fixture.root, &anchor.to_string(), 0, 2).unwrap();
        assert!(initial.has_more);
        assert_eq!(
            initial
                .commits
                .iter()
                .map(|commit| commit.id.clone())
                .collect::<Vec<_>>(),
            [anchor.to_string(), second.to_string()]
        );
        let latest = fixture.commit(Some(anchor), "external new commit");
        fixture.move_head(latest);
        let next = page_at(&fixture.root, &anchor.to_string(), 2, 2).unwrap();
        assert!(!next.has_more);
        assert_eq!(next.commits.len(), 1);
        assert_eq!(next.commits[0].id, first.to_string());
        assert_eq!(
            page(&fixture.root, 0, 1).unwrap().commits[0].id,
            latest.to_string()
        );
    }

    #[test]
    fn an_existing_anchor_remains_readable_when_head_becomes_unborn() {
        let fixture = Fixture::new();
        let anchor = fixture.commit(None, "retained object");
        assert!(page(&fixture.root, 0, 1).unwrap().commits.is_empty());
        let pinned = page_at(&fixture.root, &anchor.to_string(), 0, 1).unwrap();
        assert_eq!(pinned.commits[0].id, anchor.to_string());
        assert!(matches!(
            page_at(&fixture.root, "HEAD", 0, 1),
            Err(GitError::InvalidRequest { .. })
        ));
    }

    #[test]
    fn oversized_commit_metadata_is_refused_before_copying_history_rows_or_details() {
        let fixture = Fixture::new();
        let id = fixture.commit(None, &"x".repeat(MAX_COMMIT_BYTES + 1));
        for result in [
            page_at(&fixture.root, &id.to_string(), 0, 1).map(|_| ()),
            details(&fixture.root, &id.to_string()).map(|_| ()),
        ] {
            assert!(matches!(
                result,
                Err(GitError::FileTooLarge {
                    limit_bytes: MAX_COMMIT_BYTES,
                    ..
                })
            ));
        }
        let child = fixture.commit(Some(id), "small child of an oversized parent");
        assert!(matches!(
            details(&fixture.root, &child.to_string()),
            Err(GitError::FileTooLarge {
                limit_bytes: MAX_COMMIT_BYTES,
                ..
            })
        ));
    }

    #[test]
    fn a_missing_object_in_a_skipped_prefix_is_not_reported_as_end_of_history() {
        let fixture = Fixture::new();
        let tree = fixture.repo.treebuilder(None).unwrap().write().unwrap();
        let raw = format!(
            "tree {tree}\nparent ffffffffffffffffffffffffffffffffffffffff\nauthor Fixture <fixture@example.invalid> 1700000000 +0000\ncommitter Fixture <fixture@example.invalid> 1700000000 +0000\n\nbroken parent graph\n"
        );
        let anchor = fixture
            .repo
            .odb()
            .unwrap()
            .write(git2::ObjectType::Commit, raw.as_bytes())
            .unwrap();
        assert!(matches!(
            page_at(&fixture.root, &anchor.to_string(), 1, 1),
            Err(GitError::Native { .. })
        ));
    }

    #[test]
    fn a_page_budget_rejects_large_metadata_without_reporting_a_false_history_end() {
        let fixture = Fixture::new();
        let message = "x".repeat(800_000);
        let mut anchor = None;
        for _ in 0..6 {
            anchor = Some(fixture.commit(anchor, &message));
        }
        let anchor = anchor.unwrap().to_string();
        let smaller = page_at(&fixture.root, &anchor, 0, 4).unwrap();
        assert_eq!(smaller.commits.len(), 4);
        assert!(smaller.has_more);
        assert!(matches!(
            page_at(&fixture.root, &anchor, 0, 6),
            Err(GitError::InvalidRequest {
                operation: "read history",
                ..
            })
        ));
    }
}
