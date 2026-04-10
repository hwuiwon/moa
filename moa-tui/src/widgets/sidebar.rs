//! Optional sidebar renderer for session details, tools, and recent memory.

use moa_core::{PageSummary, SessionMeta};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

/// Minimum terminal width that auto-enables the sidebar.
pub(crate) const SIDEBAR_AUTO_WIDTH: u16 = 120;

/// Returns whether the sidebar should be visible for the current viewport.
pub(crate) fn should_show_sidebar(
    viewport_width: u16,
    sidebar_auto: bool,
    sidebar_override: Option<bool>,
) -> bool {
    sidebar_override.unwrap_or(sidebar_auto && viewport_width >= SIDEBAR_AUTO_WIDTH)
}

/// Renders the right-hand sidebar.
pub(crate) fn render_sidebar(
    frame: &mut Frame<'_>,
    area: Rect,
    session: Option<&SessionMeta>,
    tool_names: &[String],
    recent_memory: &[PageSummary],
) {
    let mut lines = Vec::new();

    lines.push(section_title("Session Info"));
    if let Some(session) = session {
        let duration = chrono::Utc::now() - session.created_at;
        lines.push(Line::from(format!(
            "Duration: {}m {:02}s",
            duration.num_minutes(),
            duration.num_seconds().rem_euclid(60)
        )));
        lines.push(Line::from(format!("Status: {:?}", session.status)));
        lines.push(Line::from(format!("Turns: {}", session.event_count / 2)));
        lines.push(Line::from(format!(
            "Tokens: {}",
            session.total_input_tokens + session.total_output_tokens
        )));
        lines.push(Line::from(format!(
            "Cost: ${:.2}",
            session.total_cost_cents as f32 / 100.0
        )));
    } else {
        lines.push(Line::from("No active session."));
    }

    lines.push(Line::raw(String::new()));
    lines.push(section_title("Workspace Tools"));
    if tool_names.is_empty() {
        lines.push(Line::from("No registered tools."));
    } else {
        for name in tool_names.iter().take(8) {
            lines.push(Line::from(format!("✓ {name}")));
        }
    }

    lines.push(Line::raw(String::new()));
    lines.push(section_title("Recent Memory"));
    if recent_memory.is_empty() {
        lines.push(Line::from("No memory pages yet."));
    } else {
        for page in recent_memory.iter().take(8) {
            lines.push(Line::from(format!("• {}", page.title)));
        }
    }

    let paragraph =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("Sidebar"));
    frame.render_widget(paragraph, area);
}

fn section_title(title: &str) -> Line<'static> {
    Line::from(vec![Span::styled(
        title.to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    )])
}

#[cfg(test)]
mod tests {
    use super::should_show_sidebar;

    #[test]
    fn sidebar_visibility_respects_width_and_override() {
        assert!(!should_show_sidebar(100, true, None));
        assert!(should_show_sidebar(120, true, None));
        assert!(should_show_sidebar(90, false, Some(true)));
        assert!(!should_show_sidebar(140, true, Some(false)));
    }
}
