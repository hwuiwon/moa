//! Chat transcript rendering for the basic TUI view.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
};

use crate::{
    app::ChatEntry,
    widgets::{approval::render_approval_card, tool_card::render_tool_card},
};

/// Returns the maximum vertical scroll offset for the current transcript.
pub(crate) fn max_scroll(entries: &[ChatEntry], width: u16, height: u16) -> u16 {
    let line_count = transcript_lines(entries, width).len() as u16;
    line_count.saturating_sub(height.saturating_sub(2))
}

/// Renders the chat transcript into the given viewport.
pub(crate) fn render_chat(frame: &mut Frame<'_>, area: Rect, entries: &[ChatEntry], scroll: u16) {
    let lines = transcript_lines(entries, area.width.saturating_sub(2));
    let paragraph = Paragraph::new(Text::from(lines))
        .block(Block::default().borders(Borders::ALL).title("Chat"))
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn transcript_lines(entries: &[ChatEntry], width: u16) -> Vec<Line<'static>> {
    let content_width = width.max(24) as usize;
    let mut lines = Vec::new();

    for entry in entries {
        match entry {
            ChatEntry::User(text) => {
                lines.push(Line::from(vec![Span::styled(
                    "You",
                    Style::default().add_modifier(Modifier::BOLD),
                )]));
                lines.extend(wrap_prefixed(text, content_width));
            }
            ChatEntry::Assistant { text, streaming } => {
                let title = if *streaming {
                    "Assistant · streaming"
                } else {
                    "Assistant"
                };
                lines.push(Line::from(vec![Span::styled(
                    title,
                    Style::default().add_modifier(Modifier::BOLD),
                )]));
                lines.extend(wrap_prefixed(text, content_width));
            }
            ChatEntry::Approval(card) => {
                lines.extend(render_approval_card(card, width));
            }
            ChatEntry::Status(text) => {
                lines.push(Line::from(vec![Span::styled(
                    "Status",
                    Style::default().add_modifier(Modifier::BOLD),
                )]));
                lines.extend(wrap_prefixed(text, content_width));
            }
            ChatEntry::Tool(card) => {
                lines.extend(render_tool_card(card, width));
            }
        }
        lines.push(Line::raw(String::new()));
    }

    if lines.is_empty() {
        lines.push(Line::raw("No messages yet."));
    }

    lines
}

fn wrap_prefixed(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for raw_line in text.lines() {
        let wrapped = wrap_line(raw_line, width.saturating_sub(2));
        for line in wrapped {
            lines.push(Line::raw(format!("  {line}")));
        }
    }

    if lines.is_empty() {
        lines.push(Line::raw("  "));
    }

    lines
}

fn wrap_line(text: &str, width: usize) -> Vec<String> {
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

    if text.trim().is_empty() {
        lines.push(String::new());
    } else if !current.is_empty() {
        lines.push(current);
    }

    lines
}
