//! Manually-run integration coverage for external Restate round-trips.

#[path = "integration/action_policy_flow_e2e.rs"]
mod action_policy_flow_e2e;
#[path = "integration/agent_artifacts_e2e.rs"]
mod agent_artifacts_e2e;
#[path = "integration/consolidate_e2e.rs"]
mod consolidate_e2e;
#[path = "integration/guardrails_e2e.rs"]
mod guardrails_e2e;
#[path = "integration/session_brain_e2e.rs"]
mod session_brain_e2e;
#[path = "integration/session_store_e2e.rs"]
mod session_store_e2e;
#[path = "integration/session_vo_e2e.rs"]
mod session_vo_e2e;
mod support;
#[path = "integration/tool_executor_e2e.rs"]
mod tool_executor_e2e;
#[path = "integration/turn_responsiveness_e2e.rs"]
mod turn_responsiveness_e2e;
