//! Bounded native history analysis and branch recovery, independent of checkout.
use crate::{FileDiff, TextEncoding, diff, history, native, source};
use git2::{
    BlameOptions, Delta, DiffFindOptions, DiffOptions, ErrorCode, Oid, Patch, Repository, Sort,
};
use std::{
    collections::BTreeSet,
    error::Error,
    fmt, fs,
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const MAX_OFFSET: usize = 50_000;
const MAX_SCAN: usize = 5_000;
const MAX_FILES: usize = 20_000;
const MAX_METADATA: usize = 4 * 1024 * 1024;
const MAX_STATS_BYTES: usize = 8 * 1024 * 1024;
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
const TIME_BUDGET: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub enum HistoryToolsRequest {
    Commits {
        anchor: Option<String>,
        offset: usize,
        limit: usize,
        path: Option<String>,
    },
    Blame {
        path: String,
        revision: Option<String>,
    },
    Compare {
        left: String,
        right: String,
        path: Option<String>,
    },
    RecoveryList {
        offset: usize,
        limit: usize,
    },
    Restore {
        name: String,
        commit_id: String,
        expected_head: Option<String>,
    },
}
impl HistoryToolsRequest {
    pub fn is_mutation(&self) -> bool {
        matches!(self, Self::Restore { .. })
    }
}

#[derive(Clone, Debug)]
pub enum HistoryToolsResult {
    Commits(HistoryGraphPage),
    Blame(BlameView),
    Comparison(ComparisonView),
    Recovery(RecoveryPage),
    Restored { summary: String },
}
#[derive(Clone, Debug, Default)]
pub struct HistoryGraphPage {
    pub anchor: Option<String>,
    pub commits: Vec<GraphCommit>,
    pub next_offset: usize,
    pub has_more: bool,
    pub truncated: bool,
    pub scanned_commits: usize,
}
#[derive(Clone, Debug)]
pub struct GraphCommit {
    pub id: String,
    pub parents: Vec<String>,
    pub subject: String,
    pub author: String,
    pub time: i64,
}
#[derive(Clone, Debug)]
pub struct BlameView {
    pub path: String,
    pub revision: String,
    pub lines: Vec<BlameLine>,
    pub encoding: TextEncoding,
}
#[derive(Clone, Debug)]
pub struct BlameLine {
    pub line_number: usize,
    pub original_line_number: usize,
    pub commit_id: Option<String>,
    pub author: String,
    pub text: String,
}
#[derive(Clone, Debug)]
pub struct ComparisonView {
    pub left: String,
    pub right: String,
    pub files: Vec<ComparedFile>,
    pub insertions: usize,
    pub deletions: usize,
    pub diff: Option<FileDiff>,
    pub truncated: bool,
}
#[derive(Clone, Debug)]
pub struct ComparedFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: char,
}
#[derive(Clone, Debug, Default)]
pub struct RecoveryPage {
    pub entries: Vec<RecoveryEntry>,
    pub has_more: bool,
    pub truncated: bool,
}
#[derive(Clone, Debug)]
pub struct RecoveryEntry {
    pub commit_id: String,
    pub label: String,
    pub source_reference: String,
    pub time: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryToolsErrorKind {
    Unsupported,
    InProgress,
    Cancelled,
    InvalidRepository,
    InvalidRequest,
    NotFound,
    BinaryFile,
    UnsupportedEncoding,
    Limit,
    ConcurrentChange,
    Failed,
}
#[derive(Clone, Debug)]
pub struct HistoryToolsError {
    pub kind: HistoryToolsErrorKind,
    pub message: String,
}
impl HistoryToolsError {
    pub fn new(kind: HistoryToolsErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}
impl fmt::Display for HistoryToolsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl Error for HistoryToolsError {}

pub(super) fn execute(
    root: &Path,
    request: &HistoryToolsRequest,
    cancel: &AtomicBool,
) -> Result<HistoryToolsResult, HistoryToolsError> {
    cancelled(cancel)?;
    let canonical = fs::canonicalize(root).map_err(|_| invalid_repository())?;
    let repo = Repository::open(&canonical).map_err(|_| invalid_repository())?;
    if repo.is_bare()
        || repo
            .workdir()
            .and_then(|p| fs::canonicalize(p).ok())
            .as_ref()
            != Some(&canonical)
    {
        return Err(invalid_repository());
    }
    let started = Instant::now();
    let result = match request {
        HistoryToolsRequest::Commits {
            anchor,
            offset,
            limit,
            path,
        } => HistoryToolsResult::Commits(commits(
            &repo,
            anchor.as_deref(),
            *offset,
            *limit,
            path.as_deref(),
            cancel,
            started,
        )?),
        HistoryToolsRequest::Blame { path, revision } => {
            HistoryToolsResult::Blame(blame(&repo, &canonical, path, revision.as_deref(), cancel)?)
        }
        HistoryToolsRequest::Compare { left, right, path } => HistoryToolsResult::Comparison(
            compare(&repo, left, right, path.as_deref(), cancel, started)?,
        ),
        HistoryToolsRequest::RecoveryList { offset, limit } => {
            HistoryToolsResult::Recovery(recovery(&repo, *offset, *limit, cancel, started)?)
        }
        HistoryToolsRequest::Restore {
            name,
            commit_id,
            expected_head,
        } => {
            if commit_id.len() != 40 || Oid::from_str(commit_id).is_err() {
                return Err(invalid_request("恢复目标必须是完整的提交 ID。"));
            }
            resolve(&repo, commit_id)?;
            cancelled(cancel)?;
            let outcome = crate::branch_manage::execute(
                &canonical,
                &crate::branch_manage::BranchRequest {
                    expected_head: expected_head.clone(),
                    action: crate::branch_manage::BranchAction::Create {
                        name: name.clone(),
                        start_point: Some(commit_id.clone()),
                    },
                },
            )
            .map_err(|error| {
                HistoryToolsError::new(
                    match error.kind {
                        crate::branch_manage::BranchErrorKind::InProgress => {
                            HistoryToolsErrorKind::InProgress
                        }
                        crate::branch_manage::BranchErrorKind::ConcurrentChange => {
                            HistoryToolsErrorKind::ConcurrentChange
                        }
                        _ => HistoryToolsErrorKind::Failed,
                    },
                    error.message,
                )
            })?;
            HistoryToolsResult::Restored {
                summary: format!(
                    "已将提交恢复为新分支 {name}，当前分支和工作文件保持不变。{}",
                    outcome.summary
                ),
            }
        }
    };
    if !request.is_mutation() {
        cancelled(cancel)?;
    }
    Ok(result)
}

fn commits(
    repo: &Repository,
    anchor: Option<&str>,
    offset: usize,
    limit: usize,
    file: Option<&str>,
    cancel: &AtomicBool,
    started: Instant,
) -> Result<HistoryGraphPage, HistoryToolsError> {
    page_limits(offset, limit)?;
    if let Some(file) = file {
        validate_path(file)?;
    }
    let head = if let Some(anchor) = anchor {
        if anchor.len() != 40 || Oid::from_str(anchor).is_err() {
            return Err(invalid_request("历史分页锚点必须是完整提交 ID。"));
        }
        resolve(repo, anchor)?.id()
    } else {
        match repo.head() {
            Ok(head) => head.peel_to_commit().map_err(map_git)?.id(),
            Err(error) if error.code() == ErrorCode::UnbornBranch => {
                return Ok(HistoryGraphPage::default());
            }
            Err(error) => return Err(map_git(error)),
        }
    };
    let mut walk = repo.revwalk().map_err(map_git)?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)
        .map_err(map_git)?;
    walk.push(head).map_err(map_git)?;
    let mut result = HistoryGraphPage {
        anchor: Some(head.to_string()),
        next_offset: offset,
        ..HistoryGraphPage::default()
    };
    for _ in 0..offset {
        cancelled(cancel)?;
        if walk.next().transpose().map_err(map_git)?.is_none() {
            return Ok(result);
        }
        if started.elapsed() > TIME_BUDGET {
            return Err(limit_error("读取历史偏移超出时间预算，请缩小范围后重试。"));
        }
    }
    let mut metadata = 0usize;
    while result.commits.len() < limit {
        cancelled(cancel)?;
        if result.scanned_commits >= MAX_SCAN
            || result.next_offset >= MAX_OFFSET
            || started.elapsed() > TIME_BUDGET
        {
            result.truncated = true;
            result.has_more = result.next_offset < MAX_OFFSET;
            break;
        }
        let Some(oid) = walk.next().transpose().map_err(map_git)? else {
            return Ok(result);
        };
        let commit = checked_commit(repo, oid)?;
        result.next_offset += 1;
        result.scanned_commits += 1;
        if let Some(file) = file {
            let tree = commit.tree().map_err(map_git)?;
            let id = tree
                .get_path(Path::new(file))
                .ok()
                .map(|entry| (entry.id(), entry.filemode()));
            let touches = if commit.parent_count() == 0 {
                id.is_some()
            } else {
                let mut differs = false;
                // Follow every merge parent: a merge-only resolution remains visible.
                for parent in commit.parents() {
                    cancelled(cancel)?;
                    let parent_tree = parent.tree().map_err(map_git)?;
                    let before = parent_tree
                        .get_path(Path::new(file))
                        .ok()
                        .map(|entry| (entry.id(), entry.filemode()));
                    differs |= id != before;
                }
                differs
            };
            if !touches {
                continue;
            }
        }
        if commit.parent_count() > 128 {
            return Err(limit_error("单个提交的父提交数量超过 128 个拓扑通道预算。"));
        }
        let author = truncate_text(&String::from_utf8_lossy(commit.author().name_bytes()), 512);
        let subject = truncate_text(
            &String::from_utf8_lossy(commit.summary_bytes().unwrap_or_default()),
            2_048,
        );
        let parents = commit
            .parent_ids()
            .map(|id| id.to_string())
            .collect::<Vec<_>>();
        metadata = metadata.saturating_add(author.len() + subject.len() + parents.len() * 40 + 40);
        if metadata > MAX_METADATA {
            result.next_offset -= 1;
            result.truncated = true;
            result.has_more = true;
            break;
        }
        result.commits.push(GraphCommit {
            id: oid.to_string(),
            parents,
            subject,
            author,
            time: commit.time().seconds(),
        });
    }
    if !result.truncated {
        cancelled(cancel)?;
        result.has_more = walk.next().transpose().map_err(map_git)?.is_some();
    }
    Ok(result)
}

fn blame(
    repo: &Repository,
    root: &Path,
    file: &str,
    revision: Option<&str>,
    cancel: &AtomicBool,
) -> Result<BlameView, HistoryToolsError> {
    validate_path(file)?;
    let anchor = resolve(repo, revision.unwrap_or("HEAD"))?;
    let anchor_id = anchor.id().to_string();
    let committed = source::read(root, file, Some(&anchor_id)).map_err(map_source)?;
    if !matches!(
        committed.encoding,
        TextEncoding::Utf8 | TextEncoding::Utf8Bom
    ) {
        return Err(HistoryToolsError::new(
            HistoryToolsErrorKind::UnsupportedEncoding,
            "UTF-16 文件可查看源文件和版本比较；Git 的逐行归属仅支持 UTF-8 文本。",
        ));
    }
    let current = if revision.is_none() {
        require_regular_working_path(root, file)?;
        source::read(root, file, None).map_err(map_source)?
    } else {
        committed.clone()
    };
    if !matches!(current.encoding, TextEncoding::Utf8 | TextEncoding::Utf8Bom) {
        return Err(HistoryToolsError::new(
            HistoryToolsErrorKind::UnsupportedEncoding,
            "当前工作文件使用 UTF-16，无法执行 Git 逐行归属。",
        ));
    }
    cancelled(cancel)?;
    let mut options = BlameOptions::new();
    options.newest_commit(anchor.id());
    let original = repo
        .blame_file(Path::new(file), Some(&mut options))
        .map_err(map_git)?;
    cancelled(cancel)?;
    // Keep the original BOM for libgit2 line comparison, while displaying decoded text.
    let mut buffer = Vec::new();
    if current.encoding == TextEncoding::Utf8Bom {
        buffer.extend_from_slice(&[0xef, 0xbb, 0xbf]);
    }
    buffer.extend_from_slice(current.text.as_bytes());
    let working = if revision.is_none() {
        Some(original.blame_buffer(&buffer).map_err(map_git)?)
    } else {
        None
    };
    let attribution = working.as_ref().unwrap_or(&original);
    let mut lines = Vec::new();
    for (index, text) in current.text.split_inclusive('\n').enumerate() {
        cancelled(cancel)?;
        let number = index + 1;
        let hunk = attribution.get_line(number);
        let commit_id = hunk
            .as_ref()
            .map(|h| h.final_commit_id())
            .filter(|id| !id.is_zero())
            .map(|id| id.to_string());
        let author = if commit_id.is_none() {
            "未提交".into()
        } else {
            hunk.as_ref()
                .and_then(|h| h.final_signature())
                .map(|signature| String::from_utf8_lossy(signature.name_bytes()).into_owned())
                .unwrap_or_default()
        };
        let original_line_number = hunk.as_ref().map_or(number, |h| {
            h.orig_start_line() + number.saturating_sub(h.final_start_line())
        });
        lines.push(BlameLine {
            line_number: number,
            original_line_number,
            commit_id,
            author,
            text: text.into(),
        });
    }
    Ok(BlameView {
        path: file.into(),
        revision: if revision.is_none() {
            format!("工作文件（基于 {}）", &anchor_id[..8])
        } else {
            anchor_id
        },
        lines,
        encoding: current.encoding,
    })
}

fn compare(
    repo: &Repository,
    left: &str,
    right: &str,
    selected: Option<&str>,
    cancel: &AtomicBool,
    started: Instant,
) -> Result<ComparisonView, HistoryToolsError> {
    if let Some(selected) = selected {
        validate_path(selected)?;
    }
    // Empty-tree comparison is deliberately read-only and local to this API.
    // The token is never accepted as a revision by blame or branch recovery.
    let left = (left != ":empty")
        .then(|| resolve(repo, left))
        .transpose()?;
    let right = (right != ":empty")
        .then(|| resolve(repo, right))
        .transpose()?;
    let old = left
        .as_ref()
        .map(|commit| commit.tree())
        .transpose()
        .map_err(map_git)?;
    let new = right
        .as_ref()
        .map(|commit| commit.tree())
        .transpose()
        .map_err(map_git)?;
    let mut options = DiffOptions::new();
    options.context_lines(3).include_typechange(true);
    let mut comparison = repo
        .diff_tree_to_tree(old.as_ref(), new.as_ref(), Some(&mut options))
        .map_err(map_git)?;
    cancelled(cancel)?;
    // Similarity needs blob reads; only enable it within a small aggregate budget.
    let mut candidate_bytes = 0usize;
    for delta in comparison.deltas() {
        cancelled(cancel)?;
        for file in [delta.old_file(), delta.new_file()] {
            if !file.id().is_zero()
                && matches!(u32::from(file.mode()) & 0o170000, 0o100000 | 0o120000)
            {
                let (size, _) = repo
                    .odb()
                    .and_then(|odb| odb.read_header(file.id()))
                    .map_err(map_git)?;
                candidate_bytes = candidate_bytes.saturating_add(size);
            }
        }
        if candidate_bytes > MAX_STATS_BYTES || started.elapsed() > TIME_BUDGET {
            break;
        }
    }
    let statistics_complete = comparison.deltas().len() <= 200
        && candidate_bytes <= MAX_STATS_BYTES
        && started.elapsed() <= TIME_BUDGET;
    if statistics_complete {
        comparison
            .find_similar(Some(DiffFindOptions::new().renames(true).rename_limit(200)))
            .map_err(map_git)?;
    }
    let mut files = Vec::new();
    let mut metadata = 0usize;
    let mut truncated = !statistics_complete;
    for delta in comparison.deltas() {
        cancelled(cancel)?;
        let path = if delta.status() == Delta::Deleted {
            delta.old_file().path()
        } else {
            delta.new_file().path()
        }
        .ok_or_else(|| invalid_request("比较包含无法显示的文件路径。"))?
        .to_str()
        .ok_or_else(|| invalid_request("比较包含非 UTF-8 文件路径。"))?
        .to_owned();
        let old_path = delta
            .old_file()
            .path()
            .and_then(Path::to_str)
            .filter(|p| *p != path)
            .map(str::to_owned);
        metadata = metadata.saturating_add(path.len() + old_path.as_ref().map_or(0, String::len));
        if files.len() >= MAX_FILES || metadata > MAX_METADATA || started.elapsed() > TIME_BUDGET {
            truncated = true;
            break;
        }
        files.push(ComparedFile {
            path,
            old_path,
            status: match delta.status() {
                Delta::Added => 'A',
                Delta::Deleted => 'D',
                Delta::Renamed => 'R',
                Delta::Copied => 'C',
                Delta::Typechange => 'T',
                _ => 'M',
            },
        });
    }
    let (mut insertions, mut deletions) = (0usize, 0usize);
    if statistics_complete {
        for delta in comparison.deltas() {
            cancelled(cancel)?;
            if started.elapsed() > TIME_BUDGET {
                truncated = true;
                break;
            }
            let old_path = delta.old_file().path().unwrap_or(Path::new(""));
            let new_path = delta.new_file().path().unwrap_or(Path::new(""));
            let contents = tree_text(repo, old.as_ref(), old_path).and_then(|a| {
                tree_text(repo, new.as_ref(), new_path)
                    .map(|b| (a.unwrap_or_default(), b.unwrap_or_default()))
            });
            let (old_text, new_text) = match contents {
                Ok(contents) => contents,
                Err(_) => {
                    truncated = true;
                    continue;
                }
            };
            let patch = Patch::from_buffers(
                old_text.as_bytes(),
                Some(old_path),
                new_text.as_bytes(),
                Some(new_path),
                None,
            )
            .map_err(map_git)?;
            let (_, added, removed) = patch.line_stats().map_err(map_git)?;
            insertions = insertions.saturating_add(added);
            deletions = deletions.saturating_add(removed);
        }
    }
    let selected_diff = selected
        .map(|file| compared_diff(repo, old.as_ref(), new.as_ref(), &comparison, file, cancel))
        .transpose()?;
    Ok(ComparisonView {
        left: left
            .as_ref()
            .map_or_else(|| ":empty".into(), |commit| commit.id().to_string()),
        right: right
            .as_ref()
            .map_or_else(|| ":empty".into(), |commit| commit.id().to_string()),
        files,
        insertions,
        deletions,
        diff: selected_diff,
        truncated,
    })
}

fn compared_diff(
    repo: &Repository,
    old: Option<&git2::Tree<'_>>,
    new: Option<&git2::Tree<'_>>,
    comparison: &git2::Diff<'_>,
    file: &str,
    cancel: &AtomicBool,
) -> Result<FileDiff, HistoryToolsError> {
    let delta = comparison.deltas().find(|delta| {
        delta.new_file().path() == Some(Path::new(file))
            || delta.old_file().path() == Some(Path::new(file))
    });
    let old_path = delta
        .as_ref()
        .and_then(|d| d.old_file().path())
        .unwrap_or(Path::new(file));
    let new_path = delta
        .as_ref()
        .and_then(|d| d.new_file().path())
        .unwrap_or(Path::new(file));
    let old_text = tree_text(repo, old, old_path)?;
    let new_text = tree_text(repo, new, new_path)?;
    if old_text.is_none() && new_text.is_none() {
        return Err(HistoryToolsError::new(
            HistoryToolsErrorKind::NotFound,
            "所选文件在两个版本中均不存在。",
        ));
    }
    let old_text = old_text.unwrap_or_default();
    let new_text = new_text.unwrap_or_default();
    cancelled(cancel)?;
    let mut options = DiffOptions::new();
    options.context_lines(3);
    let mut patch = Patch::from_buffers(
        old_text.as_bytes(),
        Some(old_path),
        new_text.as_bytes(),
        Some(new_path),
        Some(&mut options),
    )
    .map_err(map_git)?;
    if patch.size(true, true, true) > diff::MAX_PATCH_BYTES {
        return Err(limit_error("单文件补丁超过 16 MiB 预览限制。"));
    }
    let mut bytes = Vec::new();
    let mut line_breaks = 0usize;
    let mut stopped = false;
    patch
        .print(&mut |_, _, line| {
            let prefix = matches!(line.origin(), '+' | '-' | ' ');
            let content = line.content();
            line_breaks += content.iter().filter(|&&b| b == b'\n').count();
            if cancel.load(Ordering::Relaxed)
                || bytes
                    .len()
                    .saturating_add(content.len() + usize::from(prefix))
                    > diff::MAX_PATCH_BYTES
                || line_breaks >= diff::MAX_DIFF_LINES
            {
                stopped = true;
                return false;
            }
            if prefix {
                bytes.push(line.origin() as u8);
            }
            bytes.extend_from_slice(content);
            true
        })
        .map_err(|error| {
            if cancel.load(Ordering::Relaxed) {
                cancelled_error()
            } else if stopped {
                limit_error("补丁超过字节或行数限制。")
            } else {
                map_git(error)
            }
        })?;
    cancelled(cancel)?;
    drop(patch);
    let patch = String::from_utf8(bytes).map_err(|_| binary_error())?;
    let lines = diff::parse_lines(&patch).map_err(map_source)?;
    Ok(FileDiff {
        path: file.into(),
        old_text,
        new_text,
        patch,
        lines,
    })
}

fn tree_text(
    repo: &Repository,
    tree: Option<&git2::Tree<'_>>,
    path: &Path,
) -> Result<Option<String>, HistoryToolsError> {
    let Some(tree) = tree else {
        return Ok(None);
    };
    let entry = match tree.get_path(path) {
        Ok(entry) => entry,
        Err(error) if error.code() == ErrorCode::NotFound => return Ok(None),
        Err(error) => return Err(map_git(error)),
    };
    if !matches!(entry.filemode() & 0o170000, 0o100000 | 0o120000) {
        return Err(invalid_request(
            "请选择普通文件；目录和子模块需进入对应仓库查看。",
        ));
    }
    let (size, _) = repo
        .odb()
        .and_then(|odb| odb.read_header(entry.id()))
        .map_err(map_git)?;
    if size > diff::MAX_TEXT_BYTES {
        return Err(limit_error("单个版本文件超过 2 MiB 差异预览限制。"));
    }
    let blob = repo.find_blob(entry.id()).map_err(map_git)?;
    let (text, _) = source::decode(blob.content(), &path.to_string_lossy()).map_err(map_source)?;
    if text.len() > diff::MAX_TEXT_BYTES {
        return Err(limit_error("解码后的文件超过 2 MiB 差异预览限制。"));
    }
    source::validate_lines(&text, &path.to_string_lossy()).map_err(map_source)?;
    Ok(Some(text))
}

fn recovery(
    repo: &Repository,
    offset: usize,
    limit: usize,
    cancel: &AtomicBool,
    started: Instant,
) -> Result<RecoveryPage, HistoryToolsError> {
    page_limits(offset, limit)?;
    let mut entries = Vec::new();
    let mut seen = BTreeSet::new();
    let mut truncated = false;
    let mut metadata_bytes = 0usize;
    let mut refs = Vec::new();
    for reference in repo
        .references_glob("refs/gitviews/deleted-branches/*")
        .map_err(map_git)?
    {
        cancelled(cancel)?;
        if refs.len() >= 2_000 {
            truncated = true;
            break;
        }
        let reference = reference.map_err(map_git)?;
        let name = reference.name().map_err(map_git)?.to_owned();
        if let Some(oid) = reference
            .resolve()
            .ok()
            .and_then(|reference| reference.target())
            && let Ok(commit) = checked_commit(repo, oid)
        {
            let message = last_log_message(&repo.commondir().join("logs").join(&name));
            let label = message.unwrap_or_else(|| "保留的分支提交".into());
            metadata_bytes = metadata_bytes.saturating_add(label.len() + name.len() + 40);
            if metadata_bytes > MAX_METADATA {
                truncated = true;
                break;
            }
            entries.push(RecoveryEntry {
                commit_id: commit.id().to_string(),
                label,
                source_reference: name.clone(),
                time: commit.time().seconds(),
            });
            seen.insert(commit.id());
        }
        refs.push(name);
    }
    refs.push("HEAD".into());
    for reference in repo.references_glob("refs/heads/*").map_err(map_git)? {
        if refs.len() >= 2_200 {
            truncated = true;
            break;
        }
        refs.push(
            reference
                .map_err(map_git)?
                .name()
                .map_err(map_git)?
                .to_owned(),
        );
    }
    let mut log_bytes = 0u64;
    let mut scanned = 0usize;
    for reference in refs {
        cancelled(cancel)?;
        let log_path = if reference == "HEAD" {
            repo.path().join("logs/HEAD")
        } else {
            repo.commondir().join("logs").join(&reference)
        };
        let metadata = match fs::symlink_metadata(&log_path) {
            Ok(meta) if meta.is_file() && !native::is_link(&meta) => meta,
            Ok(_) => {
                truncated = true;
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                truncated = true;
                continue;
            }
        };
        log_bytes = log_bytes.saturating_add(metadata.len());
        if log_bytes > MAX_LOG_BYTES || started.elapsed() > TIME_BUDGET {
            truncated = true;
            break;
        }
        let log = repo.reflog(&reference).map_err(map_git)?;
        for (index, entry) in log.iter().enumerate() {
            cancelled(cancel)?;
            scanned += 1;
            if scanned > 20_000 || started.elapsed() > TIME_BUDGET {
                truncated = true;
                break;
            }
            for oid in [entry.id_new(), entry.id_old()] {
                if oid.is_zero() || seen.contains(&oid) {
                    continue;
                }
                if let Ok(commit) = checked_commit(repo, oid) {
                    let message = entry.message().ok().flatten().unwrap_or("历史提交");
                    let label = truncate_text(message, 2_048);
                    metadata_bytes =
                        metadata_bytes.saturating_add(label.len() + reference.len() + 64);
                    if metadata_bytes > MAX_METADATA {
                        truncated = true;
                        break;
                    }
                    entries.push(RecoveryEntry {
                        commit_id: oid.to_string(),
                        label,
                        source_reference: format!("{reference}@{{{index}}}"),
                        time: entry.committer().when().seconds(),
                    });
                    seen.insert(commit.id());
                }
            }
        }
        if scanned > 20_000 || metadata_bytes > MAX_METADATA {
            break;
        }
    }
    entries.sort_by(|a, b| {
        b.time
            .cmp(&a.time)
            .then_with(|| a.commit_id.cmp(&b.commit_id))
    });
    let has_more = entries.len() > offset.saturating_add(limit);
    let entries = entries.into_iter().skip(offset).take(limit).collect();
    Ok(RecoveryPage {
        entries,
        has_more,
        truncated,
    })
}

fn last_log_message(path: &Path) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || native::is_link(&metadata) {
        return None;
    }
    let mut file = fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(metadata.len().saturating_sub(8_192)))
        .ok()?;
    let mut bytes = Vec::new();
    file.take(8_192).read_to_end(&mut bytes).ok()?;
    let line = bytes
        .rsplit(|b| *b == b'\n')
        .find(|line| !line.is_empty())?;
    let message = line.rsplit(|b| *b == b'\t').next()?;
    Some(truncate_text(&String::from_utf8_lossy(message), 2_048))
}

fn require_regular_working_path(root: &Path, file: &str) -> Result<(), HistoryToolsError> {
    let mut path = root.to_path_buf();
    let components = file.split('/').collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        path.push(component);
        let metadata = fs::symlink_metadata(&path).map_err(|_| {
            HistoryToolsError::new(
                HistoryToolsErrorKind::NotFound,
                "工作文件不存在或不可读取。",
            )
        })?;
        if native::is_link(&metadata) || index + 1 < components.len() && path.join(".git").exists()
        {
            return Err(invalid_request(
                "逐行归属不读取符号链接、重解析点或其他仓库内部的文件。",
            ));
        }
        if index + 1 == components.len() && !metadata.is_file() {
            return Err(invalid_request("请选择普通工作文件。"));
        }
    }
    Ok(())
}
fn validate_path(path: &str) -> Result<(), HistoryToolsError> {
    if path.split('/').any(|part| {
        part.is_empty()
            || part == "."
            || part == ".."
            || part
                .trim_end_matches([' ', '.'])
                .eq_ignore_ascii_case(".git")
    }) {
        return Err(invalid_request(
            "请填写不含空段、上级目录或 Git 元数据目录的仓库相对路径。",
        ));
    }
    diff::validate_path(path).map_err(map_source)
}
fn resolve<'a>(
    repo: &'a Repository,
    revision: &str,
) -> Result<git2::Commit<'a>, HistoryToolsError> {
    if revision.trim() != revision
        || revision.is_empty()
        || revision.len() > 1_024
        || revision.contains('\0')
        || revision.starts_with('-')
    {
        return Err(invalid_request("请填写有效的分支、标签或提交 ID。"));
    }
    // Ref resolution is metadata-only. Check annotated-tag headers before
    // revparse can allocate their contents; normal branch/tag/hash syntax is
    // still handled by the shared revision parser below.
    let base = revision
        .split(['^', '~', '@', ':'])
        .next()
        .unwrap_or(revision);
    let oid = repo
        .resolve_reference_from_short_name(base)
        .ok()
        .and_then(|reference| reference.resolve().ok())
        .and_then(|reference| reference.target())
        .or_else(|| {
            Oid::from_str(base)
                .ok()
                .and_then(|id| repo.odb().ok()?.exists_prefix(id, base.len()).ok())
        });
    if let Some(mut oid) = oid {
        for depth in 0..32 {
            let (size, kind) = repo
                .odb()
                .and_then(|odb| odb.read_header(oid))
                .map_err(map_git)?;
            if size > 1024 * 1024 {
                return Err(limit_error(
                    "版本引用的提交或标签对象超过 1 MiB 元数据预算。",
                ));
            }
            if kind != git2::ObjectType::Tag {
                break;
            }
            if depth == 31 {
                return Err(limit_error("标签引用链超过 32 层预算。"));
            }
            oid = repo.find_tag(oid).map_err(map_git)?.target_id();
        }
    }
    history::resolve_commit(repo, revision).map_err(map_source)
}
fn checked_commit(repo: &Repository, oid: Oid) -> Result<git2::Commit<'_>, HistoryToolsError> {
    let (size, _) = repo
        .odb()
        .and_then(|odb| odb.read_header(oid))
        .map_err(map_git)?;
    if size > 1024 * 1024 {
        return Err(limit_error("提交对象超过 1 MiB 元数据预算。"));
    }
    repo.find_commit(oid).map_err(map_git)
}
fn truncate_text(text: &str, limit: usize) -> String {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].into()
}
fn page_limits(offset: usize, limit: usize) -> Result<(), HistoryToolsError> {
    if !(1..=200).contains(&limit) || offset > MAX_OFFSET {
        return Err(invalid_request(
            "每页限制为 1–200 项，历史扫描偏移最多 50000 项。",
        ));
    }
    Ok(())
}
fn cancelled(cancel: &AtomicBool) -> Result<(), HistoryToolsError> {
    if cancel.load(Ordering::Relaxed) {
        Err(cancelled_error())
    } else {
        Ok(())
    }
}
fn cancelled_error() -> HistoryToolsError {
    HistoryToolsError::new(HistoryToolsErrorKind::Cancelled, "历史分析已取消。")
}
fn invalid_repository() -> HistoryToolsError {
    HistoryToolsError::new(
        HistoryToolsErrorKind::InvalidRepository,
        "请选择可用的非裸仓库根目录。",
    )
}
fn invalid_request(message: &str) -> HistoryToolsError {
    HistoryToolsError::new(HistoryToolsErrorKind::InvalidRequest, message)
}
fn limit_error(message: &str) -> HistoryToolsError {
    HistoryToolsError::new(HistoryToolsErrorKind::Limit, message)
}
fn binary_error() -> HistoryToolsError {
    HistoryToolsError::new(
        HistoryToolsErrorKind::BinaryFile,
        "二进制或非 UTF-8/UTF-16 文本不能显示逐行差异。",
    )
}
fn map_source(error: crate::GitError) -> HistoryToolsError {
    let kind = match error {
        crate::GitError::BinaryFile { .. } | crate::GitError::InvalidEncoding { .. } => {
            HistoryToolsErrorKind::BinaryFile
        }
        crate::GitError::FileTooLarge { .. }
        | crate::GitError::LineCountLimit { .. }
        | crate::GitError::LineTooLong { .. } => HistoryToolsErrorKind::Limit,
        crate::GitError::InvalidPath { .. } | crate::GitError::InvalidRequest { .. } => {
            HistoryToolsErrorKind::InvalidRequest
        }
        _ => HistoryToolsErrorKind::Failed,
    };
    HistoryToolsError::new(kind, error.to_string())
}
fn map_git(error: git2::Error) -> HistoryToolsError {
    HistoryToolsError::new(
        if error.code() == ErrorCode::NotFound {
            HistoryToolsErrorKind::NotFound
        } else {
            HistoryToolsErrorKind::Failed
        },
        format!("读取 Git 历史失败：{}", error.message()),
    )
}
