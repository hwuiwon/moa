//! Terminal UI and local runtime glue for multi-session MOA chat flows.

pub mod app;
pub mod keybindings;
pub mod runner;
pub mod views;
pub mod widgets;

pub use app::{App, AppMode, run_tui};
pub use runner::ChatRuntime;
