// Platform requirements:
// - macOS: full Xcode.app (for `xcrun metal` — Command Line Tools alone is insufficient)
// - Linux: libxkbcommon-dev, libwayland-dev, Vulkan drivers (wgpu backend)

mod app;
mod components;
mod layout;
mod panels;
mod services;
mod statusbar;
mod theme;
mod titlebar;

use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
use gpui_component::Root;

use crate::app::MoaApp;
use crate::services::{
    ServiceBridge, ServiceBridgeHandle, bridge::spawn_into, init::initialize_services,
};

fn main() {
    Application::new().run(|cx: &mut App| {
        theme::setup_theme(cx);

        // Bridge boots the tokio runtime immediately so it's available before
        // initialization completes. ChatRuntime construction runs asynchronously.
        let bridge = match ServiceBridge::new() {
            Ok(bridge) => bridge,
            Err(err) => {
                tracing::error!(%err, "failed to build tokio runtime");
                panic!("fatal: could not build tokio runtime: {err}");
            }
        };
        let handle = bridge.tokio_handle();
        let bridge_entity = cx.new(|_| bridge);
        cx.set_global(ServiceBridgeHandle(bridge_entity.clone()));

        // Kick off async initialization; status flips Ready/Error when it finishes.
        spawn_into(
            cx,
            handle,
            bridge_entity,
            async move { initialize_services().await },
            |bridge, result, cx| match result {
                Ok(services) => bridge.mark_ready(services, cx),
                Err(err) => bridge.mark_error(format!("{err:#}"), cx),
            },
        );

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
