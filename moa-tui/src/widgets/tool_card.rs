//! Inline bordered tool card rendering helpers.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

use crate::app::ToolCardEntry;
use moa_core::ToolCardStatus;

/// Renders a tool card into boxed transcript lines.
pub(crate) fn render_tool_card(card: &ToolCardEntry, width: u16) -> Vec<Line<'static>> {
    let inner_width = width.saturating_sub(2).max(24) as usize;
    let status_label = status_label(card.status);
    let title = format!(
        "{} {} · {}",
        status_icon(card.status),
        card.tool_name,
        status_label
    );
    let mut lines = Vec::new();
    lines.push(border_line(
        '┌',
        '┐',
        &title,
        inner_width,
        status_style(card.status),
    ));

    for content in wrap_text(&card.summary, inner_width.saturating_sub(2)) {
        lines.push(content_line(&content, inner_width));
    }

    if let Some(detail) = &card.detail {
        lines.push(content_line("", inner_width));
        for detail_line in detail.lines() {
            for wrapped in wrap_text(detail_line, inner_width.saturating_sub(2)) {
                lines.push(content_line(&wrapped, inner_width));
            }
        }
    }

    lines.push(Line::raw(format!("└{}┘", "─".repeat(inner_width))));
    lines
}

fn border_line(
    left: char,
    right: char,
    title: &str,
    inner_width: usize,
    style: Style,
) -> Line<'static> {
    let truncated = truncate_to_width(title, inner_width.saturating_sub(2));
    let used = truncated.chars().count() + 2;
    let pad = inner_width.saturating_sub(used);
    Line::from(vec![
        Span::raw(left.to_string()),
        Span::styled(format!(" {truncated} "), style),
        Span::raw("─".repeat(pad)),
        Span::raw(right.to_string()),
    ])
}

fn content_line(text: &str, inner_width: usize) -> Line<'static> {
    let truncated = truncate_to_width(text, inner_width);
    let pad = inner_width.saturating_sub(truncated.chars().count());
    Line::raw(format!("│{truncated}{}│", " ".repeat(pad)))
}

fn status_icon(status: ToolCardStatus) -> &'static str {
    match status {
        ToolCardStatus::Pending => "…",
        ToolCardStatus::WaitingApproval => "🟡",
        ToolCardStatus::Running => "🔧",
        ToolCardStatus::Succeeded => "✅",
        ToolCardStatus::Failed => "❌",
    }
}

fn status_label(status: ToolCardStatus) -> &'static str {
    match status {
        ToolCardStatus::Pending => "Pending",
        ToolCardStatus::WaitingApproval => "Approval",
        ToolCardStatus::Running => "Running",
        ToolCardStatus::Succeeded => "Done",
        ToolCardStatus::Failed => "Failed",
    }
}

fn status_style(status: ToolCardStatus) -> Style {
    match status {
        ToolCardStatus::Pending => Style::default().fg(Color::Gray),
        ToolCardStatus::WaitingApproval => Style::default().fg(Color::Yellow),
        ToolCardStatus::Running => Style::default().fg(Color::Cyan),
        ToolCardStatus::Succeeded => Style::default().fg(Color::Green),
        ToolCardStatus::Failed => Style::default().fg(Color::Red),
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
            continue;
        }

        let next_len = current.chars().count() + 1 + word.chars().count();
        if next_len > width {
            lines.push(current);
            current = word.to_string();
        } else {
            current.push(' ');
            current.push_str(word);
        }
    }

    if current.is_empty() {
        lines.push(String::new());
    } else {
        lines.push(current);
    }

    lines
}

fn truncate_to_width(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}
