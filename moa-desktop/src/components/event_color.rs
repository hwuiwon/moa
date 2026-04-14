//! Centralized color mapping for `Event`/`SpanKind` so the timeline
//! waterfall, chat tool turns, and any future event renderer all agree on
//! which token represents which kind of event. Colors flow through
//! `cx.theme().*`.

use gpui::{App, Hsla};
use gpui_component::ActiveTheme;
use moa_core::Event;

/// Semantic kinds of timeline events. We group several Event variants
/// onto each kind so renderers don't repeat the matching themselves.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventColor {
    User,
    Brain,
    Tool,
    ToolError,
    Memory,
    Approval,
    ApprovalDecided,
    Checkpoint,
    Notice,
    Error,
    Hand,
    Session,
}

impl EventColor {
    pub fn from_event(event: &Event) -> Self {
        match event {
            Event::UserMessage { .. } | Event::QueuedMessage { .. } => Self::User,
            Event::BrainResponse { .. } | Event::BrainThinking { .. } => Self::Brain,
            Event::ToolCall { .. } | Event::ToolResult { .. } => Self::Tool,
            Event::ToolError { .. } => Self::ToolError,
            Event::ApprovalRequested { .. } => Self::Approval,
            Event::ApprovalDecided { .. } => Self::ApprovalDecided,
            Event::MemoryRead { .. } | Event::MemoryWrite { .. } | Event::MemoryIngest { .. } => {
                Self::Memory
            }
            Event::Checkpoint { .. } => Self::Checkpoint,
            Event::Error { .. } => Self::Error,
            Event::Warning { .. } => Self::Notice,
            Event::HandProvisioned { .. }
            | Event::HandDestroyed { .. }
            | Event::HandError { .. } => Self::Hand,
            Event::SessionCreated { .. }
            | Event::SessionStatusChanged { .. }
            | Event::SessionCompleted { .. } => Self::Session,
        }
    }

    /// Theme color for this event kind. `info` is the closest-to-blue
    /// neutral accent in our palette and is reused for the user/brain
    /// distinction via subtle contrast.
    pub fn color(self, cx: &App) -> Hsla {
        let theme = cx.theme();
        match self {
            Self::User => theme.info,
            Self::Brain => theme.primary,
            Self::Tool => theme.success,
            Self::ToolError => theme.danger,
            Self::Memory => theme.accent_foreground,
            Self::Approval => theme.warning,
            Self::ApprovalDecided => theme.success,
            Self::Checkpoint => theme.warning,
            Self::Notice => theme.warning,
            Self::Error => theme.danger,
            Self::Hand => theme.muted_foreground,
            Self::Session => theme.muted_foreground,
        }
    }
}

/// Convenience: directly resolve an `Event` to its theme color.
#[allow(dead_code)]
pub fn event_color(cx: &App, event: &Event) -> Hsla {
    EventColor::from_event(event).color(cx)
}
