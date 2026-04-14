//! Row component rendering one session in the sidebar list.

use gpui::{App, ElementId, RenderOnce, SharedString, div, prelude::*, px, rgb};
use gpui_component::ActiveTheme;
use moa_core::{SessionId, SessionStatus};

use super::time::relative;

/// Visual props for a session list row. Kept as `RenderOnce` for cheap virtualization.
#[derive(IntoElement)]
pub struct SessionRow {
    pub id: SessionId,
    pub title: SharedString,
    pub status: SessionStatus,
    pub model: SharedString,
    pub last_message: Option<SharedString>,
    pub updated: chrono::DateTime<chrono::Utc>,
    pub selected: bool,
}

impl SessionRow {
    fn status_color(status: &SessionStatus) -> u32 {
        match status {
            SessionStatus::Running => 0x3b82f6,
            SessionStatus::Completed => 0x10b981,
            SessionStatus::Failed => 0xef4444,
            SessionStatus::WaitingApproval => 0xeab308,
            SessionStatus::Paused => 0x9ca3af,
            SessionStatus::Cancelled => 0x6b7280,
            SessionStatus::Created => 0x8b5cf6,
        }
    }

    fn status_label(status: &SessionStatus) -> &'static str {
        match status {
            SessionStatus::Running => "running",
            SessionStatus::Completed => "done",
            SessionStatus::Failed => "failed",
            SessionStatus::WaitingApproval => "needs approval",
            SessionStatus::Paused => "paused",
            SessionStatus::Cancelled => "cancelled",
            SessionStatus::Created => "new",
        }
    }
}

impl RenderOnce for SessionRow {
    fn render(self, _window: &mut gpui::Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let dot = rgb(Self::status_color(&self.status));
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
            .border_color(if self.selected {
                rgb(Self::status_color(&self.status)).into()
            } else {
                theme.transparent
            })
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
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().size(px(8.)).rounded_full().bg(dot))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted_fg)
                            .child(Self::status_label(&self.status)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .px_1p5()
                            .rounded_sm()
                            .bg(theme.muted)
                            .text_color(theme.muted_foreground)
                            .child(self.model.clone()),
                    ),
            )
            .when_some(self.last_message, |row, msg| {
                row.child(
                    div()
                        .text_xs()
                        .text_color(muted_fg)
                        .overflow_hidden()
                        .child(msg),
                )
            })
    }
}
