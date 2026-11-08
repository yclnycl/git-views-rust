//! Shared compiled query/filter options. Filters run before ranking or file IO.
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::{Regex, RegexBuilder};
use std::{error::Error, fmt, ops::Range};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SearchOptions {
    pub regex: bool,
    pub case_sensitive: bool,
    /// Repository-relative glob patterns. Bare patterns such as *.rs match any depth.
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    /// Repository-relative directory; descendants only, never a path prefix alias.
    pub directory: Option<String>,
    /// File extensions, with or without the leading dot, e.g. rs/java/tsx.
    pub extensions: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchOptionsError(pub String);
impl fmt::Display for SearchOptionsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl Error for SearchOptionsError {}

impl SearchOptions {
    pub fn validate(&self, query: &str) -> Result<(), SearchOptionsError> {
        CompiledSearchOptions::new(self, query).map(|_| ())
    }
}

pub(crate) struct CompiledSearchOptions {
    pub regex: Option<Regex>,
    include: GlobSet,
    exclude: GlobSet,
    directory: Option<String>,
    extensions: Vec<String>,
    case_sensitive: bool,
    prefix: String,
}
impl CompiledSearchOptions {
    pub fn new(options: &SearchOptions, query: &str) -> Result<Self, SearchOptionsError> {
        if query.len() > 1024 || query.chars().count() > 256 || query.contains(['\0', '\r', '\n']) {
            return Err(SearchOptionsError(
                "查询最多 256 个字符，不能包含换行或空字符。".into(),
            ));
        }
        let regex = options
            .regex
            .then(|| {
                RegexBuilder::new(query)
                    .case_insensitive(!options.case_sensitive)
                    .size_limit(1024 * 1024)
                    .dfa_size_limit(2 * 1024 * 1024)
                    .nest_limit(64)
                    .build()
                    .map_err(|error| {
                        SearchOptionsError(format!("正则表达式无效或过于复杂：{error}"))
                    })
            })
            .transpose()?;
        let directory = options
            .directory
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .map(|path| path.replace('\\', "/").trim_end_matches('/').to_owned());
        if directory.as_ref().is_some_and(|path| !safe_relative(path)) {
            return Err(SearchOptionsError(
                "目录范围必须是仓库内相对目录，不能包含 .. 或 .git。".into(),
            ));
        }
        if options.extensions.len() > 64
            || options.extensions.iter().any(|extension| {
                extension.len() > 32
                    || extension.trim_start_matches('.').is_empty()
                    || extension.trim_start_matches('.').chars().any(|character| {
                        !character.is_alphanumeric() && character != '_' && character != '-'
                    })
            })
        {
            return Err(SearchOptionsError(
                "文件类型最多 64 项，请填写 rs、java、tsx 等扩展名。".into(),
            ));
        }
        Ok(Self {
            regex,
            include: globs(&options.include_globs, options.case_sensitive)?,
            exclude: globs(&options.exclude_globs, options.case_sensitive)?,
            directory: directory.map(|path| {
                if options.case_sensitive {
                    path
                } else {
                    path.to_ascii_lowercase()
                }
            }),
            extensions: options
                .extensions
                .iter()
                .map(|extension| extension.trim_start_matches('.').to_ascii_lowercase())
                .collect(),
            case_sensitive: options.case_sensitive,
            prefix: String::new(),
        })
    }
    pub fn allows(&self, path: &str) -> bool {
        let combined;
        let path = if self.prefix.is_empty() {
            path
        } else {
            combined = format!("{}/{path}", self.prefix);
            &combined
        };
        let folded;
        let directory_path = if self.case_sensitive {
            path
        } else {
            folded = path.to_ascii_lowercase();
            &folded
        };
        self.directory.as_ref().is_none_or(|directory| {
            directory_path
                .strip_prefix(directory)
                .is_some_and(|suffix| suffix.starts_with('/'))
        }) && (self.extensions.is_empty()
            || path
                .rsplit('/')
                .next()
                .and_then(|name| name.rsplit_once('.'))
                .is_some_and(|(_, extension)| {
                    self.extensions
                        .iter()
                        .any(|candidate| candidate.eq_ignore_ascii_case(extension))
                }))
            && (self.include.is_empty() || self.include.is_match(path))
            && !self.exclude.is_match(path)
    }
    pub(crate) fn scoped(
        options: &SearchOptions,
        query: &str,
        prefix: &str,
    ) -> Result<Self, SearchOptionsError> {
        let mut compiled = Self::new(options, query)?;
        if !prefix.is_empty() && !safe_relative(prefix) {
            return Err(SearchOptionsError("搜索范围前缀无效。".into()));
        }
        compiled.prefix = prefix.to_owned();
        Ok(compiled)
    }
    pub fn path_highlights(&self, path: &str, query: &str) -> Vec<Range<usize>> {
        if let Some(regex) = &self.regex {
            return regex
                .find_iter(path)
                .take(32)
                .filter(|hit| !hit.is_empty())
                .map(|hit| hit.range())
                .collect();
        }
        let path_folded;
        let query_folded;
        let (haystack, needle) = if self.case_sensitive {
            (path, query)
        } else {
            path_folded = path.to_ascii_lowercase();
            query_folded = query.to_ascii_lowercase();
            (path_folded.as_str(), query_folded.as_str())
        };
        if needle.is_empty() {
            return Vec::new();
        }
        let exact: Vec<_> = haystack
            .match_indices(needle)
            .take(32)
            .map(|(position, text)| position..position + text.len())
            .collect();
        if !exact.is_empty() {
            return exact;
        }
        let mut next = needle.chars();
        let mut wanted = next.next();
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for (offset, character) in haystack.char_indices() {
            if wanted != Some(character) {
                continue;
            }
            if let Some(last) = ranges.last_mut().filter(|last| last.end == offset) {
                last.end += character.len_utf8();
            } else {
                ranges.push(offset..offset + character.len_utf8());
            }
            wanted = next.next();
            if wanted.is_none() {
                return ranges;
            }
        }
        Vec::new()
    }
}

fn globs(patterns: &[String], case_sensitive: bool) -> Result<GlobSet, SearchOptionsError> {
    if patterns.len() > 64 || patterns.iter().any(|pattern| pattern.len() > 512) {
        return Err(SearchOptionsError(
            "包含/排除规则各最多 64 条，每条最多 512 字节。".into(),
        ));
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let pattern = pattern.replace('\\', "/");
        if !safe_relative(&pattern) {
            return Err(SearchOptionsError(
                "匹配规则必须是仓库相对路径，不能包含 .. 或 .git。".into(),
            ));
        }
        let pattern = if pattern.contains('/') {
            pattern
        } else {
            format!("**/{pattern}")
        };
        builder.add(
            GlobBuilder::new(&pattern)
                .literal_separator(true)
                .backslash_escape(false)
                .case_insensitive(!case_sensitive)
                .build()
                .map_err(|error| SearchOptionsError(format!("匹配规则无效：{error}")))?,
        );
    }
    builder
        .build()
        .map_err(|error| SearchOptionsError(format!("匹配规则无法编译：{error}")))
}
pub(crate) fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\0', '\\', ':'])
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| {
            !part.is_empty() && part != "." && part != ".." && !part.eq_ignore_ascii_case(".git")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filter_intersects_directory_extension_globs_and_exclusion() {
        let options = SearchOptions {
            directory: Some("src".into()),
            extensions: vec![".rs".into()],
            include_globs: vec!["*.rs".into()],
            exclude_globs: vec!["**/generated/**".into()],
            ..Default::default()
        };
        let filters = CompiledSearchOptions::new(&options, "needle").unwrap();
        assert!(filters.allows("src/deep/file.rs"));
        assert!(!filters.allows("src2/file.rs"));
        assert!(!filters.allows("src/file.java"));
        assert!(!filters.allows("src/generated/file.rs"));
    }
    #[test]
    fn invalid_or_unsafe_globs_and_regex_are_explicit_errors() {
        for pattern in ["../secret", "/absolute", ".git/**", "[broken"] {
            assert!(
                SearchOptions {
                    include_globs: vec![pattern.into()],
                    ..Default::default()
                }
                .validate("x")
                .is_err()
            );
        }
        assert!(
            SearchOptions {
                regex: true,
                ..Default::default()
            }
            .validate("(")
            .is_err()
        );
    }
}
