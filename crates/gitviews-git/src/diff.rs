use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::{GitError, checked_git, command_error, repository_root, run_git};

pub(super) const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
pub(super) const MAX_PATCH_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_DIFF_LINES: usize = 100_000;

/// A UTF-8 text comparison ready for unified or side-by-side rendering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileDiff {
    /// Repository-relative selected file path.
    pub path: String,
    /// HEAD content for staged diffs, or index content for unstaged diffs.
    pub old_text: String,
    /// Index content for staged diffs, or working-file content otherwise.
    pub new_text: String,
    /// Unified Git patch, including hunk and file headers.
    pub patch: String,
    /// Hunk lines with independent old/new line numbers.
    pub lines: Vec<DiffLine>,
}

/// A hunk row; adjacent deletion/addition runs can be paired by a two-column UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffLine {
    /// One-based old line number, absent for additions and section markers.
    pub old_line: Option<usize>,
    /// One-based new line number, absent for deletions and section markers.
    pub new_line: Option<usize>,
    /// The semantic role of this row.
    pub kind: DiffLineKind,
    /// Line text without its patch prefix or line ending.
    pub text: String,
}

/// Styling and alignment role for a diff row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffLineKind {
    /// Hunk heading or a missing-final-newline marker.
    Section,
    /// Line present only in the new version.
    Added,
    /// Line present only in the old version.
    Deleted,
    /// Unchanged context shared by both versions.
    Context,
}

pub(super) fn read(path: &Path, file: &str, staged: bool) -> Result<FileDiff, GitError> {
    validate_path(file)?;
    let root = repository_root(path)?;
    let working_path = checked_working_path(&root, file)?;
    let original = if staged {
        original_path(&root, file)?
    } else {
        None
    };
    let old_path = original.as_deref().unwrap_or(file);
    let old = if staged {
        read_head(&root, old_path)?
    } else {
        read_index(&root, file)?
    };
    let new = if staged {
        read_index(&root, file)?
    } else {
        read_working(&working_path, file)?
    };
    if old.is_none() && new.is_none() {
        return Err(GitError::InvalidPath {
            path: file.to_owned(),
            reason: "the file is absent from this comparison",
        });
    }
    let untracked = !staged && old.is_none();
    let old_text = old.unwrap_or_default();
    let new_text = new.unwrap_or_default();
    let patch = if untracked {
        added_patch(file, &new_text)?
    } else {
        let mut args = vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-relative",
            "--unified=3",
            "--find-renames",
        ];
        if staged {
            args.push("--cached");
        }
        args.extend(["--", file]);
        if let Some(original) = original.as_deref() {
            args.push(original);
        }
        let output = checked_git(&root, "read file diff", &args)?;
        let patch = String::from_utf8_lossy(&output.stdout).into_owned();
        if patch
            .lines()
            .any(|line| line.starts_with("Binary files ") || line == "GIT binary patch")
        {
            return Err(GitError::BinaryFile {
                path: file.to_owned(),
            });
        }
        patch
    };
    validate_patch(&patch, file)?;
    let lines = parse_lines(&patch)?;
    Ok(FileDiff {
        path: file.to_owned(),
        old_text,
        new_text,
        patch,
        lines,
    })
}

pub(super) fn validate_path(file: &str) -> Result<(), GitError> {
    if file.is_empty()
        || file.contains(['\0', '\\', ':'])
        || Path::new(file)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || file
            .split('/')
            .any(|component| component.eq_ignore_ascii_case(".git"))
    {
        return Err(GitError::InvalidPath {
            path: file.to_owned(),
            reason: "use a repository-relative file path without parent traversal",
        });
    }
    Ok(())
}

pub(super) fn checked_working_path(root: &Path, file: &str) -> Result<PathBuf, GitError> {
    let root = fs::canonicalize(root).map_err(|source| GitError::Io {
        operation: "resolve repository path",
        source,
    })?;
    let mut candidate = root.clone();
    let components: Vec<_> = Path::new(file).components().collect();
    for (index, component) in components.iter().enumerate() {
        candidate.push(component.as_os_str());
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                // A final symlink is previewed as its link text, never followed.
                if metadata.file_type().is_symlink() && index + 1 == components.len() {
                    break;
                }
                candidate = fs::canonicalize(&candidate).map_err(|source| GitError::Io {
                    operation: "resolve diff file",
                    source,
                })?;
                if !candidate.starts_with(&root) {
                    return Err(GitError::InvalidPath {
                        path: file.to_owned(),
                        reason: "the path resolves outside the repository",
                    });
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                for remaining in &components[index + 1..] {
                    candidate.push(remaining.as_os_str());
                }
                break;
            }
            Err(source) => {
                return Err(GitError::Io {
                    operation: "resolve diff file",
                    source,
                });
            }
        }
    }
    Ok(candidate)
}

pub(super) fn read_working(path: &Path, file: &str) -> Result<Option<String>, GitError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(GitError::Io {
                operation: "read working file",
                source,
            });
        }
    };
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(|source| GitError::Io {
            operation: "read symbolic link",
            source,
        })?;
        return Ok(Some(target.to_string_lossy().into_owned()));
    }
    if !metadata.is_file() {
        return Err(GitError::InvalidPath {
            path: file.to_owned(),
            reason: "only ordinary files can be previewed",
        });
    }
    check_size(metadata.len(), file)?;
    let reader = File::open(path).map_err(|source| GitError::Io {
        operation: "read working file",
        source,
    })?;
    let mut bytes = Vec::new();
    reader
        .take((MAX_TEXT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| GitError::Io {
            operation: "read working file",
            source,
        })?;
    text(bytes, file).map(Some)
}

fn read_index(root: &Path, file: &str) -> Result<Option<String>, GitError> {
    let output = checked_git(
        root,
        "locate index file",
        &["ls-files", "--stage", "-z", "--", file],
    )?;
    if output.stdout.is_empty() {
        return Ok(None);
    }
    let header = output
        .stdout
        .split(|byte| *byte == b'\t')
        .next()
        .unwrap_or_default();
    let fields: Vec<_> = header.split(|byte| *byte == b' ').collect();
    if fields.len() != 3 || fields[2] != b"0" {
        return Err(GitError::Unsupported {
            operation: "diff of an unmerged index",
        });
    }
    if fields[0] == b"160000" {
        return Err(GitError::Unsupported {
            operation: "submodule diff",
        });
    }
    read_blob(root, &format!(":{file}"), file).map(Some)
}

fn read_head(root: &Path, file: &str) -> Result<Option<String>, GitError> {
    let head = run_git(
        root,
        "read diff base",
        &["rev-parse", "--verify", "--quiet", "HEAD"],
    )?;
    if !head.status.success() {
        if head.status.code() == Some(1) {
            return Ok(None);
        }
        return Err(command_error("read diff base", head));
    }
    let output = checked_git(
        root,
        "locate HEAD file",
        &["ls-tree", "-z", "HEAD", "--", file],
    )?;
    if output.stdout.is_empty() {
        return Ok(None);
    }
    if output.stdout.starts_with(b"160000 ") {
        return Err(GitError::Unsupported {
            operation: "submodule diff",
        });
    }
    read_blob(root, &format!("HEAD:{file}"), file).map(Some)
}

fn read_blob(root: &Path, object: &str, file: &str) -> Result<String, GitError> {
    let output = checked_git(root, "measure diff content", &["cat-file", "-s", object])?;
    let size = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .map_err(|_| GitError::InvalidOutput {
            operation: "measure diff content",
            details: "invalid object size",
        })?;
    check_size(size, file)?;
    let output = checked_git(
        root,
        "read diff content",
        &["show", "--no-ext-diff", "--no-textconv", object],
    )?;
    text(output.stdout, file)
}

pub(super) fn check_size(size: u64, file: &str) -> Result<(), GitError> {
    if size > MAX_TEXT_BYTES as u64 {
        return Err(GitError::FileTooLarge {
            path: file.to_owned(),
            limit_bytes: MAX_TEXT_BYTES,
        });
    }
    Ok(())
}

pub(super) fn text(bytes: Vec<u8>, file: &str) -> Result<String, GitError> {
    check_size(bytes.len() as u64, file)?;
    if bytes.contains(&0) {
        return Err(GitError::BinaryFile {
            path: file.to_owned(),
        });
    }
    let text = String::from_utf8(bytes).map_err(|_| GitError::BinaryFile {
        path: file.to_owned(),
    })?;
    crate::source::validate_lines(&text, file)?;
    Ok(text)
}

fn original_path(root: &Path, file: &str) -> Result<Option<String>, GitError> {
    let output = checked_git(
        root,
        "read renamed paths",
        &[
            "diff",
            "--cached",
            "--no-ext-diff",
            "--no-textconv",
            "--name-status",
            "-z",
            "--find-renames",
            "--",
        ],
    )?;
    let mut fields = output.stdout.split(|byte| *byte == 0);
    while let Some(status) = fields.next().filter(|status| !status.is_empty()) {
        let Some(path) = fields.next() else {
            break;
        };
        if matches!(status.first(), Some(b'R' | b'C')) {
            let Some(destination) = fields.next() else {
                break;
            };
            if destination == file.as_bytes() {
                return Ok(Some(String::from_utf8_lossy(path).into_owned()));
            }
        }
    }
    Ok(None)
}

fn added_patch(file: &str, content: &str) -> Result<String, GitError> {
    if content.is_empty() {
        return Ok(String::new());
    }
    let count = content.lines().count();
    let mut patch = format!("--- /dev/null\n+++ b/{file}\n@@ -0,0 +1,{count} @@\n");
    check_patch_size(
        patch
            .len()
            .saturating_add(content.len())
            .saturating_add(count * 2)
            .saturating_add(32),
        file,
    )?;
    for line in content.lines() {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    if !content.ends_with('\n') {
        patch.push_str("\\ No newline at end of file\n");
    }
    validate_patch(&patch, file)?;
    Ok(patch)
}

pub(super) fn parse_lines(patch: &str) -> Result<Vec<DiffLine>, GitError> {
    // Defensive validation also protects custom backends and future callers
    // before allocating one String and DiffLine for every row.
    validate_patch(patch, "diff")?;
    let mut lines = Vec::new();
    let mut positions = None;
    for line in patch.lines() {
        if line.starts_with("@@ ") {
            let fields: Vec<_> = line.split_whitespace().collect();
            let position = |value: Option<&&str>| {
                value
                    .and_then(|value| value.get(1..))
                    .and_then(|value| value.split(',').next())
                    .and_then(|value| value.parse::<usize>().ok())
            };
            positions = position(fields.get(1)).zip(position(fields.get(2)));
            if positions.is_none() {
                return Err(GitError::InvalidOutput {
                    operation: "parse file diff",
                    details: "malformed hunk heading",
                });
            }
            lines.push(DiffLine {
                old_line: None,
                new_line: None,
                kind: DiffLineKind::Section,
                text: line.to_owned(),
            });
        } else if let Some((old, new)) = positions.as_mut() {
            let (old_line, new_line, kind) = match line.as_bytes().first() {
                Some(b'+') => {
                    let result = (None, Some(*new), DiffLineKind::Added);
                    *new += 1;
                    result
                }
                Some(b'-') => {
                    let result = (Some(*old), None, DiffLineKind::Deleted);
                    *old += 1;
                    result
                }
                Some(b' ') => {
                    let result = (Some(*old), Some(*new), DiffLineKind::Context);
                    *old += 1;
                    *new += 1;
                    result
                }
                Some(b'\\') => (None, None, DiffLineKind::Section),
                _ => continue,
            };
            lines.push(DiffLine {
                old_line,
                new_line,
                kind,
                text: if kind == DiffLineKind::Section {
                    line.to_owned()
                } else {
                    line[1..].to_owned()
                },
            });
        }
    }
    Ok(lines)
}

pub(super) fn check_patch_size(size: usize, file: &str) -> Result<(), GitError> {
    if size > MAX_PATCH_BYTES {
        return Err(GitError::FileTooLarge {
            path: file.to_owned(),
            limit_bytes: MAX_PATCH_BYTES,
        });
    }
    Ok(())
}

pub(super) fn validate_patch(patch: &str, file: &str) -> Result<(), GitError> {
    check_patch_size(patch.len(), file)?;
    if patch.lines().take(MAX_DIFF_LINES + 1).count() > MAX_DIFF_LINES {
        return Err(GitError::LineCountLimit {
            path: file.to_owned(),
            limit_lines: MAX_DIFF_LINES,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_limits_are_checked_before_allocating_parsed_rows() {
        assert!(matches!(
            check_patch_size(MAX_PATCH_BYTES + 1, "large.patch"),
            Err(GitError::FileTooLarge {
                limit_bytes: MAX_PATCH_BYTES,
                ..
            })
        ));
        assert!(matches!(
            parse_lines(&"+\n".repeat(MAX_DIFF_LINES + 1)),
            Err(GitError::LineCountLimit {
                limit_lines: MAX_DIFF_LINES,
                ..
            })
        ));
        assert!(validate_patch(&"\n".repeat(MAX_DIFF_LINES), "at-limit.patch").is_ok());
    }

    #[test]
    fn bounded_untracked_patch_preserves_lines_and_final_newline_marker() {
        let patch = added_patch("中文 文件.rs", "one\ntwo").unwrap();
        assert!(patch.contains("+++ b/中文 文件.rs\n"));
        let lines = parse_lines(&patch).unwrap();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.kind == DiffLineKind::Added)
                .count(),
            2
        );
        assert!(patch.ends_with("\\ No newline at end of file\n"));
        assert!(added_patch("empty.txt", "").unwrap().is_empty());
    }
}
