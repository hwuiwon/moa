//! Shared styling for Markdown rendering via `gpui_component::TextView`.
//!
//! The library's default `TextViewStyle` is tuned for documentation-site
//! output: 1 rem paragraph gap (too airy inside dense chat bubbles),
//! 14 px headings that barely differ from body text, and light-mode
//! syntax highlighting that washes out on our dark UI. This helper
//! returns a style that matches the rest of the app.

use gpui::{App, Pixels, StyleRefinement, Styled, px, rems};
use gpui_component::{ActiveTheme, text::TextViewStyle};

/// Style applied to every markdown block (chat bubbles + memory viewer).
pub fn markdown_style(cx: &App) -> TextViewStyle {
    let theme = cx.theme();

    // Spacing + sizing targets based on web-typography conventions that
    // carry over cleanly to desktop apps:
    //   * paragraph gap ≈ 0.75 rem gives prose room to breathe without
    //     feeling editorial (typographical sweet spot: 0.6–1.0 rem).
    //   * heading scale follows a ~1.2 modular scale (22/18/16/15) so
    //     levels are distinguishable at a glance.
    //   * body line-height is applied on the wrapping container (see
    //     `message_bubble.rs` and `memory_viewer.rs`) — it can't be set
    //     here because `TextViewStyle` doesn't expose it.
    let mut style = TextViewStyle::default()
        .paragraph_gap(rems(0.75))
        .heading_font_size(|level: u8, _base: Pixels| match level {
            1 => px(22.0),
            2 => px(18.0),
            3 => px(16.0),
            4 => px(15.0),
            _ => px(14.0),
        });

    // Use the active theme's highlight colors for code blocks so syntax
    // tokens actually contrast on dark backgrounds.
    style.highlight_theme = theme.highlight_theme.clone();

    // Code-block container: subtle panel background, padding, rounded
    // corners, faint border. The inner mono font size comes from the
    // theme's `mono_font_size`, which we leave untouched.
    style.code_block = StyleRefinement::default()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(theme.muted)
        .border_1()
        .border_color(theme.border);

    style
}
