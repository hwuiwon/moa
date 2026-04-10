//! Session toolbar rendering for the multi-session TUI.

use moa_core::{SessionId, SessionStatus};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use crate::runner::SessionPreview;

const MAX_VISIBLE_TABS: usize = 8;

/// Header metrics rendered alongside the session tabs.
pub(crate) struct ToolbarMetrics<'a> {
    /// Current workspace label.
    pub workspace_name: &'a str,
    /// Active model identifier.
    pub model: &'a str,
    /// Aggregate token count for the current session.
    pub total_tokens: usize,
    /// Aggregate session cost in cents.
    pub total_cost_cents: u32,
}

/// Renders the top toolbar with session tabs, model information, and token totals.
pub(crate) fn render_toolbar(
    frame: &mut Frame<'_>,
    area: Rect,
    sessions: &[SessionPreview],
    active_session_id: &SessionId,
    metrics: ToolbarMetrics<'_>,
) {
    let title = format!(
        "MOA · workspace: {workspace_name} · {model} · {total_tokens} tok · ${:.2}",
        metrics.total_cost_cents as f32 / 100.0,
        workspace_name = metrics.workspace_name,
        model = metrics.model,
        total_tokens = metrics.total_tokens,
    );
    let tab_spans = build_tab_spans(sessions, active_session_id, MAX_VISIBLE_TABS);
    let lines = if tab_spans.is_empty() {
        vec![
            Line::from(vec![Span::styled(
                "No sessions",
                Style::default().add_modifier(Modifier::BOLD),
            )]),
            Line::from("Ctrl+N starts a new session."),
        ]
    } else {
        vec![
            Line::from(tab_spans),
            Line::from(
                "Alt+1-9 switch · Alt+[ / Alt+] cycle · Ctrl+N new · Ctrl+P palette · Ctrl+O, S sessions",
            ),
        ]
    };

    let paragraph =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(paragraph, area);
}

/// Returns the toolbar tab spans for the visible session window.
pub(crate) fn build_tab_spans(
    sessions: &[SessionPreview],
    active_session_id: &SessionId,
    max_visible: usize,
) -> Vec<Span<'static>> {
    let Some(active_index) = sessions
        .iter()
        .position(|preview| preview.summary.session_id == *active_session_id)
    else {
        return Vec::new();
    };
    let (start, end) = visible_window(sessions.len(), active_index, max_visible);
    let mut spans = Vec::new();

    if start > 0 {
        spans.push(Span::raw("… "));
    }

    for (visible_idx, preview) in sessions[start..end].iter().enumerate() {
        let absolute_idx = start + visible_idx;
        let title = tab_title(preview, absolute_idx);
        let style = if preview.summary.session_id == *active_session_id {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
        };
        spans.push(Span::styled(format!(" {title} "), style));
        spans.push(Span::raw(" "));
    }

    if end < sessions.len() {
        spans.push(Span::raw("…"));
    }

    spans
}

fn visible_window(total: usize, active_index: usize, max_visible: usize) -> (usize, usize) {
    if total <= max_visible {
        return (0, total);
    }

    let half = max_visible / 2;
    let mut start = active_index.saturating_sub(half);
    let mut end = (start + max_visible).min(total);
    start = end.saturating_sub(max_visible);
    end = (start + max_visible).min(total);
    (start, end)
}

fn tab_title(preview: &SessionPreview, index: usize) -> String {
    let shortcut = if index < 9 {
        format!("{}:", index + 1)
    } else {
        String::new()
    };
    let title = preview
        .summary
        .title
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| short_session_id(&preview.summary.session_id));
    format!(
        "{shortcut}{} {}",
        status_icon(&preview.summary.status),
        truncate_title(&title, 18)
    )
}

fn short_session_id(session_id: &SessionId) -> String {
    session_id.to_string().chars().take(8).collect()
}

fn truncate_title(title: &str, max_chars: usize) -> String {
    let mut truncated = title.chars().take(max_chars).collect::<String>();
    if title.chars().count() > max_chars {
        truncated.push('…');
    }
    truncated
}

fn status_icon(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Running => "🔄",
        SessionStatus::WaitingApproval | SessionStatus::Paused => "⏸",
        SessionStatus::Completed => "✅",
        SessionStatus::Failed | SessionStatus::Cancelled => "❌",
        SessionStatus::Created => "●",
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{Platform, SessionId, SessionStatus, SessionSummary, UserId, WorkspaceId};

    use super::{build_tab_spans, visible_window};
    use crate::runner::SessionPreview;

    fn preview(index: usize, status: SessionStatus) -> SessionPreview {
        SessionPreview {
            summary: SessionSummary {
                session_id: SessionId::new(),
                workspace_id: WorkspaceId::new("ws"),
                user_id: UserId::new("u"),
                title: Some(format!("Session {index}")),
                status,
                platform: Platform::Tui,
                model: "test".to_string(),
                updated_at: Utc::now(),
            },
            last_message: Some("hello".to_string()),
        }
    }

    #[test]
    fn visible_window_caps_tabs_to_requested_limit() {
        assert_eq!(visible_window(3, 1, 8), (0, 3));
        assert_eq!(visible_window(10, 0, 8), (0, 8));
        assert_eq!(visible_window(10, 9, 8), (2, 10));
    }

    #[test]
    fn toolbar_labels_include_status_icons_and_tab_limit() {
        let sessions = vec![
            preview(1, SessionStatus::Running),
            preview(2, SessionStatus::WaitingApproval),
            preview(3, SessionStatus::Completed),
            preview(4, SessionStatus::Failed),
            preview(5, SessionStatus::Created),
            preview(6, SessionStatus::Running),
            preview(7, SessionStatus::Running),
            preview(8, SessionStatus::Running),
            preview(9, SessionStatus::Running),
        ];
        let spans = build_tab_spans(&sessions, &sessions[2].summary.session_id, 8);
        let rendered = spans
            .iter()
            .map(|span| span.content.to_string())
            .collect::<String>();

        assert!(rendered.contains("🔄"));
        assert!(rendered.contains("⏸"));
        assert!(rendered.contains("✅"));
        assert!(rendered.contains("❌"));
        assert!(rendered.contains("…"));
    }
}
