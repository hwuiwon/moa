//! Prompt widget wrapper around `tui-textarea`.

use crossterm::event::KeyEvent;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use tui_textarea::{CursorMove, TextArea};

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
    /// Absolute byte range in the prompt that will be replaced when accepted.
    pub(crate) replace_range: std::ops::Range<usize>,
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

    /// Returns the current cursor position as `(row, column)`.
    pub fn cursor(&self) -> (usize, usize) {
        self.textarea.cursor()
    }

    /// Returns the current cursor offset as a byte index into [`Self::text`].
    pub fn cursor_offset(&self) -> usize {
        let (row, column) = self.cursor();
        offset_for_position(self.textarea.lines(), row, column)
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

    /// Replaces the full prompt text and restores the cursor to the provided byte offset.
    pub fn replace_text_with_cursor(&mut self, text: impl AsRef<str>, cursor_offset: usize) {
        let text = text.as_ref();
        self.textarea = build_textarea();
        self.textarea.insert_str(text);
        let (row, column) = position_for_offset(text, cursor_offset.min(text.len()));
        self.textarea.move_cursor(CursorMove::Jump(
            row.min(u16::MAX as usize) as u16,
            column.min(u16::MAX as usize) as u16,
        ));
    }

    /// Replaces a byte range in the prompt and places the cursor at the end of the inserted text.
    pub fn replace_range(&mut self, range: std::ops::Range<usize>, replacement: &str) {
        let mut text = self.text();
        text.replace_range(range.clone(), replacement);
        self.replace_text_with_cursor(text, range.start + replacement.len());
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

fn offset_for_position(lines: &[String], row: usize, column: usize) -> usize {
    let mut offset = 0usize;
    for (line_index, line) in lines.iter().enumerate() {
        if line_index == row {
            return offset + byte_index_for_char_position(line, column);
        }
        offset += line.len();
        if line_index + 1 < lines.len() {
            offset += 1;
        }
    }
    offset
}

fn position_for_offset(text: &str, offset: usize) -> (usize, usize) {
    let clamped = offset.min(text.len());
    let mut row = 0usize;
    let mut column = 0usize;
    for (index, ch) in text.char_indices() {
        if index >= clamped {
            break;
        }
        if ch == '\n' {
            row += 1;
            column = 0;
        } else {
            column += 1;
        }
    }
    (row, column)
}

fn byte_index_for_char_position(text: &str, char_position: usize) -> usize {
    if char_position == 0 {
        return 0;
    }
    text.char_indices()
        .nth(char_position)
        .map(|(index, _)| index)
        .unwrap_or(text.len())
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
