use std::path::PathBuf;

use gpui_kit::*;

use crate::ui::{Workspace, WorkspaceOptions, theme};

pub fn run(
    initial_paths: Vec<PathBuf>,
    state_file: PathBuf,
    preview: bool,
    settings: gitviews_service::AppSettings,
    settings_store: gitviews_service::SettingsStore,
    settings_error: Option<String>,
) {
    application().with_assets(assets::AllAssets).run(move |cx| {
        cx.text_system()
            .add_fonts(vec![
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/NotoSansSC-VF.ttf").as_slice(),
                ),
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf").as_slice(),
                ),
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf").as_slice(),
                ),
                std::borrow::Cow::Borrowed(
                    include_bytes!("../assets/fonts/JetBrainsMono-SemiBold.ttf").as_slice(),
                ),
            ])
            .expect("bundled design fonts must load");
        init(cx);
        crate::ui::initialize_syntax();
        theme::initialize(settings.appearance.theme, cx);
        cx.set_global(crate::ui::PreviewPreferences {
            font_size: settings.appearance.editor_font_size,
        });
        cx.on_window_closed(|cx, _window_id| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1680.), px(1000.)),
                cx,
            ))),
            window_min_size: Some(size(px(1200.), px(720.))),
            titlebar: Some(TitlebarOptions {
                title: Some("GitScope".into()),
                appears_transparent: true,
                traffic_light_position: None,
            }),
            app_owns_titlebar_drag: true,
            ..Default::default()
        };
        open_window(options, cx, |window, cx| {
            window.set_window_title("GitScope");
            cx.new(|cx| {
                Workspace::new(
                    WorkspaceOptions {
                        initial_paths,
                        state_file,
                        preview,
                        settings,
                        settings_store,
                        settings_error,
                    },
                    window,
                    cx,
                )
            })
        })
        .expect("Failed to open the GitViews native window");
        cx.activate(true);
    });
}
