use gpui_kit::{
    component::{Theme, ThemeMode},
    *,
};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub fn from_preference(value: gitviews_service::ThemePreference) -> Self {
        match value {
            gitviews_service::ThemePreference::System => Self::System,
            gitviews_service::ThemePreference::Light => Self::Light,
            gitviews_service::ThemePreference::Dark => Self::Dark,
        }
    }

    pub fn preference(self) -> gitviews_service::ThemePreference {
        match self {
            Self::System => gitviews_service::ThemePreference::System,
            Self::Light => gitviews_service::ThemePreference::Light,
            Self::Dark => gitviews_service::ThemePreference::Dark,
        }
    }
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "跟随系统",
            Self::Light => "浅色",
            Self::Dark => "深色",
        }
    }

    pub fn apply(self, window: &mut Window, cx: &mut App) {
        match self {
            Self::System => Theme::sync_system_appearance(Some(window), cx),
            Self::Light => Theme::change(ThemeMode::Light, Some(window), cx),
            Self::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
        }
        apply_design_palette(cx);
    }
}

pub fn initialize(preference: gitviews_service::ThemePreference, cx: &mut App) {
    match preference {
        gitviews_service::ThemePreference::System => Theme::sync_system_appearance(None, cx),
        gitviews_service::ThemePreference::Light => Theme::change(ThemeMode::Light, None, cx),
        gitviews_service::ThemePreference::Dark => Theme::change(ThemeMode::Dark, None, cx),
    }
    apply_design_palette(cx);
}

fn apply_design_palette(cx: &mut App) {
    Theme::update(cx, |theme| {
        let light = theme.mode == ThemeMode::Light;
        theme.font_family = "Noto Sans SC".into();
        theme.mono_font_family = "JetBrains Mono".into();
        theme.font_size = px(13.);
        theme.colors.background = rgb(if light { 0xffffff } else { 0x0d1117 }).into();
        theme.colors.foreground = rgb(if light { 0x1f2328 } else { 0xf0f6fc }).into();
        theme.colors.border = rgb(if light { 0xd0d7de } else { 0x30363d }).into();
        theme.colors.muted = rgb(if light { 0xf6f8fa } else { 0x161b22 }).into();
        theme.colors.muted_foreground = rgb(if light { 0x59636e } else { 0x9198a1 }).into();
        theme.colors.selection = rgb(if light { 0xddf4ff } else { 0x1a3453 }).into();
    });
}
