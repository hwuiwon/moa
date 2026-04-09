//! Keybinding dispatch for the basic Step 08 TUI.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::AppMode;

/// High-level action derived from a keyboard event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// Submit the current prompt.
    Submit,
    /// Insert a newline into the prompt.
    InsertNewline,
    /// Cancel the current generation or exit when idle.
    Cancel,
    /// Approve the pending tool exactly once.
    ApproveOnce,
    /// Persist an always-allow rule for the pending tool.
    AlwaysAllow,
    /// Deny the pending tool.
    Deny,
    /// Scroll the transcript upward.
    ScrollUp,
    /// Scroll the transcript downward.
    ScrollDown,
    /// Jump back to auto-scroll at the bottom.
    ScrollEnd,
    /// Forward the key into the prompt widget.
    PromptInput,
    /// Ignore the key press.
    Noop,
}

/// Maps a raw key event into a higher-level app action.
pub fn map_key_event(mode: AppMode, key: KeyEvent) -> KeyAction {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return KeyAction::Cancel;
    }

    if key.code == KeyCode::Esc {
        return KeyAction::Cancel;
    }

    if mode == AppMode::WaitingApproval {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => return KeyAction::ApproveOnce,
            KeyCode::Char('a') | KeyCode::Char('A') => return KeyAction::AlwaysAllow,
            KeyCode::Char('n') | KeyCode::Char('N') => return KeyAction::Deny,
            _ => {}
        }
    }

    match key.code {
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => KeyAction::InsertNewline,
        KeyCode::Enter => KeyAction::Submit,
        KeyCode::Up | KeyCode::PageUp => KeyAction::ScrollUp,
        KeyCode::Down | KeyCode::PageDown => KeyAction::ScrollDown,
        KeyCode::End => KeyAction::ScrollEnd,
        KeyCode::Char(_)
        | KeyCode::Backspace
        | KeyCode::Delete
        | KeyCode::Tab
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Home => KeyAction::PromptInput,
        _ => KeyAction::Noop,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventKind, KeyEventState};

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn enter_submits_but_shift_enter_inserts_newline() {
        assert_eq!(
            map_key_event(AppMode::Composing, key(KeyCode::Enter, KeyModifiers::NONE)),
            KeyAction::Submit
        );
        assert_eq!(
            map_key_event(AppMode::Composing, key(KeyCode::Enter, KeyModifiers::SHIFT)),
            KeyAction::InsertNewline
        );
    }

    #[test]
    fn approval_shortcuts_only_activate_while_waiting() {
        assert_eq!(
            map_key_event(
                AppMode::WaitingApproval,
                key(KeyCode::Char('y'), KeyModifiers::NONE)
            ),
            KeyAction::ApproveOnce
        );
        assert_eq!(
            map_key_event(
                AppMode::Running,
                key(KeyCode::Char('y'), KeyModifiers::NONE)
            ),
            KeyAction::PromptInput
        );
    }

    #[test]
    fn ctrl_c_and_escape_cancel() {
        assert_eq!(
            map_key_event(
                AppMode::Running,
                key(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            KeyAction::Cancel
        );
        assert_eq!(
            map_key_event(AppMode::Running, key(KeyCode::Esc, KeyModifiers::NONE)),
            KeyAction::Cancel
        );
    }
}
