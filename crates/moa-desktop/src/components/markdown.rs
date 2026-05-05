//! Shared styling for Markdown rendering via `gpui_component::TextView`.
//!
//! Mirrors Vercel Streamdown's spacing/typography conventions adapted to
//! the smaller lever set `TextViewStyle` exposes (paragraph_gap, heading
//! scale, code block, is_dark). Streamdown uses CSS `space-y-4` (16 px)
//! between blocks and a tighter heading scale; we approximate both here.

use gpui::{App, Pixels, StyleRefinement, Styled, px, rems};
use gpui_component::{ActiveTheme, ThemeMode, text::TextViewStyle};

/// Style applied to every markdown block (chat bubbles + memory viewer).
pub fn markdown_style(cx: &App) -> TextViewStyle {
    let theme = cx.theme();

    // `paragraph_gap` is the only block-rhythm lever — it fires between
    // every adjacent block, including consecutive list items. Streamdown
    // uses 16 px between top-level blocks but only 4 px between list
    // items via per-element CSS; we don't have that split, so 0.5 rem
    // (8 px) is the compromise that keeps lists tight without crushing
    // paragraph rhythm.
    let mut style = TextViewStyle::default()
        .paragraph_gap(rems(0.5))
        // Heading scale matches Streamdown's text-3xl → text-sm
        // (30/24/20/18/16/14 px). The larger top-level sizes carry the
        // visual hierarchy that `mt-6 mb-2` would normally provide.
        .heading_font_size(|level: u8, _base: Pixels| match level {
            1 => px(30.0),
            2 => px(24.0),
            3 => px(20.0),
            4 => px(18.0),
            5 => px(16.0),
            _ => px(14.0),
        });

    style.highlight_theme = theme.highlight_theme.clone();
    style.is_dark = matches!(theme.mode, ThemeMode::Dark);

    style.code_block = StyleRefinement::default()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(theme.muted)
        .border_1()
        .border_color(theme.border);

    style
}
