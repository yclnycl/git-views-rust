//! First-run engine preference, isolated from the existing copyable UI settings.
use crate::{GitBackend, RepositoryService};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    error::Error,
    fmt, fs,
    fs::OpenOptions,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitEngineKind {
    BuiltIn,
    System,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitEnginePreferences {
    /// Missing on the first run; selecting an engine is an explicit UI step.
    pub engine: Option<GitEngineKind>,
    pub executable: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemGitInstallation {
    pub executable: PathBuf,
    pub version: String,
}

#[derive(Clone, Debug)]
pub struct GitEngineError {
    pub message: String,
}
impl GitEngineError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}
impl fmt::Display for GitEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl Error for GitEngineError {}

fn validate_shape(preferences: &GitEnginePreferences) -> Result<(), GitEngineError> {
    match preferences.engine {
        Some(GitEngineKind::System) => {
            let path = preferences
                .executable
                .as_ref()
                .ok_or_else(|| GitEngineError::new("请选择本地 Git 可执行文件。"))?;
            if !path.is_absolute() {
                return Err(GitEngineError::new("Git 可执行文件必须使用绝对路径。"));
            }
            #[cfg(windows)]
            if !path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
            {
                return Err(GitEngineError::new(
                    "请选择 Git 的 .exe 可执行文件，例如 Git\\cmd\\git.exe。",
                ));
            }
        }
        None | Some(GitEngineKind::BuiltIn) => {}
    }
    Ok(())
}

/// Validate an explicitly selected executable without changing any Git settings.
pub fn validate_system_git(
    path: impl AsRef<Path>,
) -> Result<SystemGitInstallation, GitEngineError> {
    let path = path.as_ref();
    validate_shape(&GitEnginePreferences {
        engine: Some(GitEngineKind::System),
        executable: Some(path.to_owned()),
    })?;
    if !path.is_file() {
        return Err(GitEngineError::new("所选 Git 可执行文件不存在。"));
    }
    let executable =
        fs::canonicalize(path).map_err(|_| GitEngineError::new("无法读取 Git 可执行文件。"))?;
    let mut command = Command::new(&executable);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command
        .spawn()
        .map_err(|_| GitEngineError::new("无法运行所选 Git 可执行文件。"))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout.by_ref().take(4097).read_to_end(&mut output);
        let _ = sender.send(output);
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() < Duration::from_secs(3) => {
                std::thread::sleep(Duration::from_millis(15))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitEngineError::new(
                    "检测 Git 版本超时或失败，请选择有效的 Git 可执行文件。",
                ));
            }
        }
    };
    let bytes = receiver
        .recv_timeout(Duration::from_millis(500))
        .map_err(|_| GitEngineError::new("Git 版本输出异常。"))?;
    let output = std::str::from_utf8(&bytes)
        .map_err(|_| GitEngineError::new("Git 版本输出不是 UTF-8。"))?
        .trim();
    let version = output
        .strip_prefix("git version ")
        .filter(|_| status.success())
        .ok_or_else(|| GitEngineError::new("所选文件不是有效的 Git 可执行文件。"))?;
    let mut parts = version.split('.');
    let major = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    let minor = parts
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    if major < 2 || major == 2 && minor < 28 {
        return Err(GitEngineError::new("请使用 Git 2.28 或更新版本。"));
    }
    if bytes.len() > 4096 || version.contains(['\r', '\n', '\0']) {
        return Err(GitEngineError::new("Git 版本输出异常。"));
    }
    Ok(SystemGitInstallation {
        executable,
        version: version.to_owned(),
    })
}

/// Detect Git from PATH and ordinary installation directories. Invalid entries
/// are omitted; manual selection reports precise validation errors separately.
pub fn detect_system_git() -> Vec<SystemGitInstallation> {
    let mut candidates = Vec::new();
    let name = if cfg!(windows) { "git.exe" } else { "git" };
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(
            std::env::split_paths(&path)
                .filter(|path| path.is_absolute())
                .map(|path| path.join(name)),
        );
    }
    #[cfg(windows)]
    for (variable, suffixes) in [
        ("ProgramFiles", &["Git/cmd/git.exe", "Git/bin/git.exe"][..]),
        (
            "ProgramFiles(x86)",
            &["Git/cmd/git.exe", "Git/bin/git.exe"][..],
        ),
        (
            "LOCALAPPDATA",
            &["Programs/Git/cmd/git.exe", "Programs/Git/bin/git.exe"][..],
        ),
    ] {
        if let Some(base) = std::env::var_os(variable) {
            candidates.extend(
                suffixes
                    .iter()
                    .map(|suffix| PathBuf::from(&base).join(suffix)),
            );
        }
    }
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter_map(|path| {
            let canonical = fs::canonicalize(path).ok()?;
            let key = if cfg!(windows) {
                canonical.to_string_lossy().to_lowercase()
            } else {
                canonical.to_string_lossy().into_owned()
            };
            if !seen.insert(key) {
                return None;
            }
            validate_system_git(canonical).ok()
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct GitEngineStore {
    path: PathBuf,
}
impl GitEngineStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn load(&self) -> Result<GitEnginePreferences, GitEngineError> {
        let mut file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(GitEnginePreferences::default());
            }
            Err(error) => {
                return Err(GitEngineError::new(format!(
                    "无法读取 Git 引擎配置：{error}"
                )));
            }
        };
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(16 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| GitEngineError::new(format!("无法读取 Git 引擎配置：{error}")))?;
        if bytes.len() > 16 * 1024 {
            return Err(GitEngineError::new("Git 引擎配置超过 16 KiB。"));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| GitEngineError::new("Git 引擎配置必须为 UTF-8。"))?;
        let preferences: GitEnginePreferences = toml::from_str(text)
            .map_err(|error| GitEngineError::new(format!("Git 引擎配置格式错误：{error}")))?;
        validate_shape(&preferences)?;
        Ok(preferences)
    }
    pub fn save(&self, preferences: &GitEnginePreferences) -> Result<(), GitEngineError> {
        validate_shape(preferences)?;
        self.load()?; // Preserve malformed existing evidence rather than replacing it.
        let text = toml::to_string_pretty(preferences)
            .map_err(|error| GitEngineError::new(error.to_string()))?;
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|error| GitEngineError::new(format!("无法创建 Git 引擎配置目录：{error}")))?;
        let temporary = parent.join(format!(
            ".git-engine-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| GitEngineError::new(format!("无法创建 Git 引擎配置：{error}")))?;
        let result = file
            .write_all(text.as_bytes())
            .and_then(|()| file.sync_all());
        drop(file);
        let result = result.and_then(|()| fs::rename(&temporary, &self.path));
        let _ = fs::remove_file(&temporary);
        result.map_err(|error| GitEngineError::new(format!("无法保存 Git 引擎配置：{error}")))
    }
    /// Explicit recovery preserves malformed configuration under a unique name.
    pub fn backup_invalid(&self) -> Result<Option<PathBuf>, GitEngineError> {
        let mut file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(GitEngineError::new(format!(
                    "无法读取需要备份的 Git 引擎配置：{error}"
                )));
            }
        };
        let metadata = file
            .metadata()
            .map_err(|error| GitEngineError::new(format!("无法检查 Git 引擎配置：{error}")))?;
        if !metadata.is_file() {
            return Err(GitEngineError::new(
                "Git 引擎配置路径不是普通文件，无法自动备份。",
            ));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(16 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                GitEngineError::new(format!("无法读取需要备份的 Git 引擎配置：{error}"))
            })?;
        let valid = bytes.len() <= 16 * 1024
            && std::str::from_utf8(&bytes)
                .ok()
                .and_then(|text| toml::from_str::<GitEnginePreferences>(text).ok())
                .is_some_and(|preferences| validate_shape(&preferences).is_ok());
        if valid {
            return Ok(None);
        }
        drop(file);
        let current = fs::metadata(&self.path)
            .map_err(|error| GitEngineError::new(format!("无法检查 Git 引擎配置：{error}")))?;
        if current.len() != metadata.len() || current.modified().ok() != metadata.modified().ok() {
            return Err(GitEngineError::new(
                "Git 引擎配置已被其他程序修改，请重新读取后再备份。",
            ));
        }
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = self
            .path
            .file_name()
            .ok_or_else(|| GitEngineError::new("Git 引擎配置文件名无效。"))?
            .to_string_lossy();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| GitEngineError::new("系统时钟无效，无法生成备份文件名。"))?
            .as_nanos();
        let backup = parent.join(format!(
            "{name}.invalid-{stamp}-{}-{}.bak",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        if backup.exists() {
            return Err(GitEngineError::new("备份文件已存在，请重试。"));
        }
        fs::rename(&self.path, &backup)
            .map_err(|error| GitEngineError::new(format!("无法备份 Git 引擎配置：{error}")))?;
        Ok(Some(backup))
    }
}
static NEXT: AtomicU64 = AtomicU64::new(0);

impl<B: GitBackend> RepositoryService<B> {
    /// Validate first, then switch every cloned handle together. Never fall back
    /// to native writes when a previously selected installed Git disappears.
    pub fn configure_git_engine(
        &self,
        preferences: &GitEnginePreferences,
    ) -> Result<(), GitEngineError> {
        let preferences = validated_preferences(preferences)?;
        let _guard = self
            .remote_gate
            .try_lock()
            .map_err(|_| GitEngineError::new("已有 Git 操作正在执行，完成后才能切换引擎。"))?;
        *self
            .git_engine
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = preferences;
        Ok(())
    }
    pub fn git_engine_preferences(&self) -> GitEnginePreferences {
        self.git_engine
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    /// Disk failures leave the active engine untouched. All service clones
    /// share the write gate across persistence and activation.
    pub fn save_git_engine(
        &self,
        store: &GitEngineStore,
        preferences: &GitEnginePreferences,
    ) -> Result<(), GitEngineError> {
        let preferences = validated_preferences(preferences)?;
        let _guard = self
            .remote_gate
            .try_lock()
            .map_err(|_| GitEngineError::new("已有 Git 操作正在执行，完成后才能切换引擎。"))?;
        store.save(&preferences)?;
        *self
            .git_engine
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = preferences;
        Ok(())
    }
    pub(crate) fn configured_system_git(&self) -> Option<gitviews_git::SystemGit> {
        let preferences = self.git_engine_preferences();
        if preferences.engine == Some(GitEngineKind::System) {
            preferences.executable.map(gitviews_git::SystemGit::new)
        } else {
            None
        }
    }
}

fn validated_preferences(
    preferences: &GitEnginePreferences,
) -> Result<GitEnginePreferences, GitEngineError> {
    validate_shape(preferences)?;
    let mut preferences = preferences.clone();
    if preferences.engine == Some(GitEngineKind::System) {
        preferences.executable = Some(
            validate_system_git(
                preferences
                    .executable
                    .as_ref()
                    .expect("validated executable"),
            )?
            .executable,
        );
    }
    Ok(preferences)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_preference_requires_a_choice_and_invalid_existing_file_is_preserved() {
        let directory = std::env::temp_dir().join(format!(
            "gitviews-engine-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let store = GitEngineStore::new(directory.join("git-engine.toml"));
        assert_eq!(store.load().unwrap(), GitEnginePreferences::default());
        let preferences = GitEnginePreferences {
            engine: Some(GitEngineKind::BuiltIn),
            executable: None,
        };
        store.save(&preferences).unwrap();
        assert_eq!(store.load().unwrap(), preferences);
        fs::write(store.path(), "broken = [").unwrap();
        assert!(store.save(&preferences).is_err());
        assert_eq!(fs::read_to_string(store.path()).unwrap(), "broken = [");
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn invalid_system_selection_does_not_change_shared_engine() {
        let service = RepositoryService::default();
        let clone = service.clone();
        let native = GitEnginePreferences {
            engine: Some(GitEngineKind::BuiltIn),
            executable: None,
        };
        service.configure_git_engine(&native).unwrap();
        assert_eq!(clone.git_engine_preferences(), native);
        assert!(
            service
                .configure_git_engine(&GitEnginePreferences {
                    engine: Some(GitEngineKind::System),
                    executable: Some(PathBuf::from("missing.exe"))
                })
                .is_err()
        );
        assert_eq!(clone.git_engine_preferences(), native);
    }
    #[test]
    fn failed_store_preserves_active_engine_and_recovery_preserves_invalid_bytes() {
        let directory = std::env::temp_dir().join(format!(
            "gitviews-engine-save-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let store = GitEngineStore::new(directory.join("git-engine.toml"));
        let service = RepositoryService::default();
        fs::write(store.path(), "engine = \"unrecognized\"\n").unwrap();
        let original = service.git_engine_preferences();
        let selected = GitEnginePreferences {
            engine: Some(GitEngineKind::BuiltIn),
            executable: None,
        };
        assert!(service.save_git_engine(&store, &selected).is_err());
        assert_eq!(service.git_engine_preferences(), original);
        let backup = store.backup_invalid().unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(backup).unwrap(),
            "engine = \"unrecognized\"\n"
        );
        assert!(!store.path().exists());
        service.save_git_engine(&store, &selected).unwrap();
        assert_eq!(service.git_engine_preferences(), selected);
        assert_eq!(store.load().unwrap(), selected);
        assert!(store.backup_invalid().unwrap().is_none());
        fs::remove_dir_all(directory).unwrap();
    }
}
