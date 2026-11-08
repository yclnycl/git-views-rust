//! Deterministic TestPlatform windows; never initializes a desktop application.

use gpui_kit::{
    App, Bounds, Entity, Pixels, Point, Render, Size, TestAppContext, Window, WindowBounds,
    WindowHandle, WindowOptions, base::Root,
};

pub(super) fn open_window<V: Render>(
    cx: &mut TestAppContext,
    size: Size<Pixels>,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
) -> (WindowHandle<Root>, Entity<V>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        super::theme::initialize(gitviews_service::ThemePreference::Dark, cx);
        super::initialize_syntax();
        let (window, content) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size,
                })),
                ..Default::default()
            },
            cx,
            build,
        )
        .expect("create a simulated GPUI window");
        (
            window.downcast::<Root>().expect("production Base Root"),
            content,
        )
    })
}
