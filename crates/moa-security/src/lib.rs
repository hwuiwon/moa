//! Tool permission policies and approval rule storage abstractions.

pub mod injection;
pub mod mcp_proxy;
pub mod policies;
pub mod vault;

pub use injection::{
    InputClassification, InputInspection, ToolInputCanaryLeak, ToolInputCanaryScreening,
    canary_system_message, check_canary, classify_input, contains_canary_tokens, inject_canary,
    inspect_input, new_canary_token, screen_tool_input_for_canary, wrap_untrusted_tool_output,
};
pub use mcp_proxy::{EnvironmentCredentialVault, MCPCredentialProxy, McpSessionToken};
pub use policies::{
    ApprovalRuleStore, PolicyCheck, ToolPolicies, ToolPolicyContext, glob_match,
    parse_and_match_bash,
};
pub use vault::FileVault;
