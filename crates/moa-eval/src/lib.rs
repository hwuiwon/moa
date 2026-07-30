//! Offline evaluation harnesses and internal-improvement runners for MOA.
#![recursion_limit = "256"]

pub mod collector;
pub mod engine;
pub mod execution;
pub mod external_memory;
pub mod golden;
pub mod kernel;
pub mod long_conversation;
pub mod memory_eval;
pub mod mock_domain;
pub mod pentest;
pub mod plan;
pub mod setup;

pub use engine::EvalEngine;
pub use plan::build_eval_plan;
pub use setup::{AgentEnvironment, EvalLineageHandle, build_agent_environment};

pub(crate) fn eval_sqlx_error(error: sqlx::Error) -> moa_eval_core::Error {
    moa_eval_core::Error::Storage(error.to_string())
}
