//! Grouped-card container used by Settings and any panel that needs a
//! titled block of rows. See `design.md` → "Section card".
//!
//! Usage:
//! ```ignore
//! section_card(cx, Some("Update"), Some("Core app updates"))
//!     .child(settings_row(...))
//!     .child(settings_row(...))
//! ```
//! Callers can also omit the title/description and just get the styled
//! container by passing `None`.

use gpui::{App, ParentElement, SharedString, Styled, div};
use gpui_component::ActiveTheme;

pub fn section_card(
    cx: &App,
    title: Option<impl Into<SharedString>>,
    description: Option<impl Into<SharedString>>,
) -> gpui::Div {
    let theme = cx.theme();
    let mut card = div()
        .flex()
        .flex_col()
        .w_full()
        .p_4()
        .bg(theme.sidebar)
        .border_1()
        .border_color(theme.border)
        .rounded_md();

    let has_header = title.is_some() || description.is_some();
    if has_header {
        let mut header = div().flex().flex_col().gap_0p5().mb_3();
        if let Some(t) = title {
            header = header.child(
                div()
                    .text_size(gpui::rems(0.9))
                    .text_color(theme.foreground)
                    .child(t.into()),
            );
        }
        if let Some(d) = description {
            header = header.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(d.into()),
            );
        }
        card = card.child(header);
    }
    card
}
