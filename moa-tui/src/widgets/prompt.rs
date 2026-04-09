//! Prompt widget wrapper around `tui-textarea`.

use crossterm::event::KeyEvent;
use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Block, Borders},
};
use tui_textarea::TextArea;

use crate::app::AppMode;

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
