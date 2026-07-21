//! Tool action policies and rule storage abstractions.

pub mod injection;
pub mod mcp_egress;
pub mod mcp_proxy;
pub mod policies;

pub use injection::{
    InputClassification, InputInspection, ToolInputCanaryLeak, ToolInputCanaryScreening,
    canary_system_message, inject_canary, inspect_input, new_canary_token,
    screen_tool_input_for_canary, wrap_untrusted_tool_output,
};
pub use mcp_egress::{McpEgressError, McpEgressGuard, McpEgressPolicy};
pub use mcp_proxy::{EnvironmentCredentialVault, MCPCredentialProxy};
pub use policies::{
    ActionPolicies, ActionPolicyCheck, ActionPolicyContext, ActionPolicyRuleStore, glob_match,
    parse_and_match_command, stricter_effect, validate_action_policy_rule,
};
