// Platform requirements:
// - macOS: full Xcode.app (for `xcrun metal` — Command Line Tools alone is insufficient)
// - Linux: libxkbcommon-dev, libwayland-dev, Vulkan drivers (wgpu backend)

mod app;
mod components;
mod layout;
mod statusbar;
mod theme;
mod titlebar;

use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
use gpui_component::Root;

use crate::app::MoaApp;

fn main() {
    Application::new().run(|cx: &mut App| {
        theme::setup_theme(cx);

        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: Some("MOA".into()),
                    appears_transparent: true,
                    ..Default::default()
                }),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(800.), px(600.))),
                ..Default::default()
            },
            |window, cx| {
                let app = cx.new(|cx| MoaApp::new(window, cx));
                cx.new(|cx| Root::new(app, window, cx))
            },
        )
        .unwrap();
        cx.activate(true);
    });
}
