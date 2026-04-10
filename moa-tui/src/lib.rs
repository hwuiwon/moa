//! Terminal UI and local runtime glue for multi-session MOA chat flows.

pub mod app;
pub mod keybindings;
pub mod runner;
pub mod views;
pub mod widgets;

pub use app::{App, AppMode, RunTuiOptions, run_tui, run_tui_with_options};
pub use runner::ChatRuntime;
