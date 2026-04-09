//! Session picker overlay and fuzzy-search helpers.

use moa_core::{SessionId, SessionStatus};
use nucleo::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use crate::runner::SessionPreview;

const MAX_VISIBLE_RESULTS: usize = 10;

/// Stateful session-picker overlay input and selection state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SessionPickerState {
    query: String,
    selected: usize,
}

impl SessionPickerState {
    /// Creates an empty session picker state.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns the current fuzzy-search query string.
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    /// Appends a typed character to the fuzzy-search query.
    pub(crate) fn push_char(&mut self, ch: char) {
        self.query.push(ch);
        self.selected = 0;
    }

    /// Removes one character from the end of the fuzzy-search query.
    pub(crate) fn backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
    }

    /// Moves the highlighted result one row upward.
    pub(crate) fn move_up(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }

        self.selected = if self.selected == 0 {
            len.saturating_sub(1)
        } else {
            self.selected.saturating_sub(1)
        };
    }

    /// Moves the highlighted result one row downward.
    pub(crate) fn move_down(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }

        self.selected = (self.selected + 1) % len;
    }

    /// Clamps the selected row to the number of available search results.
    pub(crate) fn clamp_selection(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    /// Returns the selected session identifier for the current filtered result set.
    pub(crate) fn selected_session_id(&self, sessions: &[SessionPreview]) -> Option<SessionId> {
        filtered_sessions(&self.query, sessions)
            .get(self.selected)
            .map(|preview| preview.summary.session_id.clone())
    }
}

/// Renders the session picker overlay centered above the chat view.
pub(crate) fn render_session_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &SessionPickerState,
    sessions: &[SessionPreview],
) {
    let popup = centered_rect(area, 70, 60);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(popup);
    let filtered = filtered_sessions(state.query(), sessions);

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default().borders(Borders::ALL).title("Sessions"),
        popup,
    );

    let query = Paragraph::new(format!("Search: {}", state.query()))
        .block(Block::default().borders(Borders::ALL).title("Filter"));
    frame.render_widget(query, layout[0]);

    let mut rows = Vec::new();
    if filtered.is_empty() {
        rows.push(Line::from("No matching sessions."));
    } else {
        for (index, preview) in filtered.iter().take(MAX_VISIBLE_RESULTS).enumerate() {
            let prefix = if index == state.selected {
                "▶ "
            } else {
                "  "
            };
            let style = if index == state.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let line = format!(
                "{prefix}{}  {}  {}  {}",
                status_icon(&preview.summary.status),
                picker_title(preview),
                preview.summary.workspace_id,
                preview
                    .last_message
                    .clone()
                    .unwrap_or_else(|| "No messages yet.".to_string())
            );
            rows.push(Line::from(Span::styled(line, style)));
        }
    }
    let results =
        Paragraph::new(rows).block(Block::default().borders(Borders::ALL).title("Matches"));
    frame.render_widget(results, layout[1]);

    let footer = Paragraph::new("Enter open · Esc close · ↑/↓ select")
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, layout[2]);
}

/// Returns the fuzzy-filtered session previews for a picker query.
pub(crate) fn filtered_sessions<'a>(
    query: &str,
    sessions: &'a [SessionPreview],
) -> Vec<&'a SessionPreview> {
    if query.trim().is_empty() {
        return sessions.iter().collect();
    }

    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buffer = Vec::new();
    let mut scored = sessions
        .iter()
        .filter_map(|preview| {
            let haystack = picker_haystack(preview);
            let score = pattern.score(Utf32Str::new(&haystack, &mut buffer), &mut matcher)?;
            Some((preview, score))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|(left_preview, left_score), (right_preview, right_score)| {
        right_score.cmp(left_score).then_with(|| {
            right_preview
                .summary
                .updated_at
                .cmp(&left_preview.summary.updated_at)
        })
    });
    scored.into_iter().map(|(preview, _)| preview).collect()
}

fn centered_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn picker_title(preview: &SessionPreview) -> String {
    preview
        .summary
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| {
            preview
                .summary
                .session_id
                .to_string()
                .chars()
                .take(8)
                .collect::<String>()
        })
}

fn picker_haystack(preview: &SessionPreview) -> String {
    format!(
        "{} {} {} {} {}",
        picker_title(preview),
        preview.summary.session_id,
        preview.summary.workspace_id,
        preview.summary.model,
        preview.last_message.clone().unwrap_or_default()
    )
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
    use chrono::{Duration, Utc};
    use moa_core::{Platform, SessionId, SessionSummary, UserId, WorkspaceId};

    use super::{SessionPickerState, filtered_sessions};
    use crate::runner::SessionPreview;

    fn preview(title: &str, last_message: &str, minutes_ago: i64) -> SessionPreview {
        SessionPreview {
            summary: SessionSummary {
                session_id: SessionId::new(),
                workspace_id: WorkspaceId::new("workspace"),
                user_id: UserId::new("user"),
                title: Some(title.to_string()),
                status: moa_core::SessionStatus::Completed,
                platform: Platform::Tui,
                model: "claude-sonnet-4-6".to_string(),
                updated_at: Utc::now() - Duration::minutes(minutes_ago),
            },
            last_message: Some(last_message.to_string()),
        }
    }

    #[test]
    fn fuzzy_search_matches_title_and_last_message() {
        let sessions = vec![
            preview("OAuth bug hunt", "Fix refresh token failure", 5),
            preview("Docs", "Write release notes", 10),
            preview("Infra", "Rotate certificates", 15),
        ];

        let filtered = filtered_sessions("refresh bug", &sessions);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].summary.title.as_deref(), Some("OAuth bug hunt"));
    }

    #[test]
    fn picker_selection_wraps_and_clamps() {
        let sessions = vec![preview("One", "hello", 1), preview("Two", "world", 2)];
        let mut picker = SessionPickerState::new();

        picker.move_up(sessions.len());
        assert_eq!(
            picker.selected_session_id(&sessions),
            Some(sessions[1].summary.session_id.clone())
        );

        picker.move_down(sessions.len());
        assert_eq!(
            picker.selected_session_id(&sessions),
            Some(sessions[0].summary.session_id.clone())
        );

        picker.clamp_selection(0);
        assert_eq!(
            picker.selected_session_id(&sessions),
            Some(sessions[0].summary.session_id.clone())
        );
    }
}
