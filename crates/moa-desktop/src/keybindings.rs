//! Global keyboard shortcut registrations.
//!
//! Called once at startup from `main.rs`. Contextual bindings scoped to a
//! focusable element (e.g. approval cards, command palette) use the optional
//! key_context string; global bindings pass `None`.

use gpui::{App, KeyBinding};

use crate::actions::{
    ApproveAlways, ApproveOnce, BackToApp, CloseSession, DenyApproval, DismissModal, FocusPrompt,
    FocusSidebar, NewSession, NextSession, OpenCommandPalette, OpenMemoryBrowser, OpenSettings,
    OpenSkillManager, PaletteConfirm, PaletteMoveDown, PaletteMoveUp, PreviousSession, Quit,
    RefreshMemory, SearchMemory, StopSession, ToggleDetailPanel, ToggleSidebar,
};

/// Human-readable table of all keyboard shortcuts, used by the
/// `Keyboard Shortcuts` settings tab. Keep this in lock-step with
/// [`register_keybindings`] — if a binding is added or changed, update
/// both places.
pub const DISPLAY_BINDINGS: &[(&str, &str)] = &[
    ("Command palette", "⌘K"),
    ("Settings", "⌘,"),
    ("Memory browser", "⌘M"),
    ("Skill manager", "⇧⌘K"),
    ("New session", "⌘N"),
    ("Close session", "⌘W"),
    ("Next session", "⌘]"),
    ("Previous session", "⌘["),
    ("Toggle sidebar", "⌘\\"),
    ("Toggle detail panel", "⇧⌘\\"),
    ("Focus prompt", "⌘L"),
    ("Focus sidebar", "⇧⌘L"),
    ("Stop current session", "⌘."),
    ("Search memory", "⇧⌘M"),
    ("Refresh memory", "⌘R"),
    ("Dismiss modal", "Esc"),
    ("Back to app (from Settings)", "Esc"),
    ("Quit", "⌘Q"),
    ("Approve once (on approval card)", "Y"),
    ("Approve always (on approval card)", "A"),
    ("Deny (on approval card)", "N"),
    ("Palette move up", "↑"),
    ("Palette move down", "↓"),
    ("Palette confirm", "Enter"),
];

/// Registers every application-wide key binding. Must run after
/// [`gpui_component::init`] so the global app has a keymap to mutate.
pub fn register_keybindings(cx: &mut App) {
    cx.bind_keys([
        // Global panels
        KeyBinding::new("cmd-k", OpenCommandPalette, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-m", OpenMemoryBrowser, None),
        KeyBinding::new("cmd-shift-k", OpenSkillManager, None),
        // Sessions
        KeyBinding::new("cmd-n", NewSession, None),
        KeyBinding::new("cmd-w", CloseSession, None),
        KeyBinding::new("cmd-]", NextSession, None),
        KeyBinding::new("cmd-[", PreviousSession, None),
        // Layout
        KeyBinding::new("cmd-\\", ToggleSidebar, None),
        KeyBinding::new("cmd-shift-\\", ToggleDetailPanel, None),
        KeyBinding::new("cmd-l", FocusPrompt, None),
        KeyBinding::new("cmd-shift-l", FocusSidebar, None),
        // Session control
        KeyBinding::new("cmd-.", StopSession, None),
        // Memory
        KeyBinding::new("cmd-shift-m", SearchMemory, None),
        KeyBinding::new("cmd-r", RefreshMemory, None),
        // Modal dismiss + quit
        KeyBinding::new("escape", DismissModal, None),
        KeyBinding::new("escape", BackToApp, Some("SettingsPage")),
        KeyBinding::new("cmd-q", Quit, None),
        // Approval shortcuts (contextual — only when an ApprovalCard holds focus)
        KeyBinding::new("y", ApproveOnce, Some("ApprovalCard")),
        KeyBinding::new("a", ApproveAlways, Some("ApprovalCard")),
        KeyBinding::new("n", DenyApproval, Some("ApprovalCard")),
        // Palette list navigation (only inside the palette)
        KeyBinding::new("up", PaletteMoveUp, Some("CommandPalette")),
        KeyBinding::new("down", PaletteMoveDown, Some("CommandPalette")),
        KeyBinding::new("enter", PaletteConfirm, Some("CommandPalette")),
    ]);
}
