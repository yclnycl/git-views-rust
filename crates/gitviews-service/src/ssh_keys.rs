//! Explicitly requested SSH key-path discovery.
//!
//! The caller must obtain the user's consent before calling this module. Only
//! key-file metadata and the bounded SSH config are read; private key contents
//! are never opened. Discovery does not run SSH, expand commands, follow
//! `Include` directives, or scan directories for arbitrary key files.

use std::{
    collections::HashSet,
    ffi::OsString,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
};

const MAX_SSH_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_KEY_CANDIDATES: usize = 64;
const DEFAULT_KEY_NAMES: &[&str] = &[
    "id_ed25519",
    "id_ed25519_sk",
    "id_ecdsa",
    "id_ecdsa_sk",
    "id_rsa",
    "id_dsa",
    "id_xmss",
    "identity",
];

/// Find existing private-key paths after the UI has obtained explicit consent.
///
/// Candidates are the common names under the current user's `.ssh` directory
/// and static `IdentityFile` entries in that directory's `config` file. Returned
/// paths are absolute, canonical, deduplicated regular files. This function
/// does not open private keys or establish any network connection.
pub fn discover_ssh_keys() -> Result<Vec<PathBuf>, String> {
    #[cfg(windows)]
    const HOME_VARIABLE: &str = "USERPROFILE";
    #[cfg(not(windows))]
    const HOME_VARIABLE: &str = "HOME";

    let home = std::env::var_os(HOME_VARIABLE)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("无法确定当前用户目录：环境变量 {HOME_VARIABLE} 未设置。"))?;
    discover_from(&home)
}

fn discover_from(home: &Path) -> Result<Vec<PathBuf>, String> {
    if !home.is_absolute() {
        return Err("无法发现 SSH 密钥：当前用户目录必须是绝对路径。".into());
    }
    let ssh_directory = home.join(".ssh");
    match fs::metadata(&ssh_directory) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "SSH 路径不是目录：{}。",
                crate::display_path(&ssh_directory)
            ));
        }
        Err(error) if is_missing(&error) => return Ok(Vec::new()),
        Err(error) => return Err(file_error("访问 SSH 目录", &ssh_directory, error)),
    }

    let mut candidates: Vec<PathBuf> = DEFAULT_KEY_NAMES
        .iter()
        .map(|name| ssh_directory.join(name))
        .collect();
    if let Some(config) = read_config(&ssh_directory.join("config"))? {
        let remaining = MAX_KEY_CANDIDATES.saturating_sub(candidates.len());
        candidates.extend(
            config
                .lines()
                .filter_map(|line| {
                    identity_file_value(line)
                        .and_then(|value| resolve_identity_file(&value, home, &ssh_directory))
                })
                .take(remaining),
        );
    }

    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for candidate in candidates {
        if is_public_key(&candidate) {
            continue;
        }
        let path = match fs::canonicalize(&candidate) {
            Ok(path) => path,
            Err(error) if is_missing(&error) => continue,
            Err(error) => return Err(file_error("定位 SSH 密钥", &candidate, error)),
        };
        // Check the resolved extension as well, so a differently named symlink
        // cannot make a public-key file appear as a private-key candidate.
        if is_public_key(&path) {
            continue;
        }
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if is_missing(&error) => continue,
            Err(error) => return Err(file_error("读取 SSH 密钥信息", &path, error)),
        };
        if metadata.is_file() && path.is_absolute() && seen.insert(path.clone()) {
            found.push(path);
        }
    }
    Ok(found)
}

fn read_config(path: &Path) -> Result<Option<String>, String> {
    let path_metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if is_missing(&error) => return Ok(None),
        Err(error) => return Err(file_error("读取 SSH 配置信息", path, error)),
    };
    if !path_metadata.is_file() {
        return Err(format!(
            "SSH 配置不是普通文件：{}。",
            crate::display_path(path)
        ));
    }
    if path_metadata.len() > MAX_SSH_CONFIG_BYTES {
        return Err(config_too_large(path));
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if is_missing(&error) => return Ok(None),
        Err(error) => return Err(file_error("打开 SSH 配置", path, error)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| file_error("读取 SSH 配置信息", path, error))?;
    if !metadata.is_file() {
        return Err(format!(
            "SSH 配置不是普通文件：{}。",
            crate::display_path(path)
        ));
    }
    if metadata.len() > MAX_SSH_CONFIG_BYTES {
        return Err(config_too_large(path));
    }
    let mut contents = String::new();
    file.take(MAX_SSH_CONFIG_BYTES + 1)
        .read_to_string(&mut contents)
        .map_err(|error| file_error("读取 SSH 配置", path, error))?;
    // The file may have grown after checking its metadata.
    if contents.len() as u64 > MAX_SSH_CONFIG_BYTES {
        return Err(config_too_large(path));
    }
    Ok(Some(contents))
}

fn config_too_large(path: &Path) -> String {
    format!(
        "SSH 配置 {} 超过 {} KiB，无法自动发现密钥，请手动选择密钥路径。",
        crate::display_path(path),
        MAX_SSH_CONFIG_BYTES / 1024
    )
}

fn is_missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn file_error(action: &str, path: &Path, error: io::Error) -> String {
    if error.kind() == io::ErrorKind::PermissionDenied {
        format!(
            "{action} {} 失败：权限不足，请检查访问权限。",
            crate::display_path(path)
        )
    } else {
        format!("{action} {} 失败：{error}。", crate::display_path(path))
    }
}

fn is_public_key(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pub"))
}

/// Parse just the static IdentityFile directive, without interpreting other
/// directives, host matching, escapes that might run commands, or includes.
fn identity_file_value(line: &str) -> Option<String> {
    let line = line.trim_start_matches('\u{feff}').trim_start();
    let option_end = line
        .find(|character: char| character.is_ascii_whitespace() || character == '=')
        .unwrap_or(line.len());
    if !line[..option_end].eq_ignore_ascii_case("IdentityFile") {
        return None;
    }
    let mut value = line[option_end..].trim_start();
    if let Some(remainder) = value.strip_prefix('=') {
        value = remainder.trim_start();
    }
    let first = value.chars().next()?;
    let (path, remainder) = if first == '"' || first == '\'' {
        let mut characters = value[first.len_utf8()..].char_indices().peekable();
        let mut path = String::new();
        let mut end = None;
        while let Some((offset, character)) = characters.next() {
            if character == first {
                end = Some(first.len_utf8() + offset + character.len_utf8());
                break;
            }
            // Preserve Windows path separators; only an escaped matching quote
            // has special meaning here.
            if character == '\\' && characters.peek().is_some_and(|(_, next)| *next == first) {
                characters.next();
                path.push(first);
            } else {
                path.push(character);
            }
        }
        (path, &value[end?..])
    } else {
        let end = value
            .find(|character: char| character.is_ascii_whitespace() || character == '#')
            .unwrap_or(value.len());
        (value[..end].to_owned(), &value[end..])
    };
    let remainder = remainder.trim_start();
    if path.is_empty() || !(remainder.is_empty() || remainder.starts_with('#')) {
        return None;
    }
    Some(path)
}

fn resolve_identity_file(value: &str, home: &Path, ssh_directory: &Path) -> Option<PathBuf> {
    if value.eq_ignore_ascii_case("none")
        || value.contains(['\0', '$', '`'])
        || (value.starts_with('~') && !value.starts_with("~/") && !value.starts_with("~\\"))
    {
        return None;
    }
    let (value, from_home) = if let Some(value) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        (value, true)
    } else {
        (value, false)
    };
    // Forward slashes in config paths are valid on Windows, but the extended
    // absolute prefix returned by canonicalize requires native separators.
    #[cfg(windows)]
    let native_value = value.replace('/', "\\");
    #[cfg(windows)]
    let value = native_value.as_str();
    let mut expanded = OsString::new();
    let mut remainder = value;
    while let Some(offset) = remainder.find('%') {
        expanded.push(&remainder[..offset]);
        let token = &remainder[offset + 1..];
        // %d is the only supported SSH token. Unknown or unresolved tokens are
        // ignored, including host-dependent %h/%r and literal-percent escapes.
        remainder = token.strip_prefix('d')?;
        expanded.push(home.as_os_str());
    }
    expanded.push(remainder);
    let path = PathBuf::from(expanded);
    if from_home {
        Some(home.join(path))
    } else if path.is_absolute() {
        Some(path)
    } else if path.has_root()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::Prefix(_)))
    {
        // Reject Windows root-relative and drive-relative paths: their meaning
        // depends on process state rather than the user's .ssh directory.
        None
    } else {
        Some(ssh_directory.join(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/ssh-key-tests");
            fs::create_dir_all(&root).unwrap();
            let directory = root.join(format!(
                "{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&directory).unwrap();
            Self(fs::canonicalize(directory).unwrap())
        }

        fn ssh(&self) -> PathBuf {
            let ssh = self.0.join(".ssh");
            fs::create_dir_all(&ssh).unwrap();
            ssh
        }

        fn key(&self, path: &Path) -> PathBuf {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            // Deliberately invalid UTF-8: discovery must never decode a key.
            fs::write(path, [0xff, 0x00, 0xfe]).unwrap();
            fs::canonicalize(path).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/ssh-key-tests");
            if fs::canonicalize(root).ok().as_deref() == self.0.parent() {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn discovers_only_common_existing_regular_files_without_scanning() {
        let fixture = Fixture::new();
        let ssh = fixture.ssh();
        let expected = fixture.key(&ssh.join("id_ed25519"));
        fixture.key(&ssh.join("id_ed25519.pub"));
        fixture.key(&ssh.join("unlisted-key"));
        fixture.key(&ssh.join("nested/id_rsa"));
        fs::create_dir(ssh.join("id_rsa")).unwrap();
        assert_eq!(discover_from(&fixture.0).unwrap(), vec![expected]);
    }

    #[test]
    fn parses_quoted_relative_home_and_percent_d_paths_and_deduplicates() {
        let fixture = Fixture::new();
        let ssh = fixture.ssh();
        let default = fixture.key(&ssh.join("id_rsa"));
        let quoted = fixture.key(&ssh.join("work key"));
        let home_key = fixture.key(&fixture.0.join("outside ssh/key"));
        let percent_key = fixture.key(&fixture.0.join("percent-key"));
        let absolute_key = fixture.key(&fixture.0.join("absolute key"));
        let config = format!(
            "Host example\n  identityfile = \"work key\" # comment\nIdentityFile '~/outside ssh/key'\nIdentityFile %d/percent-key\nIdentityFile \"{}\"\nIdentityFile ~/.ssh/id_rsa\nIdentityFile work.pub\n",
            absolute_key.display()
        );
        fixture.key(&ssh.join("work.pub"));
        fs::write(ssh.join("config"), config).unwrap();
        assert_eq!(
            discover_from(&fixture.0).unwrap(),
            vec![default, quoted, home_key, percent_key, absolute_key]
        );
    }

    #[test]
    fn ignores_includes_commands_unresolved_tokens_and_malformed_directives() {
        let fixture = Fixture::new();
        let ssh = fixture.ssh();
        let ignored = fixture.key(&ssh.join("hidden"));
        fs::write(ssh.join("extra-config"), "IdentityFile hidden").unwrap();
        fs::write(
            ssh.join("config"),
            "Include extra-config\nProxyCommand echo hidden\nIdentityFile %h/hidden\nIdentityFile $HOME/hidden\nIdentityFile `echo hidden`\nIdentityFile $(echo hidden)\nIdentityFile ~someone/hidden\nIdentityFile none\nIdentityFile \"hidden\nIdentityFile hidden extra\n",
        )
        .unwrap();
        assert!(ignored.is_file());
        assert!(discover_from(&fixture.0).unwrap().is_empty());
    }

    #[test]
    fn missing_ssh_directory_and_missing_config_are_normal() {
        let fixture = Fixture::new();
        assert!(discover_from(&fixture.0).unwrap().is_empty());
        fixture.ssh();
        assert!(discover_from(&fixture.0).unwrap().is_empty());
        assert!(discover_from(Path::new("relative-home")).is_err());
    }

    #[test]
    fn refuses_oversized_or_non_file_config() {
        let fixture = Fixture::new();
        let config = fixture.ssh().join("config");
        let file = File::create(&config).unwrap();
        file.set_len(MAX_SSH_CONFIG_BYTES + 1).unwrap();
        drop(file);
        assert!(
            discover_from(&fixture.0)
                .unwrap_err()
                .contains("超过 256 KiB")
        );
        fs::remove_file(&config).unwrap();
        fs::create_dir(&config).unwrap();
        assert!(discover_from(&fixture.0).is_err());
    }

    #[test]
    fn candidate_count_is_bounded() {
        let fixture = Fixture::new();
        let ssh = fixture.ssh();
        let mut config = String::new();
        for index in 0..MAX_KEY_CANDIDATES + 2 {
            fixture.key(&ssh.join(format!("key-{index}")));
            config.push_str(&format!("IdentityFile key-{index}\n"));
        }
        fs::write(ssh.join("config"), config).unwrap();
        assert_eq!(
            discover_from(&fixture.0).unwrap().len(),
            MAX_KEY_CANDIDATES - DEFAULT_KEY_NAMES.len()
        );
    }

    #[test]
    fn quoted_comments_and_windows_separators_are_preserved() {
        assert_eq!(
            identity_file_value("IdentityFile = \"C:\\Users\\name\\key # 1\" # comment"),
            Some("C:\\Users\\name\\key # 1".into())
        );
        assert_eq!(identity_file_value("IdentityFile=key"), Some("key".into()));
        assert_eq!(identity_file_value("# IdentityFile key"), None);
        assert_eq!(identity_file_value("IdentityFiles key"), None);
        assert_eq!(identity_file_value("IdentityFile \"key\" garbage"), None);
    }

    #[test]
    fn permission_failures_explain_how_to_recover() {
        let error = file_error(
            "访问 SSH 目录",
            Path::new("fixture/.ssh"),
            io::Error::from(io::ErrorKind::PermissionDenied),
        );
        assert!(error.contains("权限不足"));
        assert!(error.contains("fixture"));
    }

    #[cfg(unix)]
    #[test]
    fn resolved_public_key_symlinks_are_excluded() {
        let fixture = Fixture::new();
        let ssh = fixture.ssh();
        let public = fixture.key(&ssh.join("other.pub"));
        std::os::unix::fs::symlink(public, ssh.join("id_ed25519")).unwrap();
        assert!(discover_from(&fixture.0).unwrap().is_empty());
    }
}
