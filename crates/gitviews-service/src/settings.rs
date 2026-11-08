//! Validated application preferences and durable, bounded TOML storage.
//!
//! The desktop supplies the path, applies preferences and serializes saves.
//! Loading an invalid file never rewrites it or silently returns defaults.

use std::{
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

pub const SETTINGS_VERSION: u32 = 1;
pub const MAX_SETTINGS_BYTES: u64 = 64 * 1024;

/// All values have defaults, so a hand-written file may override only one field.
/// Unknown fields are rejected, including in nested sections, to expose typos.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppSettings {
    #[serde(default = "default_version")]
    pub version: u32,
    /// Observe repositories without exposing or executing Git mutations.
    pub read_only: bool,
    pub appearance: AppearanceSettings,
    pub preview: PreviewSettings,
    pub refresh: RefreshSettings,
    pub logging: LoggingSettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            read_only: true,
            appearance: AppearanceSettings::default(),
            preview: PreviewSettings::default(),
            refresh: RefreshSettings::default(),
            logging: LoggingSettings::default(),
        }
    }
}

const fn default_version() -> u32 {
    SETTINGS_VERSION
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    System,
    Light,
    #[default]
    Dark,
}

impl ThemePreference {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppearanceSettings {
    pub theme: ThemePreference,
    pub editor_font_size: f32,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: ThemePreference::Dark,
            editor_font_size: 12.5,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PreviewSettings {
    pub cache_items: usize,
    pub cache_mib: usize,
    pub retained_panes: usize,
    pub retained_mib: usize,
}

impl Default for PreviewSettings {
    fn default() -> Self {
        Self {
            cache_items: 48,
            cache_mib: 32,
            retained_panes: 8,
            retained_mib: 32,
        }
    }
}

impl PreviewSettings {
    pub fn cache_limits(self) -> crate::PreviewCacheLimits {
        crate::PreviewCacheLimits {
            max_entries: self.cache_items,
            max_bytes: self.cache_mib.saturating_mul(1024 * 1024),
        }
    }

    pub fn retained_bytes(self) -> usize {
        self.retained_mib.saturating_mul(1024 * 1024)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RefreshSettings {
    pub scan_interval_secs: u64,
    pub debounce_ms: u64,
}

impl Default for RefreshSettings {
    fn default() -> Self {
        Self {
            scan_interval_secs: 10,
            debounce_ms: 300,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    #[default]
    Debug,
    Trace,
}

impl LogLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingSettings {
    pub level: LogLevel,
}

impl AppSettings {
    pub fn validate(&self) -> Result<(), SettingsError> {
        if self.version != SETTINGS_VERSION {
            return Err(SettingsError::InvalidValue {
                field: "version",
                message: format!("仅支持配置版本 {SETTINGS_VERSION}，当前为 {}", self.version),
            });
        }
        if !self.appearance.editor_font_size.is_finite() {
            return Err(SettingsError::InvalidValue {
                field: "appearance.editor_font_size",
                message: "应为 10 到 24 之间的有限数值".into(),
            });
        }
        check_range(
            "appearance.editor_font_size",
            self.appearance.editor_font_size,
            10.0,
            24.0,
        )?;
        check_range("preview.cache_items", self.preview.cache_items, 1, 256)?;
        check_range("preview.cache_mib", self.preview.cache_mib, 8, 256)?;
        check_range("preview.retained_panes", self.preview.retained_panes, 1, 32)?;
        check_range("preview.retained_mib", self.preview.retained_mib, 8, 256)?;
        check_range(
            "refresh.scan_interval_secs",
            self.refresh.scan_interval_secs,
            2,
            300,
        )?;
        check_range("refresh.debounce_ms", self.refresh.debounce_ms, 50, 2000)?;
        Ok(())
    }

    pub fn from_toml(text: &str) -> Result<Self, SettingsError> {
        if text.len() as u64 > MAX_SETTINGS_BYTES {
            return Err(SettingsError::TooLarge);
        }
        let settings: Self = toml::from_str(text).map_err(SettingsError::Parse)?;
        settings.validate()?;
        Ok(settings)
    }

    pub fn to_toml(&self) -> Result<String, SettingsError> {
        self.validate()?;
        let body = toml::to_string_pretty(self).map_err(SettingsError::Serialize)?;
        Ok(format!(
            "# GitViews preferences. Save in Settings, or edit this file and click Reload in Settings.\n\
             # Themes: system, light, dark. Memory budgets use MiB (1024 * 1024 bytes).\n\
             # Log levels: off, error, warn, info, debug, trace.\n\n{body}"
        ))
    }
}

fn check_range<T: Copy + PartialOrd + fmt::Display>(
    field: &'static str,
    value: T,
    min: T,
    max: T,
) -> Result<(), SettingsError> {
    if value < min || value > max {
        return Err(SettingsError::InvalidValue {
            field,
            message: format!("应在 {min} 到 {max} 之间，当前为 {value}"),
        });
    }
    Ok(())
}

/// Save through a synced temporary file in the destination directory, then one
/// replacement rename. Invalid existing files are preserved. Concurrent desktop
/// instances do not merge preferences; callers serialize their own save tasks.
#[derive(Clone, Debug)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Missing files return defaults without creating a directory or file.
    pub fn load(&self) -> Result<AppSettings, SettingsError> {
        self.read_existing().map(Option::unwrap_or_default)
    }

    /// Used at startup to materialize a readable template on the first run.
    pub fn load_or_create(&self) -> Result<AppSettings, SettingsError> {
        if let Some(settings) = self.read_existing()? {
            return Ok(settings);
        }
        let settings = AppSettings::default();
        self.save(&settings)?;
        Ok(settings)
    }

    pub fn save(&self, settings: &AppSettings) -> Result<(), SettingsError> {
        let text = settings.to_toml()?;
        // A malformed configuration is evidence the user may need to repair.
        // Even direct save calls must not destroy it by writing defaults.
        self.read_existing()?;
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|source| self.io_error("创建配置目录", parent, source))?;
        let (mut file, temporary) = create_temporary(parent)?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|source| self.io_error("写入临时配置", &temporary.path, source))?;
        // Windows replacement requires closing our temporary handle first.
        drop(file);
        fs::rename(&temporary.path, &self.path)
            .map_err(|source| self.io_error("替换配置文件", &self.path, source))?;
        Ok(())
    }

    fn read_existing(&self) -> Result<Option<AppSettings>, SettingsError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(self.io_error("读取配置文件", &self.path, source)),
        };
        if file
            .metadata()
            .map_err(|source| self.io_error("读取配置大小", &self.path, source))?
            .len()
            > MAX_SETTINGS_BYTES
        {
            return Err(SettingsError::TooLarge);
        }
        let mut bytes = Vec::new();
        file.take(MAX_SETTINGS_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| self.io_error("读取配置文件", &self.path, source))?;
        if bytes.len() as u64 > MAX_SETTINGS_BYTES {
            return Err(SettingsError::TooLarge);
        }
        let text = std::str::from_utf8(&bytes).map_err(SettingsError::Utf8)?;
        AppSettings::from_toml(text).map(Some)
    }

    fn io_error(&self, operation: &'static str, path: &Path, source: io::Error) -> SettingsError {
        SettingsError::Io {
            operation,
            path: path.to_owned(),
            source,
        }
    }
}

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

struct TemporarySettings {
    path: PathBuf,
}

impl Drop for TemporarySettings {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn create_temporary(parent: &Path) -> Result<(File, TemporarySettings), SettingsError> {
    for _ in 0..32 {
        let suffix = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".gitviews-settings-{}-{suffix}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, TemporarySettings { path })),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(SettingsError::Io {
                    operation: "创建临时配置",
                    path,
                    source,
                });
            }
        }
    }
    Err(SettingsError::Io {
        operation: "创建临时配置",
        path: parent.to_owned(),
        source: io::Error::new(io::ErrorKind::AlreadyExists, "无法分配唯一临时文件"),
    })
}

#[derive(Debug)]
pub enum SettingsError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
    Utf8(std::str::Utf8Error),
    InvalidValue {
        field: &'static str,
        message: String,
    },
    TooLarge,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "{operation}失败（{}）：{source}",
                crate::display_path(path)
            ),
            Self::Parse(source) => write!(formatter, "TOML 配置格式错误：{source}"),
            Self::Serialize(source) => write!(formatter, "无法生成 TOML 配置：{source}"),
            Self::Utf8(_) => write!(formatter, "配置文件必须使用 UTF-8 编码"),
            Self::InvalidValue { field, message } => {
                write!(formatter, "配置项 {field} 无效：{message}")
            }
            Self::TooLarge => write!(formatter, "配置文件超过 64 KiB 大小限制"),
        }
    }
}

impl Error for SettingsError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse(source) => Some(source),
            Self::Serialize(source) => Some(source),
            Self::Utf8(source) => Some(source),
            Self::InvalidValue { .. } | Self::TooLarge => None,
        }
    }
}

#[cfg(test)]
mod tests;
