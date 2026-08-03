//! Tool action policies and rule storage abstractions.

pub mod call_origin;
pub mod injection;
pub mod mcp_catalog_policy;
pub mod mcp_credentials;
pub mod mcp_egress;
pub mod outbound_http;
pub mod policies;

pub use call_origin::admit_capability_for_origin;
pub use injection::{
    AssessmentApplication, CircuitTarget, OutputClassification, SecurityCircuitOwnerMismatch,
    ToolInputCanaryLeak, ToolInputCanaryScreening, apply_assessment, apply_owner_assessment,
    canary_system_message, classify_tool_output, inject_canary, new_canary_token,
    screen_tool_input_for_canary, wrap_untrusted_tool_output,
};
pub use mcp_catalog_policy::{
    ConnectorCandidateFacts, ConnectorPolicyDefect, ConnectorPolicyReport, ConnectorPolicyWarning,
    check_connector_policy,
};
pub use mcp_credentials::McpDeploymentCredentials;
pub use mcp_egress::{McpEgressError, McpEgressGuard, McpEgressPolicy};
pub use outbound_http::{
    AdmittedHttpDestination, OutboundHostResolutionError, OutboundHostResolver,
    OutboundHttpAdmissionError, OutboundHttpPolicy, TokioOutboundHostResolver,
};
pub use policies::{
    ActionPolicies, ActionPolicyCheck, ActionPolicyContext, ActionPolicyRuleStore,
    UnmatchedPermissionPattern, glob_match, parse_and_match_command, stricter_effect,
    validate_action_policy_rule,
};
