//! Window size, position, and panel-visibility state persisted across runs.
//!
//! Stored as JSON at `~/.moa/window_state.json`. Values are saved
//! best-effort — IO errors are logged but never surfaced to the user, since
//! the app can always fall back to the compiled defaults on next launch.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Persisted window/layout state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowState {
    pub width: f32,
    pub height: f32,
    pub x: f32,
    pub y: f32,
    pub sidebar_width: f32,
    pub detail_width: f32,
    pub sidebar_visible: bool,
    pub detail_visible: bool,
    /// `true` once the user has closed the window for the first time (and
    /// been shown a toast explaining that MOA continues running in the
    /// tray). Stored here so the notice only appears once per install.
    #[serde(default)]
    pub close_to_tray_shown: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            width: 1400.0,
            height: 900.0,
            x: 100.0,
            y: 100.0,
            sidebar_width: 250.0,
            detail_width: 320.0,
            sidebar_visible: true,
            detail_visible: true,
            close_to_tray_shown: false,
        }
    }
}

impl WindowState {
    /// Path to `~/.moa/window_state.json`. Returns `None` if `$HOME` isn't
    /// set (extremely rare — we fall back to no persistence in that case).
    pub fn default_path() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".moa").join("window_state.json"))
    }

    /// Loads state from the default path, returning defaults on any error.
    pub fn load_or_default() -> Self {
        Self::default_path()
            .and_then(|p| Self::load(&p).ok())
            .unwrap_or_default()
    }

    /// Loads state from an explicit file path.
    pub fn load(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let content = std::fs::read_to_string(path.as_ref())?;
        serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Persists to the default path. IO failures are logged and swallowed.
    pub fn save_to_default_path(&self) {
        let Some(path) = Self::default_path() else {
            return;
        };
        if let Err(err) = self.save(&path) {
            tracing::warn!(%err, "failed to persist window state");
        }
    }

    /// Persists to an explicit file path.
    pub fn save(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Clamps position/size into sensible bounds so a stored off-screen
    /// position (e.g. from a disconnected monitor) doesn't hide the window.
    /// Width/height are clamped to a minimum so the window stays usable.
    pub fn validate_bounds(&mut self, screen_width: f32, screen_height: f32) {
        self.width = self.width.clamp(600.0, screen_width);
        self.height = self.height.clamp(400.0, screen_height);
        // Keep at least 100 px of the window on-screen in each axis.
        let max_x = (screen_width - 100.0).max(0.0);
        let max_y = (screen_height - 100.0).max(0.0);
        self.x = self.x.clamp(0.0, max_x);
        self.y = self.y.clamp(0.0, max_y);
        self.sidebar_width = self.sidebar_width.clamp(180.0, 600.0);
        self.detail_width = self.detail_width.clamp(220.0, 700.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_disk() {
        let tmp = std::env::temp_dir().join("moa-window-state-test.json");
        let _ = std::fs::remove_file(&tmp);

        let state = WindowState {
            width: 1400.0,
            height: 900.0,
            x: 120.0,
            y: 60.0,
            sidebar_width: 260.0,
            detail_width: 300.0,
            sidebar_visible: false,
            detail_visible: true,
            close_to_tray_shown: false,
        };
        state.save(&tmp).expect("save");
        let loaded = WindowState::load(&tmp).expect("load");

        assert_eq!(loaded.width, 1400.0);
        assert_eq!(loaded.sidebar_width, 260.0);
        assert!(!loaded.sidebar_visible);
        assert!(loaded.detail_visible);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn validate_bounds_clamps_offscreen_positions() {
        let mut state = WindowState {
            x: -5000.0,
            y: -5000.0,
            width: 10_000.0,
            height: 10_000.0,
            ..Default::default()
        };
        state.validate_bounds(1920.0, 1080.0);
        assert!(state.x >= 0.0);
        assert!(state.y >= 0.0);
        assert!(state.width <= 1920.0);
        assert!(state.height <= 1080.0);
    }

    #[test]
    fn validate_bounds_clamps_tiny_panels() {
        let mut state = WindowState {
            sidebar_width: 10.0,
            detail_width: 10_000.0,
            ..Default::default()
        };
        state.validate_bounds(1920.0, 1080.0);
        assert!(state.sidebar_width >= 180.0);
        assert!(state.detail_width <= 700.0);
    }

    #[test]
    fn missing_file_falls_back_to_default() {
        let missing = std::env::temp_dir().join("moa-window-state-missing.json");
        let _ = std::fs::remove_file(&missing);
        assert!(WindowState::load(&missing).is_err());
    }
}
