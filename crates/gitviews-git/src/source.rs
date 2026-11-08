//! Bounded, encoding-aware source reading independent of repository diffs.

use std::{
    fs::{self, File},
    io::Read,
    path::Path,
};

use crate::{GitError, diff, native};

const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_LINE_CHARS: usize = 16_384;
pub(super) const MAX_SOURCE_LINES: usize = 100_000;

/// Text suitable for a read-only source editor. Empty files are valid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFile {
    /// Full repository-relative selected path.
    pub path: String,
    /// Decoded text with its BOM removed; original line endings are preserved.
    pub text: String,
    /// Original byte encoding.
    pub encoding: TextEncoding,
    /// Editor line count; empty content has one line and final newlines add a line.
    pub line_count: usize,
}

/// Encodings accepted by the source editor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextEncoding {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
}

pub(super) fn read(
    path: &Path,
    file: &str,
    revision: Option<&str>,
) -> Result<SourceFile, GitError> {
    diff::validate_path(file)?;
    let repo = native::open(path)?;
    let bytes = if revision == Some(":index") {
        let index = repo.index().map_err(native::native("read source index"))?;
        if (1..=3).any(|stage| index.get_path(Path::new(file), stage).is_some()) {
            return Err(GitError::Unsupported {
                operation: "source from an unmerged index",
            });
        }
        let entry = index
            .get_path(Path::new(file), 0)
            .filter(|entry| entry.flags_extended & 0x2000 == 0)
            .ok_or(GitError::InvalidPath {
                path: file.to_owned(),
                reason: "the file is absent from the index",
            })?;
        blob_bytes(&repo, entry.id, entry.mode, file)?
    } else if let Some(revision) = revision {
        let commit = crate::history::resolve_commit(&repo, revision)?;
        let tree = commit
            .tree()
            .map_err(native::native("read source revision tree"))?;
        let entry = tree
            .get_path(Path::new(file))
            .map_err(native::native("find source in revision"))?;
        blob_bytes(&repo, entry.id(), entry.filemode() as u32, file)?
    } else {
        let path = diff::checked_working_path(&native::root(&repo)?, file)?;
        let metadata = fs::symlink_metadata(&path).map_err(|source| GitError::Io {
            operation: "read source file",
            source,
        })?;
        // A link is displayed as its target text; it is never dereferenced.
        if native::is_link(&metadata) {
            let target = fs::read_link(path).map_err(|source| GitError::Io {
                operation: "read source symbolic link",
                source,
            })?;
            target
                .to_str()
                .ok_or(GitError::InvalidEncoding {
                    path: file.to_owned(),
                    encoding: "symbolic link path",
                })?
                .as_bytes()
                .to_vec()
        } else {
            if !metadata.is_file() {
                return Err(GitError::InvalidPath {
                    path: file.to_owned(),
                    reason: "only ordinary files can be read as source",
                });
            }
            check_size(metadata.len(), file)?;
            let reader = File::open(&path).map_err(|source| GitError::Io {
                operation: "open source file",
                source,
            })?;
            let mut bytes = Vec::new();
            reader
                .take((MAX_SOURCE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|source| GitError::Io {
                    operation: "read source file",
                    source,
                })?;
            bytes
        }
    };
    check_size(bytes.len() as u64, file)?;
    let (text, encoding) = decode(&bytes, file)?;
    let line_count = validate_lines(&text, file)?;
    Ok(SourceFile {
        path: file.to_owned(),
        text,
        encoding,
        line_count,
    })
}

pub(super) fn validate_lines(text: &str, file: &str) -> Result<usize, GitError> {
    let mut line_count = 1;
    let mut line_chars = 0;
    for character in text.chars() {
        if character == '\n' {
            line_count += 1;
            line_chars = 0;
            if line_count > MAX_SOURCE_LINES {
                return Err(GitError::LineCountLimit {
                    path: file.to_owned(),
                    limit_lines: MAX_SOURCE_LINES,
                });
            }
        } else {
            line_chars += 1;
            if line_chars > MAX_LINE_CHARS {
                return Err(GitError::LineTooLong {
                    path: file.to_owned(),
                    limit_chars: MAX_LINE_CHARS,
                });
            }
        }
    }
    Ok(line_count)
}

fn blob_bytes(
    repo: &git2::Repository,
    id: git2::Oid,
    mode: u32,
    file: &str,
) -> Result<Vec<u8>, GitError> {
    if mode == 0o160000 || mode == 0o040000 {
        return Err(GitError::InvalidPath {
            path: file.to_owned(),
            reason: "select a file rather than a directory or submodule",
        });
    }
    let (size, _) = repo
        .odb()
        .map_err(native::native("read source database"))?
        .read_header(id)
        .map_err(native::native("measure source file"))?;
    check_size(size as u64, file)?;
    Ok(repo
        .find_blob(id)
        .map_err(native::native("read source blob"))?
        .content()
        .to_vec())
}

fn check_size(size: u64, file: &str) -> Result<(), GitError> {
    if size > MAX_SOURCE_BYTES as u64 {
        return Err(GitError::FileTooLarge {
            path: file.to_owned(),
            limit_bytes: MAX_SOURCE_BYTES,
        });
    }
    Ok(())
}

pub(super) fn decode(bytes: &[u8], file: &str) -> Result<(String, TextEncoding), GitError> {
    let (text, encoding) = if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe]) {
        (decode_utf16(bytes, file, false)?, TextEncoding::Utf16Le)
    } else if let Some(bytes) = bytes.strip_prefix(&[0xfe, 0xff]) {
        (decode_utf16(bytes, file, true)?, TextEncoding::Utf16Be)
    } else {
        let (bytes, encoding) = bytes
            .strip_prefix(&[0xef, 0xbb, 0xbf])
            .map_or((bytes, TextEncoding::Utf8), |bytes| {
                (bytes, TextEncoding::Utf8Bom)
            });
        (
            std::str::from_utf8(bytes)
                .map_err(|_| GitError::BinaryFile {
                    path: file.to_owned(),
                })?
                .to_owned(),
            encoding,
        )
    };
    // Preserve tabs and line endings, but reject control-heavy or NUL data.
    if text.chars().any(|character| {
        character.is_control() && !matches!(character, '\n' | '\r' | '\t' | '\u{c}')
    }) {
        return Err(GitError::BinaryFile {
            path: file.to_owned(),
        });
    }
    Ok((text, encoding))
}

fn decode_utf16(bytes: &[u8], file: &str, big_endian: bool) -> Result<String, GitError> {
    let (units, tail) = bytes.as_chunks::<2>();
    let encoding = if big_endian { "UTF-16BE" } else { "UTF-16LE" };
    if !tail.is_empty() {
        return Err(GitError::InvalidEncoding {
            path: file.to_owned(),
            encoding,
        });
    }
    let units = units.iter().map(|unit| {
        if big_endian {
            u16::from_be_bytes(*unit)
        } else {
            u16::from_le_bytes(*unit)
        }
    });
    let mut text = String::with_capacity(bytes.len());
    for character in char::decode_utf16(units) {
        text.push(character.map_err(|_| GitError::InvalidEncoding {
            path: file.to_owned(),
            encoding,
        })?);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_count_is_bounded_even_when_a_small_file_has_many_empty_lines() {
        assert_eq!(validate_lines("", "empty.txt").unwrap(), 1);
        assert_eq!(validate_lines("one\ntwo\n", "small.txt").unwrap(), 3);
        assert_eq!(
            validate_lines(&"\n".repeat(MAX_SOURCE_LINES - 1), "limit.txt").unwrap(),
            MAX_SOURCE_LINES
        );
        assert!(matches!(
            validate_lines(&"\n".repeat(MAX_SOURCE_LINES), "overflow.txt"),
            Err(GitError::LineCountLimit {
                limit_lines: MAX_SOURCE_LINES,
                ..
            })
        ));
    }

    #[test]
    fn line_length_limit_counts_unicode_scalars_and_is_independent_of_encoding() {
        let text = "中".repeat(MAX_LINE_CHARS);
        assert_eq!(validate_lines(&text, "wide.txt").unwrap(), 1);
        assert!(matches!(
            validate_lines(&(text + "文"), "wide.txt"),
            Err(GitError::LineTooLong { .. })
        ));
        let bytes = [0xff, 0xfe, 0x2d, 0x4e, 0x3d, 0xd8, 0x00, 0xde];
        let (text, encoding) = decode(&bytes, "utf16.txt").unwrap();
        assert_eq!(text, "中😀");
        assert_eq!(encoding, TextEncoding::Utf16Le);
        assert!(matches!(
            decode(&[0xfe, 0xff, 0xd8, 0x00], "bad.txt"),
            Err(GitError::InvalidEncoding { .. })
        ));
    }
}
