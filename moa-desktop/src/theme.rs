//! Theme initialization and runtime switching for the MOA desktop app.
//!
//! The initial mode is applied at startup from `setup_theme`; the settings
//! panel calls [`apply_theme_name`] when the user changes theme so the
//! change takes effect immediately without restarting.

use gpui::App;
use gpui_component::{Theme, ThemeMode};

/// Initializes gpui-component and switches the global theme to dark mode.
///
/// Call once at application startup, before opening the first window.
pub fn setup_theme(cx: &mut App) {
    gpui_component::init(cx);
    Theme::change(ThemeMode::Dark, None, cx);
}

/// Applies a theme selection by name. Accepts the values the settings panel
/// uses ("light", "dark", "system"/"auto"/"default" — falling back to dark
/// for unrecognized input so the UI always renders).
pub fn apply_theme_name(name: &str, cx: &mut App) {
    let mode = match name.to_ascii_lowercase().as_str() {
        "light" => ThemeMode::Light,
        "dark" => ThemeMode::Dark,
        // gpui-component ≥0.5 exposes only Light/Dark at the moment;
        // "system"/"auto" map to Dark until platform detection lands.
        _ => ThemeMode::Dark,
    };
    Theme::change(mode, None, cx);
}

/// Returns the canonical UI key (`"dark"` or `"light"`) for any stored
/// theme string. Used by the settings panel so the active-state check
/// matches even when the config holds historical values like `"default"`.
pub fn canonical_theme_key(raw: &str) -> &'static str {
    match raw.to_ascii_lowercase().as_str() {
        "light" => "light",
        _ => "dark",
    }
}

#[cfg(test)]
mod tests {
    use super::canonical_theme_key;

    #[test]
    fn maps_light_and_dark_directly() {
        assert_eq!(canonical_theme_key("light"), "light");
        assert_eq!(canonical_theme_key("dark"), "dark");
        assert_eq!(canonical_theme_key("LIGHT"), "light");
    }

    #[test]
    fn unknown_or_legacy_values_fall_back_to_dark() {
        assert_eq!(canonical_theme_key("default"), "dark");
        assert_eq!(canonical_theme_key("system"), "dark");
        assert_eq!(canonical_theme_key(""), "dark");
        assert_eq!(canonical_theme_key("sepia"), "dark");
    }
}
