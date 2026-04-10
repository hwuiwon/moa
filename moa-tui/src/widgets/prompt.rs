//! Prompt widget wrapper around `tui-textarea`.

use crossterm::event::KeyEvent;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use tui_textarea::TextArea;

use crate::app::AppMode;

/// Prompt completion kind shown above the textarea.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptCompletionKind {
    Slash,
    File,
}

/// Prompt completion state rendered above the textarea.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptCompletionState {
    /// Type of completion currently shown.
    pub(crate) kind: PromptCompletionKind,
    /// Candidate strings in display order.
    pub(crate) items: Vec<String>,
    /// Selected candidate index.
    pub(crate) selected: usize,
}

/// Multiline prompt input widget used at the bottom of the TUI.
pub struct PromptWidget {
    textarea: TextArea<'static>,
}

impl PromptWidget {
    /// Creates an empty prompt widget.
    pub fn new() -> Self {
        Self {
            textarea: build_textarea(),
        }
    }

    /// Feeds a key event into the underlying text area.
    pub fn input(&mut self, key: KeyEvent) {
        let _ = self.textarea.input(key);
    }

    /// Inserts a newline at the current cursor location.
    pub fn insert_newline(&mut self) {
        self.textarea.insert_str("\n");
    }

    /// Returns the full prompt text.
    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Clears the prompt input back to an empty value.
    pub fn clear(&mut self) {
        self.textarea = build_textarea();
    }

    /// Replaces the prompt text with a new value.
    pub fn replace_text(&mut self, text: impl AsRef<str>) {
        self.textarea = build_textarea();
        self.textarea.insert_str(text.as_ref());
    }

    /// Renders the prompt widget into the given frame area.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect, mode: AppMode) {
        let mut textarea = self.textarea.clone();
        let title = match mode {
            AppMode::WaitingApproval => "Prompt · waiting for approval",
            AppMode::Running => "Prompt · running",
            _ => "Prompt",
        };
        textarea.set_block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(&textarea, area);
    }
}

impl Default for PromptWidget {
    fn default() -> Self {
        Self::new()
    }
}

fn build_textarea() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_placeholder_text("Type a message. Enter to send, Shift+Enter for newline.");
    textarea
}

/// Renders the prompt completion dropdown above the textarea.
pub(crate) fn render_completion_menu(
    frame: &mut Frame<'_>,
    area: Rect,
    completion: &PromptCompletionState,
) {
    if completion.items.is_empty() {
        return;
    }

    let height = (completion.items.len().min(6) as u16).saturating_add(2);
    let popup = Rect {
        x: area.x,
        y: area.y.saturating_sub(height),
        width: area.width.min(72),
        height,
    };

    frame.render_widget(Clear, popup);
    let title = match completion.kind {
        PromptCompletionKind::Slash => "Commands",
        PromptCompletionKind::File => "Files",
    };
    let lines = completion
        .items
        .iter()
        .enumerate()
        .take(6)
        .map(|(index, item)| {
            let style = if index == completion.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(Span::styled(item.clone(), style))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        popup,
    );
}
