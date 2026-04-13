//! Theme initialization for the MOA desktop application.

use gpui::App;
use gpui_component::{Theme, ThemeMode};

/// Initializes gpui-component and switches the global theme to dark mode.
///
/// Call once at application startup, before opening the first window.
pub fn setup_theme(cx: &mut App) {
    gpui_component::init(cx);
    Theme::change(ThemeMode::Dark, None, cx);
}
