//! Row component rendering one session in the sidebar list.

use gpui::{App, ElementId, RenderOnce, SharedString, div, prelude::*, px};
use gpui_component::ActiveTheme;
use moa_core::{SessionId, SessionStatus};

use crate::components::badges::{status_color, status_label};

use super::time::relative;

/// Visual props for a session list row. Kept as `RenderOnce` for cheap virtualization.
#[derive(IntoElement)]
pub struct SessionRow {
    pub id: SessionId,
    pub title: SharedString,
    pub status: SessionStatus,
    /// Retained on the struct for future use (e.g. filter-by-model) even
    /// though the row no longer renders a model chip — the model is shown
    /// on each assistant bubble instead.
    #[allow(dead_code)]
    pub model: SharedString,
    pub last_message: Option<SharedString>,
    pub updated: chrono::DateTime<chrono::Utc>,
    pub selected: bool,
}

/// Collapses newlines and truncates a preview string to a single line.
fn truncate_single_line(msg: SharedString, limit: usize) -> SharedString {
    let cleaned: String = msg
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.chars().count() <= limit {
        return SharedString::from(trimmed.to_string());
    }
    let short: String = trimmed.chars().take(limit).collect();
    SharedString::from(format!("{short}…"))
}

impl RenderOnce for SessionRow {
    fn render(self, _window: &mut gpui::Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme().clone();
        let dot = status_color(cx, &self.status);
        let row_bg = if self.selected {
            theme.accent
        } else {
            theme.sidebar
        };
        let fg = if self.selected {
            theme.accent_foreground
        } else {
            theme.sidebar_foreground
        };
        let muted_fg = if self.selected {
            theme.accent_foreground
        } else {
            theme.muted_foreground
        };

        div()
            .id(ElementId::NamedInteger(
                "session-row".into(),
                self.id.0.as_u128() as u64,
            ))
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .bg(row_bg)
            .border_l_2()
            .border_color(if self.selected { dot } else { theme.transparent })
            .hover(|s| s.bg(theme.muted))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(fg)
                            .overflow_hidden()
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(relative(self.updated)),
                    ),
            )
            .child(
                // Status dot + label. The short session-id chip now
                // lives in the right-hand detail panel when a session
                // is selected — keep the sidebar row visually clean.
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().size(px(6.0)).rounded_full().bg(dot))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(status_label(&self.status)),
                    ),
            )
            .when_some(self.last_message, |row, msg| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(muted_fg)
                        .overflow_hidden()
                        .max_h(px(18.0))
                        .whitespace_nowrap()
                        .child(truncate_single_line(msg, 80)),
                )
            })
    }
}
