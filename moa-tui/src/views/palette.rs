//! Command-palette state and rendering.

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

/// Static command-palette action definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PaletteAction {
    /// Stable action identifier.
    pub id: &'static str,
    /// Human-readable action label.
    pub label: &'static str,
    /// Display shortcut hint.
    pub shortcut: &'static str,
    /// Optional one-line description.
    pub description: &'static str,
}

/// Stateful command-palette search model.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PaletteState {
    query: String,
    selected: usize,
}

impl PaletteState {
    /// Creates an empty palette state.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns the current fuzzy-search query.
    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    /// Appends a character to the palette query.
    pub(crate) fn push_char(&mut self, ch: char) {
        self.query.push(ch);
        self.selected = 0;
    }

    /// Removes one query character.
    pub(crate) fn backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
    }

    /// Moves the highlighted action upward.
    pub(crate) fn move_up(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = if self.selected == 0 {
            len - 1
        } else {
            self.selected.saturating_sub(1)
        };
    }

    /// Moves the highlighted action downward.
    pub(crate) fn move_down(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected + 1) % len;
    }

    /// Returns the selected action.
    pub(crate) fn selected_action(&self, actions: &[PaletteAction]) -> Option<PaletteAction> {
        filtered_actions(&self.query, actions)
            .get(self.selected)
            .copied()
    }
}

/// Renders the command palette overlay.
pub(crate) fn render_palette(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &PaletteState,
    actions: &[PaletteAction],
) {
    let popup = centered_rect(area, 70, 55);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(8)])
        .split(popup);
    let filtered = filtered_actions(state.query(), actions);

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title("Command Palette"),
        popup,
    );

    frame.render_widget(
        Paragraph::new(format!("Search: {}", state.query()))
            .block(Block::default().borders(Borders::ALL).title("Filter")),
        layout[0],
    );

    let rows = if filtered.is_empty() {
        vec![Line::from("No matching actions.")]
    } else {
        filtered
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let style = if index == state.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(
                    format!(
                        "{}  {}  {}",
                        action.shortcut, action.label, action.description
                    ),
                    style,
                ))
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(rows).block(Block::default().borders(Borders::ALL).title("Actions")),
        layout[1],
    );
}

/// Returns the fuzzy-filtered palette actions ordered by score.
pub(crate) fn filtered_actions(query: &str, actions: &[PaletteAction]) -> Vec<PaletteAction> {
    if query.trim().is_empty() {
        return actions.to_vec();
    }

    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buffer = Vec::new();
    let mut scored = actions
        .iter()
        .filter_map(|action| {
            let haystack = format!(
                "{} {} {}",
                action.label, action.shortcut, action.description
            );
            let score = pattern.score(Utf32Str::new(&haystack, &mut buffer), &mut matcher)?;
            Some((*action, score))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|(left_action, left_score), (right_action, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| left_action.label.cmp(right_action.label))
    });
    scored.into_iter().map(|(action, _)| action).collect()
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

#[cfg(test)]
mod tests {
    use super::{PaletteAction, filtered_actions};

    #[test]
    fn palette_fuzzy_search_prefers_matching_actions() {
        let actions = vec![
            PaletteAction {
                id: "new",
                label: "New Session",
                shortcut: "Ctrl+N",
                description: "Start a new chat session",
            },
            PaletteAction {
                id: "memory",
                label: "Open Memory Browser",
                shortcut: "Ctrl+M",
                description: "Browse wiki memory pages",
            },
        ];

        let filtered = filtered_actions("memory", &actions);
        assert_eq!(filtered.first().map(|item| item.id), Some("memory"));
    }
}
