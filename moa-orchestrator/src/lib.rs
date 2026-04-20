//! Local multi-session orchestrator and supporting runtime surfaces.

mod brain_bridge;
pub mod config;
pub mod local;
pub mod objects;
mod observability;
pub mod runtime;
pub mod services;
mod session_engine;
mod sub_agent_dispatch;
pub mod workflows;

pub use local::LocalOrchestrator;
