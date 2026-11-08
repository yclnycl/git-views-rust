//! A frozen, multi-reference commit graph. Filtering never drops connecting rows.

use crate::{GraphCommit, HistoryToolsError, HistoryToolsErrorKind, paths_equal};
use git2::{ErrorCode, ObjectType, Oid, Repository, Sort};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const MAX_OFFSET: usize = 50_000;
const MAX_REFERENCES: usize = 4_096;
const MAX_REFERENCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_OBJECT_BYTES: usize = 1024 * 1024;
const MAX_TREE_BYTES: usize = 8 * 1024 * 1024;
const TIME_BUDGET: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub struct GraphHistoryRequest {
    /// Reuse the first page's snapshot when loading another page. Reference moves
    /// and newly created branches then do not reorder the existing graph.
    pub snapshot: Option<GraphHistorySnapshot>,
    /// Empty selects all local/remote branches plus HEAD. Otherwise use full ref
    /// names (or HEAD). Tags label commits but are not default traversal roots.
    pub references: Vec<String>,
    /// Offset counts graph rows, including rows that do not match the filter.
    pub offset: usize,
    /// 1..=200. The default request uses 100 rows.
    pub limit: usize,
    pub filter: GraphHistoryFilter,
}

impl Default for GraphHistoryRequest {
    fn default() -> Self {
        Self {
            snapshot: None,
            references: Vec::new(),
            offset: 0,
            limit: 100,
            filter: GraphHistoryFilter::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GraphHistoryFilter {
    /// Case-insensitive literal substring in the full commit message or hash.
    pub text: String,
    /// Case-insensitive literal substring in author name or email.
    pub author: String,
    /// Inclusive commit timestamps in Unix seconds.
    pub since: Option<i64>,
    pub until: Option<i64>,
    /// Literal repository-relative file or directory; no shell/glob expansion.
    pub path: Option<String>,
}

impl GraphHistoryFilter {
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
            && self.author.trim().is_empty()
            && self.since.is_none()
            && self.until.is_none()
            && self.path.is_none()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphReferenceKind {
    Head,
    LocalBranch,
    RemoteBranch,
    Tag,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphReference {
    /// HEAD or full refs/heads/*, refs/remotes/*, refs/tags/* name.
    pub full_name: String,
    pub name: String,
    pub target: String,
    pub kind: GraphReferenceKind,
    pub is_current: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GraphHistorySnapshot {
    /// Prevent an old graph from being accidentally continued in another root.
    pub root: PathBuf,
    /// Sorted, deduplicated full commit OIDs, including detached HEAD.
    pub tips: Vec<String>,
    /// Frozen labels from the same initial read, including annotated tags.
    pub references: Vec<GraphReference>,
}

#[derive(Clone, Debug)]
pub struct GraphHistoryRow {
    /// Every parent is retained, including parents beyond the loaded page.
    pub commit: GraphCommit,
    /// False rows are connecting context. A UI should dim them, not pretend that
    /// a visible child directly parents a later matching commit.
    pub matches_filter: bool,
}

#[derive(Clone, Debug, Default)]
pub struct GraphHistoryPage {
    pub snapshot: GraphHistorySnapshot,
    pub rows: Vec<GraphHistoryRow>,
    pub next_offset: usize,
    pub has_more: bool,
    /// A metadata, time, or total traversal budget ended this page.
    pub truncated: bool,
    pub scanned_commits: usize,
}

pub(super) fn read(
    root: &Path,
    request: &GraphHistoryRequest,
    cancel: &AtomicBool,
) -> Result<GraphHistoryPage, HistoryToolsError> {
    check_cancel(cancel)?;
    validate_request(request)?;
    let root = fs::canonicalize(root).map_err(|_| invalid_root())?;
    let repo = Repository::open(&root).map_err(|_| invalid_root())?;
    if repo.is_bare()
        || repo
            .workdir()
            .and_then(|p| fs::canonicalize(p).ok())
            .as_ref()
            != Some(&root)
    {
        return Err(invalid_root());
    }
    let started = Instant::now();
    let snapshot = match &request.snapshot {
        Some(snapshot) => {
            validate_snapshot(&repo, &root, snapshot, cancel, started)?;
            snapshot.clone()
        }
        None => capture_snapshot(&repo, root, &request.references, cancel, started)?,
    };
    let mut page = GraphHistoryPage {
        snapshot,
        next_offset: request.offset,
        ..GraphHistoryPage::default()
    };
    let mut walk = repo.revwalk().map_err(map_git)?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)
        .map_err(map_git)?;
    for tip in &page.snapshot.tips {
        check_cancel(cancel)?;
        walk.push(parse_oid(tip)?).map_err(map_git)?;
    }
    for _ in 0..request.offset {
        check_budget(cancel, started)?;
        if walk.next().transpose().map_err(map_git)?.is_none() {
            return Ok(page);
        }
    }
    let text = request.filter.text.trim().to_lowercase();
    let author = request.filter.author.trim().to_lowercase();
    let mut metadata = 0usize;
    while page.rows.len() < request.limit {
        check_cancel(cancel)?;
        if started.elapsed() > TIME_BUDGET || page.next_offset >= MAX_OFFSET {
            page.truncated = true;
            page.has_more = page.next_offset < MAX_OFFSET;
            return Ok(page);
        }
        let Some(oid) = walk.next().transpose().map_err(map_git)? else {
            return Ok(page);
        };
        let commit = checked_commit(&repo, oid)?;
        if commit.parent_count() > 128 {
            return Err(limit_error(
                "单个提交超过 128 个父提交，无法在图形预算内呈现。",
            ));
        }
        let parents = commit
            .parent_ids()
            .map(|id| id.to_string())
            .collect::<Vec<_>>();
        let subject = truncate_text(
            &String::from_utf8_lossy(commit.summary_bytes().unwrap_or_default()),
            2_048,
        );
        let author_name =
            truncate_text(&String::from_utf8_lossy(commit.author().name_bytes()), 512);
        let row_bytes = parents.len() * 40 + subject.len() + author_name.len() + 40;
        if metadata.saturating_add(row_bytes) > MAX_PAGE_BYTES {
            page.truncated = true;
            page.has_more = true;
            return Ok(page);
        }
        let matches_filter = matches_filter(
            &repo,
            &commit,
            &request.filter,
            &text,
            &author,
            cancel,
            started,
        )?;
        page.rows.push(GraphHistoryRow {
            commit: GraphCommit {
                id: oid.to_string(),
                parents,
                subject,
                author: author_name,
                time: commit.time().seconds(),
            },
            matches_filter,
        });
        metadata += row_bytes;
        page.next_offset += 1;
        page.scanned_commits += 1;
    }
    check_cancel(cancel)?;
    page.has_more = walk.next().transpose().map_err(map_git)?.is_some();
    if page.next_offset >= MAX_OFFSET && page.has_more {
        page.has_more = false;
        page.truncated = true;
    }
    Ok(page)
}

fn capture_snapshot(
    repo: &Repository,
    root: PathBuf,
    selected: &[String],
    cancel: &AtomicBool,
    started: Instant,
) -> Result<GraphHistorySnapshot, HistoryToolsError> {
    let mut snapshot = GraphHistorySnapshot {
        root,
        ..GraphHistorySnapshot::default()
    };
    let head = match repo.head() {
        Ok(reference) => Some(reference),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => None,
        Err(error) => return Err(map_git(error)),
    };
    let head_name = head
        .as_ref()
        .map(|head| head.name())
        .transpose()
        .map_err(map_git)?;
    let mut metadata = 0usize;
    for (glob, kind) in [
        ("refs/heads/*", GraphReferenceKind::LocalBranch),
        ("refs/remotes/*", GraphReferenceKind::RemoteBranch),
        ("refs/tags/*", GraphReferenceKind::Tag),
    ] {
        for reference in repo.references_glob(glob).map_err(map_git)? {
            check_budget(cancel, started)?;
            let reference = reference.map_err(map_git)?;
            // origin/HEAD duplicates origin's default branch and is not a branch.
            if reference.symbolic_target().map_err(map_git)?.is_some() {
                continue;
            }
            let full_name = reference.name().map_err(map_git)?;
            let Some(oid) = reference.target() else {
                continue;
            };
            let Some(target) = peel_commit(repo, oid)? else {
                continue;
            };
            let name = full_name
                .strip_prefix("refs/heads/")
                .or_else(|| full_name.strip_prefix("refs/remotes/"))
                .or_else(|| full_name.strip_prefix("refs/tags/"))
                .unwrap_or(full_name)
                .to_owned();
            metadata = metadata.saturating_add(full_name.len() + name.len() + 40);
            if metadata > MAX_REFERENCE_BYTES || snapshot.references.len() >= MAX_REFERENCES {
                return Err(limit_error(
                    "分支和标签数量或名称超出历史图引用预算，请缩小仓库范围。",
                ));
            }
            snapshot.references.push(GraphReference {
                full_name: full_name.into(),
                name,
                target: target.to_string(),
                kind,
                is_current: kind == GraphReferenceKind::LocalBranch && head_name == Some(full_name),
            });
        }
    }
    if let Some(head) = head.and_then(|head| head.target()) {
        checked_commit(repo, head)?;
        snapshot.references.push(GraphReference {
            full_name: "HEAD".into(),
            name: "HEAD".into(),
            target: head.to_string(),
            kind: GraphReferenceKind::Head,
            is_current: true,
        });
    }
    snapshot
        .references
        .sort_by(|left, right| left.full_name.cmp(&right.full_name));
    let mut tips = BTreeSet::new();
    if selected.is_empty() {
        for reference in &snapshot.references {
            if reference.kind != GraphReferenceKind::Tag {
                tips.insert(reference.target.clone());
            }
        }
    } else {
        for name in selected {
            let reference = snapshot
                .references
                .iter()
                .find(|reference| reference.full_name == *name)
                .ok_or_else(|| invalid("选中的历史分支或标签已不存在，请刷新后重试。"))?;
            tips.insert(reference.target.clone());
        }
    }
    snapshot.tips = tips.into_iter().collect();
    check_budget(cancel, started)?;
    Ok(snapshot)
}

fn validate_snapshot(
    repo: &Repository,
    root: &Path,
    snapshot: &GraphHistorySnapshot,
    cancel: &AtomicBool,
    started: Instant,
) -> Result<(), HistoryToolsError> {
    if !paths_equal(root, &snapshot.root) {
        return Err(invalid("历史分页快照不属于当前仓库，请重新加载历史。"));
    }
    if snapshot.tips.len() > MAX_REFERENCES + 1 || snapshot.references.len() > MAX_REFERENCES + 1 {
        return Err(limit_error("历史快照引用数量超出预算。"));
    }
    let mut metadata = 0usize;
    for reference in &snapshot.references {
        check_budget(cancel, started)?;
        metadata = metadata.saturating_add(
            reference.full_name.len() + reference.name.len() + reference.target.len(),
        );
        if metadata > MAX_REFERENCE_BYTES + 128 {
            return Err(limit_error("历史快照引用元数据超出预算。"));
        }
        parse_oid(&reference.target)?;
    }
    for tip in &snapshot.tips {
        check_budget(cancel, started)?;
        checked_commit(repo, parse_oid(tip)?)?;
    }
    Ok(())
}

fn matches_filter(
    repo: &Repository,
    commit: &git2::Commit<'_>,
    filter: &GraphHistoryFilter,
    text: &str,
    author: &str,
    cancel: &AtomicBool,
    started: Instant,
) -> Result<bool, HistoryToolsError> {
    let time = commit.time().seconds();
    if filter.since.is_some_and(|since| time < since)
        || filter.until.is_some_and(|until| time > until)
    {
        return Ok(false);
    }
    if !text.is_empty()
        && !commit.id().to_string().contains(text)
        && !String::from_utf8_lossy(commit.message_bytes())
            .to_lowercase()
            .contains(text)
    {
        return Ok(false);
    }
    if !author.is_empty() {
        let signature = commit.author();
        if !String::from_utf8_lossy(signature.name_bytes())
            .to_lowercase()
            .contains(author)
            && !String::from_utf8_lossy(signature.email_bytes())
                .to_lowercase()
                .contains(author)
        {
            return Ok(false);
        }
    }
    let Some(path) = &filter.path else {
        return Ok(true);
    };
    let current = path_entry(repo, commit.tree_id(), path, cancel, started)?;
    if commit.parent_count() == 0 {
        return Ok(current.is_some());
    }
    for parent_id in commit.parent_ids() {
        check_budget(cancel, started)?;
        let parent = checked_commit(repo, parent_id)?;
        let before = path_entry(repo, parent.tree_id(), path, cancel, started)?;
        if before != current {
            return Ok(true);
        }
    }
    Ok(false)
}

fn path_entry(
    repo: &Repository,
    mut tree_id: Oid,
    path: &str,
    cancel: &AtomicBool,
    started: Instant,
) -> Result<Option<(Oid, i32)>, HistoryToolsError> {
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        check_budget(cancel, started)?;
        let (bytes, _) = repo
            .odb()
            .and_then(|odb| odb.read_header(tree_id))
            .map_err(map_git)?;
        if bytes > MAX_TREE_BYTES {
            return Err(limit_error("历史路径目录树超过 8 MiB 元数据预算。"));
        }
        let tree = repo.find_tree(tree_id).map_err(map_git)?;
        let Some(entry) = tree.get_name(part) else {
            return Ok(None);
        };
        if parts.peek().is_none() {
            return Ok(Some((entry.id(), entry.filemode())));
        }
        // A gitlink or symlink names an entry in this repository, not a tree that
        // may be followed into another repository or the working filesystem.
        if entry.kind() != Some(ObjectType::Tree) {
            return Ok(None);
        }
        tree_id = entry.id();
    }
    Ok(None)
}

fn validate_request(request: &GraphHistoryRequest) -> Result<(), HistoryToolsError> {
    if !(1..=200).contains(&request.limit) || request.offset > MAX_OFFSET {
        return Err(invalid("历史图每页限制为 1–200 项，总偏移最多 50000 项。"));
    }
    if request.references.len() > MAX_REFERENCES
        || request.references.iter().any(|name| {
            name.len() > 1_024
                || name.contains('\0')
                || !(name == "HEAD"
                    || name.starts_with("refs/heads/")
                    || name.starts_with("refs/remotes/")
                    || name.starts_with("refs/tags/"))
        })
    {
        return Err(invalid("请选择有效的本地分支、远程分支或标签引用。"));
    }
    let filter = &request.filter;
    if filter.text.len() > 2_048
        || filter.author.len() > 1_024
        || filter.text.contains('\0')
        || filter.author.contains('\0')
    {
        return Err(invalid("历史搜索条件过长或包含无效字符。"));
    }
    if let (Some(since), Some(until)) = (filter.since, filter.until)
        && since > until
    {
        return Err(invalid("历史起始日期不能晚于结束日期。"));
    }
    if let Some(path) = &filter.path {
        if path.len() > 4_096
            || path.split('/').count() > 128
            || path.split('/').any(|part| {
                part.is_empty()
                    || part == "."
                    || part == ".."
                    || part
                        .trim_end_matches([' ', '.'])
                        .eq_ignore_ascii_case(".git")
            })
        {
            return Err(invalid("请填写有效的仓库相对文件或目录路径。"));
        }
        crate::diff::validate_path(path)
            .map_err(|_| invalid("请填写有效的仓库相对文件或目录路径。"))?;
    }
    Ok(())
}

fn peel_commit(repo: &Repository, mut oid: Oid) -> Result<Option<Oid>, HistoryToolsError> {
    for _ in 0..32 {
        let (size, kind) = repo
            .odb()
            .and_then(|odb| odb.read_header(oid))
            .map_err(map_git)?;
        if size > MAX_OBJECT_BYTES {
            return Err(limit_error("引用指向的标签或提交超过 1 MiB 元数据预算。"));
        }
        match kind {
            ObjectType::Commit => return Ok(Some(oid)),
            ObjectType::Tag => {
                oid = repo.find_tag(oid).map_err(map_git)?.target_id();
            }
            _ => return Ok(None),
        }
    }
    Err(limit_error("标签引用链超过 32 层预算。"))
}

fn checked_commit(repo: &Repository, oid: Oid) -> Result<git2::Commit<'_>, HistoryToolsError> {
    let (size, _) = repo
        .odb()
        .and_then(|odb| odb.read_header(oid))
        .map_err(map_git)?;
    if size > MAX_OBJECT_BYTES {
        return Err(limit_error("提交对象超过 1 MiB 元数据预算。"));
    }
    repo.find_commit(oid).map_err(map_git)
}
fn parse_oid(value: &str) -> Result<Oid, HistoryToolsError> {
    if value.len() == 40
        && let Ok(oid) = Oid::from_str(value)
    {
        return Ok(oid);
    }
    Err(invalid("历史快照需要完整的提交 ID。"))
}
fn truncate_text(value: &str, limit: usize) -> String {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].into()
}
fn check_cancel(cancel: &AtomicBool) -> Result<(), HistoryToolsError> {
    if cancel.load(Ordering::Relaxed) {
        Err(HistoryToolsError::new(
            HistoryToolsErrorKind::Cancelled,
            "历史图读取已取消。",
        ))
    } else {
        Ok(())
    }
}
fn check_budget(cancel: &AtomicBool, started: Instant) -> Result<(), HistoryToolsError> {
    check_cancel(cancel)?;
    if started.elapsed() > TIME_BUDGET {
        return Err(limit_error(
            "历史图读取超过时间预算，请缩小范围或重新加载。",
        ));
    }
    Ok(())
}
fn invalid(message: &str) -> HistoryToolsError {
    HistoryToolsError::new(HistoryToolsErrorKind::InvalidRequest, message)
}
fn limit_error(message: &str) -> HistoryToolsError {
    HistoryToolsError::new(HistoryToolsErrorKind::Limit, message)
}
fn invalid_root() -> HistoryToolsError {
    HistoryToolsError::new(
        HistoryToolsErrorKind::InvalidRepository,
        "请选择可用的非裸仓库根目录。",
    )
}
fn map_git(error: git2::Error) -> HistoryToolsError {
    HistoryToolsError::new(
        if error.code() == ErrorCode::NotFound {
            HistoryToolsErrorKind::NotFound
        } else {
            HistoryToolsErrorKind::Failed
        },
        format!("读取 Git 历史图失败：{}", error.message()),
    )
}
