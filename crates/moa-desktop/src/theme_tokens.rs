//! Semantic token layer over `gpui_component::ActiveTheme`.
//!
//! Linear's design system reduces a full palette to **three** variables —
//! base, accent, contrast — and computes everything else perceptually.
//! gpui-component's theme exposes ~30 raw tokens; this layer groups them
//! into the three semantic buckets so panel code can express *intent*
//! rather than reach for a specific token.
//!
//! The mapping is one-to-many but stable, and lives in one place so a
//! future move to LCH-derived scales (or a different design system) only
//! touches this file.

use gpui::{App, Hsla};
use gpui_component::ActiveTheme;

/// Grouped accessor returned by [`tokens`]. Cheap to clone — every field
/// is an `Hsla` value, copied from the theme on each call.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct Tokens {
    /// `base` family — surface colors, foreground text, neutral fills.
    pub base: BaseTokens,
    /// `accent` family — the single brand color and its hover/active.
    pub accent: AccentTokens,
    /// `contrast` family — semantic notification colors (success, etc.)
    /// plus contrast-driven overlays.
    pub contrast: ContrastTokens,
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct BaseTokens {
    /// Window background, deepest surface.
    pub background: Hsla,
    /// Elevated surface (sidebar, statusbar, cards, titlebar).
    pub surface: Hsla,
    /// Subtle fill (inline chips, segmented control track).
    pub subtle: Hsla,
    /// Body foreground text.
    pub foreground: Hsla,
    /// Secondary / muted text.
    pub muted: Hsla,
    /// Borders / dividers.
    pub border: Hsla,
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct AccentTokens {
    /// Primary brand color (toggle-on, primary buttons, focus rings).
    pub primary: Hsla,
    /// Hover state for primary surfaces.
    pub primary_hover: Hsla,
    /// Foreground placed on a primary background.
    pub primary_foreground: Hsla,
    /// Soft-accent surface (left-nav active, selected list rows).
    pub accent: Hsla,
    /// Foreground placed on an accent background.
    pub accent_foreground: Hsla,
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct ContrastTokens {
    pub success: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
    pub info: Hsla,
}

/// Returns the semantic [`Tokens`] for the current theme.
///
/// Panel code prefers `tokens(cx).accent.primary` over
/// `cx.theme().primary` — the indirection makes a future palette swap
/// (LCH-derived scales, multi-brand theming) a one-file change.
#[allow(dead_code)]
pub fn tokens(cx: &App) -> Tokens {
    let theme = cx.theme();
    Tokens {
        base: BaseTokens {
            background: theme.background,
            surface: theme.sidebar,
            subtle: theme.muted,
            foreground: theme.foreground,
            muted: theme.muted_foreground,
            border: theme.border,
        },
        accent: AccentTokens {
            primary: theme.primary,
            primary_hover: theme.primary_hover,
            primary_foreground: theme.primary_foreground,
            accent: theme.accent,
            accent_foreground: theme.accent_foreground,
        },
        contrast: ContrastTokens {
            success: theme.success,
            warning: theme.warning,
            danger: theme.danger,
            info: theme.info,
        },
    }
}
