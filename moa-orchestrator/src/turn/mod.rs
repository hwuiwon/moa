//! Agent turn execution shared by Session and SubAgent virtual objects.
//!
//! The turn loop shape is the same across both durable actors: compile the next
//! request, call the LLM, run approvals, dispatch tools, and map the response
//! back into a durable turn outcome. Agent-specific state and history behavior
//! is expressed through [`AgentAdapter`].

pub(crate) mod adapter;
pub(crate) mod approval;
pub(crate) mod runner;
pub(crate) mod util;

// The canonical home for span helpers is `crate::observability`; the turn
// module re-exports them for compact internal imports.
pub(crate) use crate::observability::{event_persist_span, llm_call_span, tool_dispatch_span};
pub(crate) use adapter::AgentAdapter;
pub(crate) use runner::TurnRunner;
