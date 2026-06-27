//! Shared collaborators for root and sub-agent turn workflows.
//!
//! These modules own deterministic helper logic and workflow state-key names
//! that are common to turn execution. Durable Restate workflow boundaries stay
//! in `workflows::turn_execution` and `workflows::sub_agent_turn_execution`.

pub(crate) mod guardrails;
pub(crate) mod learning;
pub(crate) mod model_loop;
pub(crate) mod progress;
pub(crate) mod segments;
