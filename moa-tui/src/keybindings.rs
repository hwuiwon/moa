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
    /// Queue the current prompt behind a running turn.
    QueuePrompt,
    /// Approve the pending tool exactly once.
    ApproveOnce,
    /// Persist an always-allow rule for the pending tool.
    AlwaysAllow,
    /// Deny the pending tool.
    Deny,
    /// Open the full-screen diff viewer for the focused approval.
    OpenDiff,
    /// Close the full-screen diff viewer.
    CloseDiff,
    /// Toggle unified versus side-by-side diff rendering.
    ToggleDiffMode,
    /// Move to the next file in the diff viewer.
    NextDiffFile,
    /// Move to the previous file in the diff viewer.
    PreviousDiffFile,
    /// Move to the next hunk in the diff viewer.
    NextDiffHunk,
    /// Move to the previous hunk in the diff viewer.
    PreviousDiffHunk,
    /// Edit approval parameters.
    EditApproval,
    /// Scroll the transcript upward.
    ScrollUp,
    /// Scroll the transcript downward.
    ScrollDown,
    /// Jump back to auto-scroll at the bottom.
    ScrollEnd,
    /// Create a new session tab.
    NewSession,
    /// Cycle to the next visible session tab.
    NextSession,
    /// Cycle to the previous visible session tab.
    PreviousSession,
    /// Switch directly to the indexed session tab.
    SwitchSessionTab(usize),
    /// Start the `Ctrl+O, S` session-picker chord.
    StartSessionPickerChord,
    /// Start the `Ctrl+X, S` soft-stop chord.
    StartSoftStopChord,
    /// Move the session picker selection upward.
    PickerUp,
    /// Move the session picker selection downward.
    PickerDown,
    /// Confirm the currently selected session picker entry.
    PickerSelect,
    /// Remove one character from the session picker query.
    PickerBackspace,
    /// Forward a key into the session picker query.
    SessionPickerInput,
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

    if mode == AppMode::PickingSession {
        return match key.code {
            KeyCode::Esc => KeyAction::Cancel,
            KeyCode::Enter => KeyAction::PickerSelect,
            KeyCode::Up => KeyAction::PickerUp,
            KeyCode::Down => KeyAction::PickerDown,
            KeyCode::Backspace => KeyAction::PickerBackspace,
            KeyCode::Char(_) | KeyCode::Delete => KeyAction::SessionPickerInput,
            _ => KeyAction::Noop,
        };
    }

    if mode == AppMode::ViewingDiff {
        return match key.code {
            KeyCode::Esc | KeyCode::Char('f') | KeyCode::Char('F') => KeyAction::CloseDiff,
            KeyCode::Char('t') | KeyCode::Char('T') => KeyAction::ToggleDiffMode,
            KeyCode::Char('n') => KeyAction::NextDiffFile,
            KeyCode::Char('N') => KeyAction::PreviousDiffFile,
            KeyCode::Char('j') | KeyCode::Down => KeyAction::NextDiffHunk,
            KeyCode::Char('k') | KeyCode::Up => KeyAction::PreviousDiffHunk,
            KeyCode::Char('a') | KeyCode::Char('A') => KeyAction::ApproveOnce,
            KeyCode::Char('r') | KeyCode::Char('R') => KeyAction::Deny,
            _ => KeyAction::Noop,
        };
    }

    if key.code == KeyCode::Esc {
        return KeyAction::Cancel;
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('n') | KeyCode::Char('N') => KeyAction::NewSession,
            KeyCode::Char('o') | KeyCode::Char('O') => KeyAction::StartSessionPickerChord,
            KeyCode::Char('q') | KeyCode::Char('Q') => KeyAction::QueuePrompt,
            KeyCode::Char('x') | KeyCode::Char('X') => KeyAction::StartSoftStopChord,
            _ => KeyAction::Noop,
        };
    }

    if key.modifiers.contains(KeyModifiers::ALT) {
        return match key.code {
            KeyCode::Char('[') => KeyAction::PreviousSession,
            KeyCode::Char(']') => KeyAction::NextSession,
            KeyCode::Char(ch @ '1'..='9') => {
                KeyAction::SwitchSessionTab((ch as usize) - ('1' as usize))
            }
            _ => KeyAction::Noop,
        };
    }

    if mode == AppMode::WaitingApproval {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => return KeyAction::ApproveOnce,
            KeyCode::Char('a') | KeyCode::Char('A') => return KeyAction::AlwaysAllow,
            KeyCode::Char('n') | KeyCode::Char('N') => return KeyAction::Deny,
            KeyCode::Char('d') | KeyCode::Char('D') => return KeyAction::OpenDiff,
            KeyCode::Char('e') | KeyCode::Char('E') => return KeyAction::EditApproval,
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
        assert_eq!(
            map_key_event(
                AppMode::WaitingApproval,
                key(KeyCode::Char('d'), KeyModifiers::NONE)
            ),
            KeyAction::OpenDiff
        );
    }

    #[test]
    fn diff_view_shortcuts_are_scoped_to_the_overlay() {
        assert_eq!(
            map_key_event(
                AppMode::ViewingDiff,
                key(KeyCode::Char('t'), KeyModifiers::NONE)
            ),
            KeyAction::ToggleDiffMode
        );
        assert_eq!(
            map_key_event(
                AppMode::ViewingDiff,
                key(KeyCode::Char('a'), KeyModifiers::NONE)
            ),
            KeyAction::ApproveOnce
        );
        assert_eq!(
            map_key_event(AppMode::ViewingDiff, key(KeyCode::Esc, KeyModifiers::NONE)),
            KeyAction::CloseDiff
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

    #[test]
    fn tab_and_session_shortcuts_map_correctly() {
        assert_eq!(
            map_key_event(
                AppMode::Idle,
                key(KeyCode::Char('n'), KeyModifiers::CONTROL)
            ),
            KeyAction::NewSession
        );
        assert_eq!(
            map_key_event(AppMode::Idle, key(KeyCode::Char(']'), KeyModifiers::ALT)),
            KeyAction::NextSession
        );
        assert_eq!(
            map_key_event(AppMode::Idle, key(KeyCode::Char('1'), KeyModifiers::ALT)),
            KeyAction::SwitchSessionTab(0)
        );
        assert_eq!(
            map_key_event(
                AppMode::Idle,
                key(KeyCode::Char('o'), KeyModifiers::CONTROL)
            ),
            KeyAction::StartSessionPickerChord
        );
    }

    #[test]
    fn session_picker_keys_are_scoped_to_picker_mode() {
        assert_eq!(
            map_key_event(
                AppMode::PickingSession,
                key(KeyCode::Down, KeyModifiers::NONE)
            ),
            KeyAction::PickerDown
        );
        assert_eq!(
            map_key_event(
                AppMode::PickingSession,
                key(KeyCode::Char('s'), KeyModifiers::NONE)
            ),
            KeyAction::SessionPickerInput
        );
        assert_eq!(
            map_key_event(
                AppMode::Composing,
                key(KeyCode::Char('s'), KeyModifiers::NONE)
            ),
            KeyAction::PromptInput
        );
    }
}
