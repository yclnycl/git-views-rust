use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/settings-tests");
        fs::create_dir_all(&parent).unwrap();
        let path = parent.join(format!(
            "settings-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn store(&self) -> SettingsStore {
        SettingsStore::new(self.0.join("中文 配置").join("config.toml"))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let expected_parent =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/settings-tests");
        // Both paths are generated inside this dedicated build-output root.
        if self.0.parent() == Some(expected_parent.as_path()) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

#[test]
fn default_settings_are_read_only_and_round_trip() {
    let settings = AppSettings::default();
    assert_eq!(settings.version, 1);
    assert!(settings.read_only);
    assert_eq!(settings.appearance.theme, ThemePreference::Dark);
    assert_eq!(settings.appearance.editor_font_size, 12.5);
    assert_eq!(settings.preview.cache_limits().max_entries, 48);
    assert_eq!(settings.preview.cache_limits().max_bytes, 32 * 1024 * 1024);
    assert_eq!(settings.preview.retained_panes, 8);
    assert_eq!(settings.preview.retained_bytes(), 32 * 1024 * 1024);
    assert_eq!(settings.refresh.scan_interval_secs, 10);
    assert_eq!(settings.refresh.debounce_ms, 300);
    assert_eq!(settings.logging.level, LogLevel::Debug);
    let text = settings.to_toml().unwrap();
    assert!(text.contains("version = 1"));
    assert!(text.contains("[appearance]"));
    assert_eq!(AppSettings::from_toml(&text).unwrap(), settings);
}

#[test]
fn old_configs_default_to_read_only_and_advanced_mode_is_persisted() {
    let old = AppSettings::from_toml("version = 1\n[appearance]\ntheme = 'light'\n").unwrap();
    assert!(old.read_only);
    let directory = TestDirectory::new();
    let store = directory.store();
    let mut settings = old;
    settings.read_only = false;
    store.save(&settings).unwrap();
    assert!(!store.load().unwrap().read_only);
    assert_eq!(
        store.load().unwrap().appearance.theme,
        ThemePreference::Light
    );
}

#[test]
fn documented_toml_example_is_valid_and_matches_defaults() {
    let example = include_str!("../../../../config.example.toml");
    assert_eq!(
        AppSettings::from_toml(example).unwrap(),
        AppSettings::default()
    );
}

#[test]
fn hand_written_partial_sections_inherit_defaults() {
    let settings = AppSettings::from_toml(
        "[appearance]\ntheme = 'system'\n[refresh]\nscan_interval_secs = 30\n",
    )
    .unwrap();
    assert_eq!(settings.appearance.theme, ThemePreference::System);
    assert_eq!(settings.appearance.editor_font_size, 12.5);
    assert_eq!(settings.refresh.scan_interval_secs, 30);
    assert_eq!(settings.refresh.debounce_ms, 300);
    assert_eq!(settings.preview, PreviewSettings::default());
    assert_eq!(AppSettings::from_toml("").unwrap(), AppSettings::default());
}

#[test]
fn unknown_fields_types_enums_and_versions_are_rejected() {
    for text in [
        "unknown = true",
        "[appearance]\ntheme_typo = 'dark'",
        "[preview]\ncache_mb = 32",
        "[refresh]\nscan_interval_secs = '10'",
        "[logging]\nlevel = 'verbose'",
        "[appearance]\ntheme = 'night'",
        "version = 2",
        "version = 0",
        "[preview]\ncache_items = -1",
    ] {
        assert!(AppSettings::from_toml(text).is_err(), "accepted {text}");
    }
}

#[test]
fn all_numeric_limits_and_non_finite_font_sizes_are_rejected() {
    for text in [
        "[appearance]\neditor_font_size = 9.5",
        "[appearance]\neditor_font_size = 24.5",
        "[appearance]\neditor_font_size = nan",
        "[appearance]\neditor_font_size = inf",
        "[appearance]\neditor_font_size = -inf",
        "[preview]\ncache_items = 0",
        "[preview]\ncache_items = 257",
        "[preview]\ncache_mib = 7",
        "[preview]\ncache_mib = 257",
        "[preview]\nretained_panes = 0",
        "[preview]\nretained_panes = 33",
        "[preview]\nretained_mib = 7",
        "[preview]\nretained_mib = 257",
        "[refresh]\nscan_interval_secs = 1",
        "[refresh]\nscan_interval_secs = 301",
        "[refresh]\ndebounce_ms = 49",
        "[refresh]\ndebounce_ms = 2001",
    ] {
        assert!(AppSettings::from_toml(text).is_err(), "accepted {text}");
    }
    let mut settings = AppSettings::default();
    settings.appearance.editor_font_size = f32::NAN;
    assert!(settings.to_toml().is_err());
}

#[test]
fn inclusive_numeric_boundaries_round_trip() {
    for (font, cache_items, mib, panes, interval, debounce) in
        [(10.0, 1, 8, 1, 2, 50), (24.0, 256, 256, 32, 300, 2000)]
    {
        let mut settings = AppSettings::default();
        settings.appearance.editor_font_size = font;
        settings.preview = PreviewSettings {
            cache_items,
            cache_mib: mib,
            retained_panes: panes,
            retained_mib: mib,
        };
        settings.refresh = RefreshSettings {
            scan_interval_secs: interval,
            debounce_ms: debounce,
        };
        assert_eq!(
            AppSettings::from_toml(&settings.to_toml().unwrap()).unwrap(),
            settings
        );
    }
}

#[test]
fn first_run_only_creates_a_template_when_requested() {
    let directory = TestDirectory::new();
    let store = directory.store();
    assert_eq!(store.load().unwrap(), AppSettings::default());
    assert!(!store.path().exists());
    assert_eq!(store.load_or_create().unwrap(), AppSettings::default());
    assert!(store.path().is_file());
    assert!(
        fs::read_to_string(store.path())
            .unwrap()
            .starts_with("# GitViews preferences")
    );
    assert_eq!(store.load().unwrap(), AppSettings::default());
}

#[test]
fn valid_save_replaces_existing_file_and_removes_temporary_files() {
    let directory = TestDirectory::new();
    let store = directory.store();
    store.load_or_create().unwrap();
    let mut settings = AppSettings::default();
    settings.appearance.theme = ThemePreference::Light;
    settings.appearance.editor_font_size = 15.5;
    settings.logging.level = LogLevel::Trace;
    store.save(&settings).unwrap();
    assert_eq!(store.load_or_create().unwrap(), settings);
    assert_eq!(
        fs::read_dir(store.path().parent().unwrap())
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn malformed_existing_file_is_preserved_by_load_and_save() {
    let directory = TestDirectory::new();
    let store = directory.store();
    fs::create_dir_all(store.path().parent().unwrap()).unwrap();
    for bytes in [
        b"[appearance\ntheme='dark'".as_slice(),
        b"[preview]\ncache_items=0",
        b"version=99",
        b"[logging]\nlevle='debug'",
    ] {
        fs::write(store.path(), bytes).unwrap();
        assert!(store.load().is_err());
        assert!(store.load_or_create().is_err());
        assert!(store.save(&AppSettings::default()).is_err());
        assert_eq!(fs::read(store.path()).unwrap(), bytes);
    }
}

#[test]
fn invalid_new_values_do_not_touch_the_previous_file() {
    let directory = TestDirectory::new();
    let store = directory.store();
    store.load_or_create().unwrap();
    let previous = fs::read(store.path()).unwrap();
    let mut settings = AppSettings::default();
    settings.preview.retained_panes = usize::MAX;
    assert!(store.save(&settings).is_err());
    assert_eq!(fs::read(store.path()).unwrap(), previous);
}

#[test]
fn non_utf8_and_oversized_files_are_rejected_and_preserved() {
    let directory = TestDirectory::new();
    let store = directory.store();
    fs::create_dir_all(store.path().parent().unwrap()).unwrap();
    for bytes in [
        vec![0xff, 0xfe, b'v'],
        vec![b' '; MAX_SETTINGS_BYTES as usize + 1],
    ] {
        fs::write(store.path(), &bytes).unwrap();
        assert!(store.load().is_err());
        assert!(store.save(&AppSettings::default()).is_err());
        assert_eq!(fs::read(store.path()).unwrap(), bytes);
    }
    assert!(matches!(
        AppSettings::from_toml(&" ".repeat(MAX_SETTINGS_BYTES as usize + 1)),
        Err(SettingsError::TooLarge)
    ));
}

#[test]
fn directory_io_failure_does_not_become_default_configuration() {
    let directory = TestDirectory::new();
    let path = directory.0.join("directory-is-not-config");
    fs::create_dir(&path).unwrap();
    let store = SettingsStore::new(&path);
    assert!(store.load().is_err());
    assert!(store.load_or_create().is_err());
    assert!(store.save(&AppSettings::default()).is_err());
    assert!(path.is_dir());
}
