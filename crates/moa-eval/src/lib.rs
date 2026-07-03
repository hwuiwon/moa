//! Offline evaluation harnesses and internal-improvement runners for MOA.
#![recursion_limit = "256"]

pub mod collector;
pub mod engine;
pub mod golden;
pub mod kernel;
pub mod long_conversation;
pub mod memory_eval;
pub mod pentest;
pub mod reporter;
pub mod reporters;
pub mod setup;

pub use collector::TrajectoryCollector;
pub use engine::EvalEngine;
pub use reporter::Reporter;
pub use reporters::JsonReporter;
pub use reporters::{ReporterOptions, TerminalReporter, build_reporters};
pub use setup::{AgentEnvironment, EvalLineageHandle, build_agent_environment};
