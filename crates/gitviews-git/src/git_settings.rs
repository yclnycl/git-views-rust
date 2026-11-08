//! Repository-local configuration. Passwords and key passphrases stay in memory.

use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use git2::{Config, ErrorCode, Repository};

use crate::{AuthProtocol, GitError, RemoteAuth, remote_protocol};

const OPERATION: &str = "Git 配置";
const HTTPS_USERNAME: &str = "gitviewsHttpsUsername";
const SSH_USERNAME: &str = "gitviewsSshUsername";
const SSH_PRIVATE_KEY: &str = "gitviewsSshPrivateKey";

/// Identity and nonsecret remote settings for one repository.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GitSettings {
    pub user_name: String,
    pub user_email: String,
    pub remotes: Vec<GitRemoteSettings>,
}

/// Existing remote addresses and explicit authentication defaults.
/// Passwords and key passphrases are intentionally absent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GitRemoteSettings {
    pub name: String,
    pub url: String,
    pub push_url: Option<String>,
    pub https_username: String,
    pub ssh_username: String,
    pub ssh_private_key: Option<PathBuf>,
}

impl GitRemoteSettings {
    /// Authentication protocol of the fetch address.
    pub fn protocol(&self) -> Option<AuthProtocol> {
        remote_protocol(&self.url)
    }
}

pub(super) fn read(root: &Path) -> Result<GitSettings, GitError> {
    let repo = open_root(root)?;
    let config = repo.config().map_err(native)?;
    // Identity can inherit Git's normal global defaults. Authentication choices
    // belong to this repository and never inherit another repository's values.
    let local = Config::open(&repo.commondir().join("config")).map_err(native)?;
    let mut remotes = Vec::new();
    for name in repo.remotes().map_err(native)?.iter() {
        let name = name
            .map_err(native)?
            .ok_or_else(|| invalid("远端名称不是有效 UTF-8。"))?;
        let remote = repo.find_remote(name).map_err(native)?;
        let url = remote.url().map_err(native)?.to_owned();
        let push_url = remote.pushurl().map_err(native)?.map(str::to_owned);
        validate_url(&url)?;
        if let Some(url) = &push_url {
            validate_url(url)?;
        }
        remotes.push(GitRemoteSettings {
            name: name.to_owned(),
            url,
            push_url,
            https_username: value(&local, &key(name, HTTPS_USERNAME))?,
            ssh_username: value(&local, &key(name, SSH_USERNAME))?,
            ssh_private_key: optional_value(&local, &key(name, SSH_PRIVATE_KEY))?
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
        });
    }
    remotes.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(GitSettings {
        user_name: value(&config, "user.name")?,
        user_email: value(&config, "user.email")?,
        remotes,
    })
}

pub(super) fn save(root: &Path, settings: &GitSettings) -> Result<(), GitError> {
    validate_text(&settings.user_name, "用户姓名不能含控制字符或首尾空白。")?;
    validate_text(&settings.user_email, "用户邮箱不能含控制字符或首尾空白。")?;
    let repo = open_root(root)?;
    let config_path = repo.commondir().join("config");
    let permissions = fs::metadata(&config_path).map_err(io)?.permissions();
    if permissions.readonly() {
        return Err(invalid("Git 配置文件为只读，请检查文件权限。"));
    }
    // Take Git's conventional config lock and stage the complete update. This
    // preserves comments, refspecs and unrelated keys, and prevents partial saves.
    let lock_path = repo.commondir().join("config.lock");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .map_err(io)?;
    let mut guard = ConfigLock {
        path: lock_path.clone(),
        committed: false,
    };
    // Match restrictive permissions before copying potentially sensitive existing
    // config bytes, and restore them after libgit2 replaces the staged file.
    file.set_permissions(permissions.clone()).map_err(io)?;
    let bytes = fs::read(&config_path).map_err(io)?;
    file.write_all(&bytes).map_err(io)?;
    file.sync_all().map_err(io)?;
    drop(file);
    let mut staged = Config::open(&lock_path).map_err(native)?;
    let mut seen = BTreeSet::new();
    for remote in &settings.remotes {
        validate_remote(&staged, remote, &mut seen)?;
    }
    // Perform every validation before modifying even the staged configuration.
    set_optional(&mut staged, "user.name", nonempty(&settings.user_name))?;
    set_optional(&mut staged, "user.email", nonempty(&settings.user_email))?;
    for remote in &settings.remotes {
        staged
            .set_str(&key(&remote.name, "url"), &remote.url)
            .map_err(native)?;
        set_optional(
            &mut staged,
            &key(&remote.name, "pushurl"),
            remote.push_url.as_deref(),
        )?;
        set_optional(
            &mut staged,
            &key(&remote.name, HTTPS_USERNAME),
            nonempty(&remote.https_username),
        )?;
        set_optional(
            &mut staged,
            &key(&remote.name, SSH_USERNAME),
            nonempty(&remote.ssh_username),
        )?;
        set_optional(
            &mut staged,
            &key(&remote.name, SSH_PRIVATE_KEY),
            remote.ssh_private_key.as_deref().and_then(Path::to_str),
        )?;
    }
    drop(staged);
    fs::set_permissions(&lock_path, permissions).map_err(io)?;
    // Synchronize libgit2's final staged file before its atomic replacement.
    OpenOptions::new()
        .write(true)
        .open(&lock_path)
        .and_then(|file| file.sync_all())
        .map_err(io)?;
    fs::rename(&lock_path, &config_path).map_err(io)?;
    guard.committed = true;
    Ok(())
}

fn validate_remote(
    config: &Config,
    remote: &GitRemoteSettings,
    seen: &mut BTreeSet<String>,
) -> Result<(), GitError> {
    if remote.name.is_empty()
        || remote.name.chars().any(char::is_control)
        || !seen.insert(remote.name.clone())
    {
        return Err(invalid("远端名称无效或重复。"));
    }
    // Names are immutable, and this editor never creates or deletes remotes.
    if optional_value(config, &key(&remote.name, "url"))?.is_none() {
        return Err(invalid("远端已被删除或更名，请刷新配置后重试。"));
    }
    validate_url(&remote.url)?;
    if let Some(url) = &remote.push_url {
        validate_url(url)?;
    }
    validate_text(
        &remote.https_username,
        "HTTPS 用户名不能含控制字符或首尾空白。",
    )?;
    validate_ssh_username(&remote.ssh_username)?;
    if !remote.ssh_username.is_empty() {
        for url in std::iter::once(remote.url.as_str()).chain(remote.push_url.as_deref()) {
            if crate::remote::ssh_url_username(url)
                .is_some_and(|username| username != remote.ssh_username)
            {
                return Err(invalid(
                    "SSH 用户名与仓库地址不一致，请修改地址中的用户名或保持一致。",
                ));
            }
        }
    }
    if let Some(path) = &remote.ssh_private_key {
        let text = path
            .to_str()
            .ok_or_else(|| invalid("SSH 私钥路径必须是有效 UTF-8。"))?;
        if !path.is_absolute() || text.chars().any(char::is_control) {
            return Err(invalid("SSH 私钥路径必须是绝对路径，且不能含控制字符。"));
        }
    }
    Ok(())
}

/// Construct defaults only for the actual named remote and selected address.
/// A prompt's explicit credentials always take precedence as one complete set.
pub(super) fn remote_auth(
    repo: &Repository,
    name: &str,
    url: &str,
    explicit: &RemoteAuth,
) -> Result<RemoteAuth, GitError> {
    if explicit.is_explicit() {
        return Ok(explicit.clone());
    }
    let remote = repo.find_remote(name).map_err(native)?;
    let fetch_url = remote.url().map_err(native)?;
    let push_url = remote.pushurl().map_err(native)?.unwrap_or(fetch_url);
    if url != fetch_url && url != push_url {
        return Ok(explicit.clone());
    }
    let config = Config::open(&repo.commondir().join("config")).map_err(native)?;
    let protocol = remote_protocol(url);
    let username = match protocol {
        Some(AuthProtocol::Https) => value(&config, &key(name, HTTPS_USERNAME))?,
        Some(AuthProtocol::Ssh) => match crate::remote::ssh_url_username(url) {
            Some(username) => username,
            None => value(&config, &key(name, SSH_USERNAME))?,
        },
        None => return Ok(explicit.clone()),
    };
    if protocol == Some(AuthProtocol::Ssh) {
        validate_ssh_username(&username)?;
    } else {
        validate_text(&username, "HTTPS 用户名不能含控制字符或首尾空白。")?;
    }
    if protocol == Some(AuthProtocol::Ssh)
        && let Some(path) =
            optional_value(&config, &key(name, SSH_PRIVATE_KEY))?.filter(|path| !path.is_empty())
    {
        let path = PathBuf::from(path);
        if !path.is_absolute()
            || path
                .to_str()
                .is_none_or(|path| path.chars().any(char::is_control))
        {
            return Err(invalid("已保存的 SSH 私钥路径无效，请检查 Git 配置。"));
        }
        return Ok(RemoteAuth::ssh_key(username, path, None));
    }
    Ok(if username.is_empty() {
        RemoteAuth::default()
    } else {
        RemoteAuth::default().with_username(username)
    })
}

fn validate_url(url: &str) -> Result<(), GitError> {
    crate::remote::validate_settings_url(url)
        .map_err(|_| invalid("仓库地址无效；请移除密码、令牌、查询参数或控制字符。"))
}

fn validate_text(text: &str, reason: &'static str) -> Result<(), GitError> {
    if text.trim() != text || text.chars().any(char::is_control) || text.len() > 4096 {
        Err(invalid(reason))
    } else {
        Ok(())
    }
}

fn validate_ssh_username(username: &str) -> Result<(), GitError> {
    validate_text(username, "认证用户名不能含控制字符或首尾空白。")?;
    if username.chars().any(char::is_whitespace) || username.contains(['@', ':', '/', '\\']) {
        return Err(invalid("认证用户名不能含空白或 @、:、/、\\ 字符。"));
    }
    Ok(())
}

fn open_root(root: &Path) -> Result<Repository, GitError> {
    let canonical = fs::canonicalize(root).map_err(io)?;
    let repo = Repository::open(&canonical).map_err(native)?;
    if repo
        .workdir()
        .and_then(|path| fs::canonicalize(path).ok())
        .as_ref()
        != Some(&canonical)
    {
        return Err(GitError::NotRepository {
            path: root.to_owned(),
        });
    }
    Ok(repo)
}

fn key(name: &str, suffix: &str) -> String {
    format!("remote.{name}.{suffix}")
}

fn optional_value(config: &Config, key: &str) -> Result<Option<String>, GitError> {
    match config.get_string(key) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(None),
        Err(error) => Err(native(error)),
    }
}

fn value(config: &Config, key: &str) -> Result<String, GitError> {
    Ok(optional_value(config, key)?.unwrap_or_default())
}

fn nonempty(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

fn set_optional(config: &mut Config, key: &str, value: Option<&str>) -> Result<(), GitError> {
    if let Some(value) = value {
        config.set_str(key, value).map_err(native)
    } else {
        match config.remove(key) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ErrorCode::NotFound => Ok(()),
            Err(error) => Err(native(error)),
        }
    }
}

fn invalid(reason: &'static str) -> GitError {
    GitError::InvalidRequest {
        operation: OPERATION,
        reason,
    }
}

fn native(_source: git2::Error) -> GitError {
    // A config parser error may include the malformed line, including a token
    // or password already present in a user-maintained file. Never expose it.
    invalid("Git 配置读取或写入失败，请检查配置格式及文件权限。")
}

fn io(source: std::io::Error) -> GitError {
    GitError::Io {
        operation: OPERATION,
        source,
    }
}

struct ConfigLock {
    path: PathBuf,
    committed: bool,
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Git2Backend, GitBackend};
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        base: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let base =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/git-settings-tests");
            fs::create_dir_all(&base).unwrap();
            let base = fs::canonicalize(base).unwrap();
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = base.join(format!(
                "{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            Repository::init(&root).unwrap();
            Self { base, root }
        }

        fn repository(&self) -> Repository {
            Repository::open(&self.root).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Ok(root) = fs::canonicalize(&self.root)
                && root != self.base
                && root.starts_with(&self.base)
            {
                let _ = fs::remove_dir_all(root);
            }
        }
    }

    #[test]
    fn roundtrip_updates_only_local_settings_and_preserves_refspecs() {
        let fixture = Fixture::new();
        let repo = fixture.repository();
        repo.remote("origin", "https://fetch.example.test/team/repo.git")
            .unwrap();
        repo.remote("backup", "git@backup.example.test:team/repo.git")
            .unwrap();
        let mut config = repo.config().unwrap();
        config
            .set_str(
                "remote.origin.pushurl",
                "ssh://git@push.example.test/team/repo.git",
            )
            .unwrap();
        config
            .set_str(
                "remote.origin.fetch",
                "+refs/heads/review/*:refs/remotes/origin/review/*",
            )
            .unwrap();
        config.set_bool("fetch.prune", true).unwrap();
        let mut settings = Git2Backend.git_settings(&fixture.root).unwrap();
        settings.user_name = "配置测试".into();
        settings.user_email = "person@example.test".into();
        let origin = settings
            .remotes
            .iter_mut()
            .find(|remote| remote.name == "origin")
            .unwrap();
        origin.url = "https://fetch.example.test/team/renamed.git".into();
        origin.https_username = "person@example.test".into();
        origin.ssh_username = "git".into();
        origin.ssh_private_key = Some(fixture.root.join("selected-private-key"));
        Git2Backend
            .save_git_settings(&fixture.root, &settings)
            .unwrap();
        assert_eq!(Git2Backend.git_settings(&fixture.root).unwrap(), settings);
        let config = repo.config().unwrap();
        assert_eq!(
            config.get_string("remote.origin.fetch").unwrap(),
            "+refs/heads/review/*:refs/remotes/origin/review/*"
        );
        assert!(config.get_bool("fetch.prune").unwrap());
        assert!(repo.find_remote("backup").is_ok());
        assert!(!repo.path().join("config.lock").exists());
    }

    #[test]
    fn invalid_updates_never_partially_change_repository_config() {
        let fixture = Fixture::new();
        let repo = fixture.repository();
        repo.remote("origin", "https://example.test/repo.git")
            .unwrap();
        let before = fs::read(repo.path().join("config")).unwrap();
        for bad_url in [
            "https://alice:secret@example.test/repo.git",
            "https://example.test/repo.git?token=secret",
            "ssh://git:secret@example.test/repo.git",
            "https://example.test\n/repo.git",
            "https://example.test\\@other.test/repo.git",
        ] {
            let mut settings = read(&fixture.root).unwrap();
            settings.user_name = "must not be saved".into();
            settings.remotes[0].url = bad_url.into();
            let error = save(&fixture.root, &settings).unwrap_err();
            assert!(!error.to_string().contains("secret"));
            assert_eq!(fs::read(repo.path().join("config")).unwrap(), before);
            assert!(!repo.path().join("config.lock").exists());
        }
        let mut settings = read(&fixture.root).unwrap();
        settings.remotes[0].ssh_private_key = Some(PathBuf::from("relative-private-key"));
        assert!(save(&fixture.root, &settings).is_err());
        assert_eq!(fs::read(repo.path().join("config")).unwrap(), before);
        settings.remotes[0].ssh_private_key = None;
        settings.remotes[0].name = "unregistered".into();
        assert!(save(&fixture.root, &settings).is_err());
        assert!(repo.find_remote("unregistered").is_err());
    }

    #[test]
    fn config_lock_and_exact_repository_root_are_respected() {
        let fixture = Fixture::new();
        let repo = fixture.repository();
        let settings = read(&fixture.root).unwrap();
        let lock = repo.path().join("config.lock");
        fs::write(&lock, "another process owns this lock").unwrap();
        assert!(save(&fixture.root, &settings).is_err());
        assert_eq!(
            fs::read_to_string(&lock).unwrap(),
            "another process owns this lock"
        );
        fs::remove_file(lock).unwrap();
        let nested = fixture.root.join("nested");
        fs::create_dir(&nested).unwrap();
        assert!(read(&nested).is_err());
        assert!(save(&nested, &settings).is_err());
    }

    #[test]
    fn ssh_username_conflicts_are_rejected_on_save_but_old_config_uses_url_username() {
        let fixture = Fixture::new();
        let repo = fixture.repository();
        let https = "https://example.test/team/repo.git";
        let ssh = "git@example.test:team/repo.git";
        repo.remote("origin", https).unwrap();
        let before = fs::read(repo.path().join("config")).unwrap();
        for (url, push_url) in [(ssh, None), (https, Some(ssh.to_owned()))] {
            let mut settings = read(&fixture.root).unwrap();
            settings.remotes[0].url = url.into();
            settings.remotes[0].push_url = push_url;
            settings.remotes[0].ssh_username = "alice".into();
            let error = save(&fixture.root, &settings).unwrap_err();
            assert!(error.to_string().contains("SSH 用户名与仓库地址不一致"));
            assert_eq!(fs::read(repo.path().join("config")).unwrap(), before);
        }
        let mut config = repo.config().unwrap();
        config.set_str("remote.origin.url", ssh).unwrap();
        config
            .set_str("remote.origin.gitviewsSshUsername", "alice")
            .unwrap();
        let mut auth = remote_auth(&repo, "origin", ssh, &RemoteAuth::default()).unwrap();
        auth.bind_remote_url(ssh).unwrap();
        assert_eq!(auth.challenge().unwrap().username.as_deref(), Some("git"));
        assert!(
            auth.credentials(ssh, Some("git"), git2::CredentialType::USERNAME)
                .is_ok()
        );
        // A configured username still fills SSH addresses that omit one.
        let without_username = "ssh://example.test/team/repo.git";
        config
            .set_str("remote.origin.url", without_username)
            .unwrap();
        let mut auth =
            remote_auth(&repo, "origin", without_username, &RemoteAuth::default()).unwrap();
        auth.bind_remote_url(without_username).unwrap();
        assert_eq!(auth.challenge().unwrap().username.as_deref(), Some("alice"));
    }

    #[test]
    fn malformed_config_diagnostics_never_expose_embedded_secrets() {
        let fixture = Fixture::new();
        let config_path = fixture.repository().path().join("config");
        let secret = "must-never-appear-in-errors";
        fs::write(
            config_path,
            format!("[core]\nrepositoryformatversion = 0\n[remote \"origin\"]\nurl = \"https://example.test/repo.git\\q{secret}\"\n"),
        ).unwrap();
        for error in [
            read(&fixture.root).unwrap_err(),
            save(&fixture.root, &GitSettings::default()).unwrap_err(),
        ] {
            assert!(!error.to_string().contains(secret));
            assert!(!format!("{error:?}").contains(secret));
            assert!(error.to_string().contains("配置格式"));
        }
    }

    #[test]
    fn readonly_config_is_unchanged_and_does_not_leave_a_lock() {
        let fixture = Fixture::new();
        let repo = fixture.repository();
        let config_path = repo.path().join("config");
        let before = fs::read(&config_path).unwrap();
        let mut settings = read(&fixture.root).unwrap();
        settings.user_name = "must not be saved".into();
        let original_permissions = fs::metadata(&config_path).unwrap().permissions();
        let mut readonly = original_permissions.clone();
        readonly.set_readonly(true);
        fs::set_permissions(&config_path, readonly).unwrap();
        let result = save(&fixture.root, &settings);
        fs::set_permissions(&config_path, original_permissions).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read(config_path).unwrap(), before);
        assert!(!repo.path().join("config.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn restrictive_config_permissions_survive_atomic_replacement() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let config_path = fixture.repository().path().join("config");
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut settings = read(&fixture.root).unwrap();
        settings.user_name = "permissions fixture".into();
        save(&fixture.root, &settings).unwrap();
        assert_eq!(
            fs::metadata(config_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn defaults_follow_the_actual_remote_and_transport_and_explicit_auth_wins() {
        let fixture = Fixture::new();
        let repo = fixture.repository();
        let https = "https://fetch.example.test/repo.git";
        let ssh = "git@push.example.test:repo.git";
        repo.remote("origin", https).unwrap();
        repo.remote("other", "https://other.example.test/repo.git")
            .unwrap();
        let key_path = fixture.root.join("private-key-not-read-during-config");
        let mut settings = read(&fixture.root).unwrap();
        let origin = settings
            .remotes
            .iter_mut()
            .find(|remote| remote.name == "origin")
            .unwrap();
        origin.push_url = Some(ssh.into());
        origin.https_username = "saved@example.test".into();
        origin.ssh_private_key = Some(key_path.clone());
        save(&fixture.root, &settings).unwrap();

        let mut fetch_auth = remote_auth(&repo, "origin", https, &RemoteAuth::default()).unwrap();
        fetch_auth.bind_remote_url(https).unwrap();
        let challenge = fetch_auth.challenge().unwrap();
        assert_eq!(challenge.username.as_deref(), Some("saved@example.test"));
        assert!(challenge.private_key.is_none());
        let mut push_auth = remote_auth(&repo, "origin", ssh, &RemoteAuth::default()).unwrap();
        push_auth.bind_remote_url(ssh).unwrap();
        let challenge = push_auth.challenge().unwrap();
        assert_eq!(challenge.username.as_deref(), Some("git"));
        assert_eq!(challenge.private_key, Some(key_path));
        assert!(
            push_auth
                .credentials(ssh, Some("git"), git2::CredentialType::USERNAME)
                .is_ok()
        );
        // The saved key cannot be sent after a callback requests another host.
        assert!(
            push_auth
                .credentials(
                    "git@other.example.test:repo.git",
                    Some("git"),
                    git2::CredentialType::USERNAME
                )
                .is_err()
        );

        let explicit = RemoteAuth::user_password("prompt@example.test", "memory-only-secret");
        let mut auth = remote_auth(&repo, "origin", https, &explicit).unwrap();
        auth.bind_remote_url(https).unwrap();
        assert_eq!(
            auth.challenge().unwrap().username.as_deref(),
            Some("prompt@example.test")
        );
        assert!(
            auth.credentials(https, None, git2::CredentialType::USER_PASS_PLAINTEXT)
                .is_ok()
        );
        for (name, url) in [
            ("other", "https://other.example.test/repo.git"),
            ("origin", "https://unselected.example.test/repo.git"),
        ] {
            let mut auth = remote_auth(&repo, name, url, &RemoteAuth::default()).unwrap();
            auth.bind_remote_url(url).unwrap();
            let challenge = auth.challenge().unwrap();
            assert!(challenge.username.is_none());
            assert!(challenge.private_key.is_none());
        }
        let another_repository = Fixture::new();
        another_repository
            .repository()
            .remote("origin", https)
            .unwrap();
        let mut auth = remote_auth(
            &another_repository.repository(),
            "origin",
            https,
            &RemoteAuth::default(),
        )
        .unwrap();
        auth.bind_remote_url(https).unwrap();
        assert!(auth.challenge().unwrap().username.is_none());
        assert!(
            !fs::read_to_string(repo.path().join("config"))
                .unwrap()
                .contains("memory-only-secret")
        );
    }
}
