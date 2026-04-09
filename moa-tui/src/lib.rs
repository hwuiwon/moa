//! Terminal UI and single-session runtime for local MOA chat flows.

pub mod app;
pub mod keybindings;
pub mod runner;
pub mod views;
pub mod widgets;

pub use app::{App, AppMode, run_tui};
pub use runner::{
    ApprovalPrompt, ChatRuntime, RuntimeCommand, RuntimeEvent, ToolCardStatus, ToolUpdate,
};
