#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
mod diagnostics;
mod ui;

fn main() {
    let mut initial_paths = Vec::new();
    let mut preview = false;
    let mut light = false;
    let mut state_file = default_state_file();
    let mut config_file = default_state_file().with_file_name("config.toml");
    let mut arguments = std::env::args_os().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--design-preview") => preview = true,
            Some("--light") => light = true,
            Some("--state-file") => {
                if let Some(path) = arguments.next() {
                    state_file = path.into();
                }
            }
            Some("--config") => {
                if let Some(path) = arguments.next() {
                    config_file = path.into();
                }
            }
            Some("--") => {
                initial_paths.extend(arguments.map(std::path::PathBuf::from));
                break;
            }
            _ => initial_paths.push(std::path::PathBuf::from(argument)),
        }
    }
    // The small, bounded bootstrap file is read before opening a window so the
    // configured log level also covers platform/window initialization.
    let settings_store = gitviews_service::SettingsStore::new(config_file);
    let (mut settings, settings_error) = match settings_store.load_or_create() {
        Ok(settings) => (settings, None),
        Err(error) => (
            gitviews_service::AppSettings::default(),
            Some(error.to_string()),
        ),
    };
    let diagnostics = diagnostics::init(settings.logging.level);
    if settings_error.is_some() {
        tracing::warn!(target: "gitviews", config_path = %settings_store.path().display(), "Configuration unavailable; using defaults and preserving the original file");
    }
    if light {
        settings.appearance.theme = gitviews_service::ThemePreference::Light;
    }
    tracing::info!(target: "gitviews", version = env!("CARGO_PKG_VERSION"), process_id = std::process::id(), log_path = ?diagnostics.log_path(), config_path = %settings_store.path().display(), repository_count = initial_paths.len(), preview, light, "application_start");
    app::run(
        initial_paths,
        state_file,
        preview,
        settings,
        settings_store,
        settings_error,
    );
    tracing::info!(target: "gitviews", "application_exit");
}

fn default_state_file() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    let directory = std::env::var_os("APPDATA").map(std::path::PathBuf::from);
    #[cfg(target_os = "macos")]
    let directory = std::env::var_os("HOME")
        .map(|home| std::path::PathBuf::from(home).join("Library/Application Support"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let directory = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        });
    directory
        .unwrap_or_else(std::env::temp_dir)
        .join("GitViews")
        .join("repositories.json")
}
