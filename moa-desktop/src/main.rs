// Platform requirements:
// - macOS: full Xcode.app (for `xcrun metal` — Command Line Tools alone is insufficient)
// - Linux: libxkbcommon-dev, libwayland-dev, Vulkan drivers (wgpu backend)

mod actions;
mod app;
mod components;
mod density;
mod keybindings;
mod layout;
mod notifications;
mod panels;
mod services;
mod statusbar;
mod streaming;
mod theme;
mod theme_tokens;
mod titlebar;
mod tray;
mod wcag;
mod window_state;

use gpui::{
    App, AppContext, Application, Bounds, Point, TitlebarOptions, WindowBounds, WindowOptions, px,
    size,
};
use gpui_component::Root;

use crate::app::MoaApp;
use crate::services::{
    ServiceBridge, ServiceBridgeHandle, bridge::spawn_into, init::initialize_services,
};
use crate::window_state::WindowState;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                // `async_openai::error` logs deserialization failures for streaming
                // items it doesn't recognize yet (e.g. `web_search_call.added` in 0.34);
                // those are already gracefully skipped by moa-providers, so the raw
                // SDK log is noise — silence it.
                tracing_subscriber::EnvFilter::new(
                    "info,moa_desktop=debug,async_openai::error=off",
                )
            }),
        )
        .init();

    Application::new().run(|cx: &mut App| {
        theme::setup_theme(cx);
        keybindings::register_keybindings(cx);

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

        // Restore prior window position/size (falls back to centered defaults
        // on first run). Clamp against a generous 4K-ish bound so a stale
        // state file from a disconnected external monitor can't park the
        // window off-screen — the OS still does final positioning.
        let mut saved = WindowState::load_or_default();
        saved.validate_bounds(4096.0, 2160.0);
        let bounds = Bounds {
            origin: Point {
                x: px(saved.x),
                y: px(saved.y),
            },
            size: size(px(saved.width), px(saved.height)),
        };
        let window_handle = cx
            .open_window(
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
                    let app = cx.new(|cx| MoaApp::new(saved.clone(), window, cx));
                    cx.new(|cx| Root::new(app, window, cx))
                },
            )
            .unwrap();

        // Close-to-tray: intercept the window close and hide the app
        // instead. Returning `false` aborts the close; `cx.hide()` hides
        // every window at the NSApplication level (macOS), which is the
        // standard "minimize to tray" behavior for apps that live in the
        // menu bar. Full quit is available via Cmd+Q or the tray menu.
        let _ = window_handle.update(cx, |_, window, cx| {
            window.on_window_should_close(cx, |window, cx| {
                // One-time toast on first close so users don't think the
                // app has quit when it's actually still in the tray.
                let mut state = WindowState::load_or_default();
                if !state.close_to_tray_shown {
                    notifications::info(
                        window,
                        cx,
                        "MOA is still running in the system tray.",
                    );
                    state.close_to_tray_shown = true;
                    state.save_to_default_path();
                }
                cx.hide();
                false
            });
        });

        // Install the system tray. Failures are logged but non-fatal — the
        // app still works without a tray icon; users just can't re-open a
        // hidden window except via Cmd-Tab/app switcher.
        match tray::install(cx) {
            Ok(handle) => cx.set_global(handle),
            Err(err) => tracing::warn!(%err, "failed to install system tray"),
        }

        cx.activate(true);
    });
}
