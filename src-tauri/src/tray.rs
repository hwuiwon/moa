//! System tray setup and status helpers for the desktop shell.

use tauri::{
    App, AppHandle, Manager, Runtime,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

const TRAY_ID: &str = "main-tray";
const MENU_SHOW_ID: &str = "show-window";
const MENU_HIDE_ID: &str = "hide-window";
const MENU_QUIT_ID: &str = "quit";

/// High-level tray status text shown in the tooltip.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TrayStatus {
    /// The desktop app is idle.
    Ready,
    /// The active session is generating.
    Working,
    /// The active session is blocked on approval.
    AwaitingApproval,
    /// The active session hit an error.
    Error,
}

impl TrayStatus {
    fn tooltip(self) -> &'static str {
        match self {
            Self::Ready => "MOA • Ready",
            Self::Working => "MOA • Working",
            Self::AwaitingApproval => "MOA • Waiting for approval",
            Self::Error => "MOA • Last run failed",
        }
    }
}

/// Creates the desktop tray icon and hooks up its menu interactions.
pub(crate) fn setup_system_tray<R: Runtime>(app: &App<R>) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, MENU_SHOW_ID, "Show MOA", true, None::<&str>)?;
    let hide_item = MenuItem::with_id(app, MENU_HIDE_ID, "Hide MOA", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, MENU_QUIT_ID, "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_item, &hide_item, &quit_item])?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip(TrayStatus::Ready.tooltip())
        .on_menu_event(|app, event| match event.id().as_ref() {
            MENU_SHOW_ID => show_main_window(app),
            MENU_HIDE_ID => hide_main_window(app),
            MENU_QUIT_ID => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    let _ = builder.build(app)?;
    Ok(())
}

/// Updates the tray tooltip to reflect the current session status.
pub(crate) fn update_tray_status<R: Runtime>(app: &AppHandle<R>, status: TrayStatus) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_tooltip(Some(status.tooltip()));
    }
}

fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };

    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}

fn hide_main_window<R: Runtime>(app: &AppHandle<R>) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };

    let _ = window.hide();
}
