//! UI density mode.
//!
//! The user picks between "Comfortable" (default) and "Compact" in the
//! Appearance tab. That preference is read from `cx.theme()` indirectly
//! via the [`MoaConfig.tui.density`] string and mapped to a concrete
//! [`Spacing`] struct that panels consume.

use gpui::{App, Pixels, Rems, px, rems};

/// Discrete density levels. Stored as a string in `MoaConfig.tui.density`
/// for forward compatibility (e.g. adding a "Dense" mode later) but only
/// the two listed variants are recognized today; unknown values fall
/// back to `Comfortable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Density {
    Comfortable,
    Compact,
}

impl Density {
    pub fn from_str(raw: &str) -> Self {
        match raw.to_ascii_lowercase().as_str() {
            "compact" => Self::Compact,
            _ => Self::Comfortable,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Comfortable => "comfortable",
            Self::Compact => "compact",
        }
    }

    /// Resolved spacing values for this density.
    pub fn spacing(self) -> Spacing {
        match self {
            Self::Comfortable => Spacing {
                bubble_padding: px(12.0),
                row_padding_y: px(8.0),
                list_gap: px(12.0),
                markdown_line_height: rems(1.55),
            },
            Self::Compact => Spacing {
                bubble_padding: px(8.0),
                row_padding_y: px(4.0),
                list_gap: px(8.0),
                markdown_line_height: rems(1.4),
            },
        }
    }
}

/// Resolves the active density by reading `ServiceBridgeHandle`'s loaded
/// config. Falls back to `Comfortable` when the bridge global isn't set
/// (e.g. early-startup tests). This is the canonical accessor — panel
/// code calls `density::current(cx)` rather than threading config refs.
pub fn current(cx: &App) -> Density {
    let handle = cx.try_global::<crate::services::ServiceBridgeHandle>();
    let Some(handle) = handle else {
        return Density::Comfortable;
    };
    let bridge = handle.entity().read(cx);
    bridge
        .config()
        .map(|cfg| Density::from_str(&cfg.tui.density))
        .unwrap_or(Density::Comfortable)
}

/// Concrete spacing values consumed by panels. Extracted as a struct so
/// the call-site doesn't need to match on the enum every time.
///
/// Some fields aren't yet wired into every panel — `markdown_line_height`
/// is consumed today; the rest are defined ahead of the panels that will
/// adopt them. The `dead_code` allow keeps the API stable while the
/// wiring lands incrementally.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct Spacing {
    pub bubble_padding: Pixels,
    pub row_padding_y: Pixels,
    pub list_gap: Pixels,
    pub markdown_line_height: Rems,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_density_falls_back_to_comfortable() {
        assert_eq!(Density::from_str(""), Density::Comfortable);
        assert_eq!(Density::from_str("foo"), Density::Comfortable);
        assert_eq!(Density::from_str("comfortable"), Density::Comfortable);
        assert_eq!(Density::from_str("Comfortable"), Density::Comfortable);
    }

    #[test]
    fn compact_is_tighter_than_comfortable() {
        let comf = Density::Comfortable.spacing();
        let comp = Density::Compact.spacing();
        assert!(comp.bubble_padding < comf.bubble_padding);
        assert!(comp.row_padding_y < comf.row_padding_y);
        assert!(comp.markdown_line_height.0 < comf.markdown_line_height.0);
    }
}
