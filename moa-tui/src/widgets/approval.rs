//! Inline approval widget rendering with risk coloring and compact diff previews.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use moa_core::RiskLevel;

use crate::{
    app::{ApprovalCardStatus, ApprovalEntry},
    views::diff::render_compact_diff_preview,
};

const MAX_PREVIEW_LINES: usize = 6;

/// Renders a bordered approval card into transcript lines.
pub(crate) fn render_approval_card(entry: &ApprovalEntry, width: u16) -> Vec<Line<'static>> {
    let inner_width = width.saturating_sub(2).max(36) as usize;
    let border_style = risk_border_style(&entry.prompt.request.risk_level);
    let mut lines = Vec::new();
    let title = format!(
        "{} {} · {}",
        approval_icon(entry.status),
        entry.prompt.request.tool_name,
        approval_label(entry.status)
    );
    lines.push(border_line('┌', '┐', &title, inner_width, border_style));

    for field in &entry.prompt.parameters {
        let value = format!("{}: {}", field.label, field.value);
        for wrapped in wrap_text(&value, inner_width.saturating_sub(2)) {
            lines.push(content_line(vec![Span::raw(wrapped)], inner_width));
        }
    }

    if let Some(note) = &entry.note {
        lines.push(content_line(vec![Span::raw(String::new())], inner_width));
        for wrapped in wrap_text(note, inner_width.saturating_sub(2)) {
            lines.push(content_line(
                vec![Span::styled(wrapped, status_style(entry.status))],
                inner_width,
            ));
        }
    }

    if let Some(diff) = entry.prompt.file_diffs.first() {
        lines.push(content_line(vec![Span::raw(String::new())], inner_width));
        lines.push(content_line(
            vec![Span::styled(
                format!("Diff: {}", diff.path),
                Style::default().add_modifier(Modifier::BOLD),
            )],
            inner_width,
        ));
        for preview in render_compact_diff_preview(diff, inner_width as u16, MAX_PREVIEW_LINES) {
            lines.push(content_line(preview.spans, inner_width));
        }
    }

    if entry.status == ApprovalCardStatus::Pending {
        lines.push(content_line(vec![Span::raw(String::new())], inner_width));
        lines.push(content_line(
            vec![Span::styled(
                "[Y]es  [N]o  [A]lways  [D]iff  [E]dit",
                Style::default().fg(Color::Gray),
            )],
            inner_width,
        ));
    }

    lines.push(Line::from(vec![
        Span::styled("└", border_style),
        Span::styled("─".repeat(inner_width), border_style),
        Span::styled("┘", border_style),
    ]));
    lines
}

/// Returns the border style for the approval request risk level.
pub(crate) fn risk_border_style(risk_level: &RiskLevel) -> Style {
    match risk_level {
        RiskLevel::Low => Style::default().fg(Color::Green),
        RiskLevel::Medium => Style::default().fg(Color::Yellow),
        RiskLevel::High => Style::default().fg(Color::Red),
    }
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
        Span::styled(left.to_string(), style),
        Span::styled(format!(" {truncated} "), style.add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(pad), style),
        Span::styled(right.to_string(), style),
    ])
}

fn content_line(spans: Vec<Span<'static>>, inner_width: usize) -> Line<'static> {
    let used = spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum::<usize>();
    let pad = inner_width.saturating_sub(used);
    let mut rendered = Vec::with_capacity(spans.len() + 3);
    rendered.push(Span::raw("│"));
    rendered.extend(spans);
    rendered.push(Span::raw(" ".repeat(pad)));
    rendered.push(Span::raw("│"));
    Line::from(rendered)
}

fn approval_icon(status: ApprovalCardStatus) -> &'static str {
    match status {
        ApprovalCardStatus::Pending => "🟡",
        ApprovalCardStatus::AllowedOnce | ApprovalCardStatus::AllowedAlways => "✅",
        ApprovalCardStatus::Denied => "❌",
    }
}

fn approval_label(status: ApprovalCardStatus) -> &'static str {
    match status {
        ApprovalCardStatus::Pending => "Approval",
        ApprovalCardStatus::AllowedOnce => "Allowed",
        ApprovalCardStatus::AllowedAlways => "Always Allow",
        ApprovalCardStatus::Denied => "Denied",
    }
}

fn status_style(status: ApprovalCardStatus) -> Style {
    match status {
        ApprovalCardStatus::Pending => Style::default().fg(Color::Yellow),
        ApprovalCardStatus::AllowedOnce | ApprovalCardStatus::AllowedAlways => {
            Style::default().fg(Color::Green)
        }
        ApprovalCardStatus::Denied => Style::default().fg(Color::Red),
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

        if current.chars().count() + 1 + word.chars().count() > width {
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

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::risk_border_style;
    use moa_core::RiskLevel;

    #[test]
    fn approval_card_uses_expected_border_color_for_each_risk_level() {
        assert_eq!(risk_border_style(&RiskLevel::Low).fg, Some(Color::Green));
        assert_eq!(
            risk_border_style(&RiskLevel::Medium).fg,
            Some(Color::Yellow)
        );
        assert_eq!(risk_border_style(&RiskLevel::High).fg, Some(Color::Red));
    }
}
