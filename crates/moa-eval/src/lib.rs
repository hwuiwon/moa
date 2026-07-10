//! Offline evaluation harnesses and internal-improvement runners for MOA.
#![recursion_limit = "256"]

pub mod collector;
pub mod engine;
pub mod external_memory;
pub mod golden;
pub mod kernel;
pub mod long_conversation;
pub mod memory_eval;
pub mod pentest;
pub mod setup;

pub use collector::TrajectoryCollector;
pub use engine::EvalEngine;
pub use setup::{AgentEnvironment, EvalLineageHandle, build_agent_environment};
