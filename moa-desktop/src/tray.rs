//! System tray integration via the `tray-icon` crate.
//!
//! `tray-icon` runs its own NSStatusItem / GTK status-icon / Windows shell
//! icon alongside GPUI's event loop. Menu events are delivered through
//! crossbeam channels; we poll them from a background-executor task and
//! dispatch actions back onto the GPUI main thread.

use std::time::Duration;

use gpui::App;
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

/// Identifiers used to route menu clicks back to the right action.
struct TrayIds {
    show: MenuId,
    settings: MenuId,
    quit: MenuId,
}

/// Handle kept alive for the lifetime of the app. Dropping it removes the
/// tray icon, so we pin it in a `Global`.
///
/// `TrayIcon` is `!Send` on macOS (it wraps `NSStatusItem`), which is fine
/// because GPUI's globals live on the app's main thread and are never sent
/// across threads.
pub struct TrayHandle {
    _tray: TrayIcon,
}

impl gpui::Global for TrayHandle {}

/// Builds and installs the system tray. Returns a [`TrayHandle`] so the
/// caller can stash it in a global to keep the tray alive.
///
/// Menu clicks drive three behaviors:
/// - **Show MOA** activates the app and brings the window back (macOS
///   `NSApplication.activateIgnoringOtherApps:`).
/// - **Settings** dispatches the `OpenSettings` action to the focused
///   window so the existing settings modal opens.
/// - **Quit MOA** exits the GPUI runtime entirely.
pub fn install(cx: &mut App) -> anyhow::Result<TrayHandle> {
    let show_item = MenuItem::new("Show MOA", true, None);
    let settings_item = MenuItem::new("Settings…", true, None);
    let quit_item = MenuItem::new("Quit MOA", true, None);

    let menu = Menu::new();
    menu.append(&show_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&settings_item)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit_item)?;

    let icon = build_default_icon();
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("MOA")
        .with_icon(icon)
        .build()?;

    let ids = TrayIds {
        show: show_item.id().clone(),
        settings: settings_item.id().clone(),
        quit: quit_item.id().clone(),
    };

    // Poll menu events on the GPUI background executor; dispatch the
    // results back onto the main thread via `cx.update`. The loop exits
    // naturally when the app shuts down and `cx.update` fails.
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                let ids = &ids;
                let id = event.id.clone();
                let _ = cx.update(|cx| {
                    if id == ids.show {
                        cx.activate(true);
                    } else if id == ids.settings {
                        cx.activate(true);
                        // Dispatch on the first open window. `dispatch_action`
                        // bubbles through whatever view chain is focused.
                        if let Some(window) = cx.windows().first().copied() {
                            let _ = window.update(cx, |_, window, cx| {
                                use crate::actions::OpenSettings;
                                window.dispatch_action(Box::new(OpenSettings), cx);
                            });
                        }
                    } else if id == ids.quit {
                        cx.quit();
                    }
                });
            }
        }
    })
    .detach();

    Ok(TrayHandle { _tray: tray })
}

/// Creates a minimal 16×16 blue-square icon as a built-in placeholder so we
/// don't have to bundle assets. Users on macOS get a template-styled icon
/// in the menu bar regardless.
fn build_default_icon() -> Icon {
    const SIZE: u32 = 32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let inner = 2..(SIZE - 2);
    for y in 0..SIZE {
        for x in 0..SIZE {
            // A simple "M" glyph on a rounded tile — visible without assets.
            let on_border = !inner.contains(&x) || !inner.contains(&y);
            let on_glyph = matches!(x, 6..=8 | 14..=17 | 23..=25)
                && (6..=25).contains(&y)
                && !(matches!(x, 14..=17) && y < 12);
            if on_border || on_glyph {
                rgba.extend_from_slice(&[255, 255, 255, 255]);
            } else {
                rgba.extend_from_slice(&[20, 90, 220, 255]);
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("synthetic RGBA buffer should always decode")
}
