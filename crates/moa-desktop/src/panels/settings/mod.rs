//! Full-page Settings view with tabs for General, Appearance, Providers,
//! Permissions, and Keyboard Shortcuts.

pub mod appearance_tab;
pub mod general_tab;
pub mod keyboard_shortcuts_tab;
pub mod permissions_tab;
pub mod providers_tab;
pub mod settings_panel;

pub use settings_panel::{SettingsDismissed, SettingsPage};
