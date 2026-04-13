// Platform requirements:
// - macOS: Xcode Command Line Tools (Metal backend)
// - Linux: libxkbcommon-dev, libwayland-dev, Vulkan drivers (wgpu backend)

// Platform requirements:
// - macOS: full Xcode.app (for `xcrun metal` — Command Line Tools alone is insufficient)
// - Linux: libxkbcommon-dev, libwayland-dev, Vulkan drivers (wgpu backend)

use gpui::{
    App, Application, Bounds, Context, Window, WindowBounds, WindowOptions, div, prelude::*, px,
    rgb, size,
};

struct HelloView;

impl Render for HelloView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .justify_center()
            .items_center()
            .bg(rgb(0x1e1e2e))
            .text_xl()
            .text_color(rgb(0xcdd6f4))
            .child("Hello, MOA")
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        gpui_component::init(cx);

        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| HelloView),
        )
        .unwrap();
        cx.activate(true);
    });
}
