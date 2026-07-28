//! Tool registration and default loadout definitions.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::adapters::mcp::McpDiscoveredToolRegistration;
use crate::tools::{memory, session_search, tool_result};
use moa_config::McpServerCredentialScope;
use moa_config::ToolBudgetConfig;
use moa_core::{
    error::Result, traits::BuiltInTool, types::action_policy::ActionClass,
    types::action_policy::ActionPolicyEffect, types::hands::BuiltinPolicyRevision,
    types::hands::SandboxPolicySnapshot, types::hands::SandboxTier,
    types::security::ToolCapabilityId, types::tools::IdempotencyClass,
    types::tools::ToolDefinition, types::tools::ToolDiffStrategy, types::tools::ToolInputShape,
    types::tools::ToolPolicySpec,
};
use serde_json::Value;

use crate::tools::sandbox_descriptor::{
    SandboxToolDescriptor, default_sandbox_tool_descriptors, sandbox_tool_descriptors,
};

use super::mcp_connections::ToolCredentialScope;
use super::{DEFAULT_PROVIDER_NAME, ToolRouter};

/// One provider route for a hand-routed tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandRoute {
    /// Registered provider name.
    pub provider: String,
    /// Sandbox tier requested from the provider.
    pub tier: SandboxTier,
    /// The route's sandbox policy layer, innermost of the four intersected into
    /// the effective profile. Required: a route with no authored policy carries
    /// the named [`BuiltinPolicyRevision::RouteUnset`] layer, never an absent
    /// one, so the layer is always visible in the policy identity hash.
    pub policy: SandboxPolicySnapshot,
}

impl HandRoute {
    /// Creates the built-in local route with no authored policy layer.
    pub(super) fn local() -> Self {
        Self {
            provider: DEFAULT_PROVIDER_NAME.to_string(),
            tier: SandboxTier::Local,
            policy: SandboxPolicySnapshot::builtin(BuiltinPolicyRevision::RouteUnset),
        }
    }
}

/// Tool execution routing target.
#[derive(Clone)]
pub enum ToolExecution {
    /// Built-in Rust implementation.
    BuiltIn(Arc<dyn BuiltInTool>),
    /// Routed to a provisioned hand.
    Hand { routes: Vec<HandRoute> },
    /// Routed to a configured MCP server.
    Mcp {
        /// Configured MCP server that owns the remote tool.
        server_name: String,
        /// Credential owner the server is invoked with, copied from that
        /// server's configuration when the tool was discovered. Dispatch requires
        /// it to still agree with the live server configuration, so a server that
        /// changes ownership cannot serve a tool registered under the other
        /// owner's scope.
        credential_scope: McpServerCredentialScope,
        /// Tool name as the owning server knows it.
        ///
        /// The registered name is server-qualified, so this is the only name
        /// that may be sent back to the server in a `tools/call`.
        remote_tool_name: String,
        /// Schema revision this registration was discovered at.
        ///
        /// Two catalogs that discovered the same tool at the same schema produce
        /// the same hash, and a server that changes a tool's input schema
        /// produces a different one. It is the capability version this tool
        /// enters the execution capability catalog under, so a pinned execution
        /// run whose connector changed a schema fails its catalog check instead
        /// of invoking the changed tool.
        schema_hash: String,
    },
}

impl ToolExecution {
    /// Returns the canonical capability identity the security circuit keys on.
    ///
    /// Resolved from the registry rather than from the caller, and — for hand
    /// tools — from the logical tool alone rather than from the route that ends
    /// up serving it. Falling back from one sandbox provider to another therefore
    /// keeps one capability identity, so a tripped circuit cannot be reset by
    /// provoking a fallback.
    #[must_use]
    pub fn capability_id(&self, tool_name: &str) -> ToolCapabilityId {
        match self {
            Self::BuiltIn(_) => ToolCapabilityId::builtin(tool_name),
            Self::Hand { .. } => ToolCapabilityId::hand(tool_name),
            Self::Mcp {
                server_name,
                remote_tool_name,
                ..
            } => ToolCapabilityId::mcp(server_name, remote_tool_name),
        }
    }

    /// Returns the credential scope every invocation of this tool carries.
    #[must_use]
    pub fn credential_scope(&self) -> ToolCredentialScope {
        match self {
            Self::BuiltIn(_) | Self::Hand { .. } => ToolCredentialScope::NonMcp,
            Self::Mcp {
                credential_scope, ..
            } => ToolCredentialScope::for_server(*credential_scope),
        }
    }
}

/// Prefix every server-qualified MCP tool reference carries.
///
/// It is part of the stable reference rather than a display convention: it is
/// what makes a discovered remote name structurally unable to collide with a
/// built-in or hand tool name.
pub const MCP_TOOL_REFERENCE_PREFIX: &str = "mcp__";

/// Builds the stable server-qualified reference a discovered MCP tool registers
/// under.
///
/// Registration is keyed by this reference, not by the remote name, so one
/// connector exposing `search` can never displace another connector's `search`
/// nor collide with a local tool — the collision that used to fail router
/// construction outright.
#[must_use]
pub fn mcp_tool_reference(server_name: &str, remote_tool_name: &str) -> String {
    format!("{MCP_TOOL_REFERENCE_PREFIX}{server_name}__{remote_tool_name}")
}

/// Returns whether a name is model-safe for the provider tool-calling APIs.
///
/// Anthropic and OpenAI both accept `[A-Za-z0-9_-]{1,128}` tool names. A
/// discovered remote name outside that set is rejected at registration with a
/// diagnostic rather than sent to a provider that would reject the whole
/// request, which would take every other tool in the loadout down with it.
fn is_model_safe_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Computes the schema revision hash for one discovered MCP tool.
///
/// Domain-separated and length-prefixed so no two different (reference, schema,
/// protocol) triples can produce the same digest by concatenation, and computed
/// over canonical JSON so a server that reorders its schema keys does not
/// present as a schema change.
fn mcp_schema_hash(
    reference: &str,
    input_schema: &Value,
    protocol_version: Option<&str>,
) -> String {
    fn absorb(hasher: &mut Sha256, part: &[u8]) {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }

    let mut hasher = Sha256::new();
    hasher.update(b"moa.mcp.tool-schema.v1");
    absorb(&mut hasher, reference.as_bytes());
    absorb(&mut hasher, canonical_schema_bytes(input_schema).as_bytes());
    absorb(&mut hasher, protocol_version.unwrap_or_default().as_bytes());
    hex::encode(hasher.finalize())
}

/// Serializes a schema with sorted object keys so the hash is order-stable.
fn canonical_schema_bytes(schema: &Value) -> String {
    let mut canonical = schema.clone();
    sort_json_keys(&mut canonical);
    canonical.to_string()
}

fn sort_json_keys(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(sort_json_keys),
        Value::Object(map) => {
            let mut ordered = map
                .iter()
                .map(|(key, value)| {
                    let mut value = value.clone();
                    sort_json_keys(&mut value);
                    (key.clone(), value)
                })
                .collect::<Vec<_>>();
            ordered.sort_by(|left, right| left.0.cmp(&right.0));
            map.clear();
            for (key, value) in ordered {
                map.insert(key, value);
            }
        }
        _ => {}
    }
}

#[derive(Clone)]
pub(super) struct RegisteredTool {
    pub(super) definition: ToolDefinition,
    pub(super) execution: ToolExecution,
}

impl RegisteredTool {
    fn builtin(tool: Arc<dyn BuiltInTool>) -> Self {
        Self {
            definition: tool.definition(),
            execution: ToolExecution::BuiltIn(tool),
        }
    }

    fn hand(
        name: &str,
        description: &str,
        schema: Value,
        policy: ToolPolicySpec,
        idempotency_class: IdempotencyClass,
    ) -> Self {
        Self {
            definition: ToolDefinition {
                name: name.to_string(),
                description: description.to_string(),
                schema,
                policy,
                idempotency_class,
                max_output_tokens: default_budget_for_tool(name),
            },
            execution: ToolExecution::Hand {
                routes: vec![HandRoute::local()],
            },
        }
    }

    fn sandbox_hand(descriptor: &SandboxToolDescriptor) -> Self {
        Self {
            definition: descriptor.definition(default_budget_for_tool(descriptor.name)),
            execution: ToolExecution::Hand {
                routes: vec![HandRoute::local()],
            },
        }
    }

    fn mcp(
        server_name: &str,
        credential_scope: McpServerCredentialScope,
        registration: McpDiscoveredToolRegistration,
    ) -> Self {
        let idempotency_class = if registration.allows_idempotent_retry() {
            IdempotencyClass::Idempotent
        } else {
            IdempotencyClass::NonIdempotent
        };
        let protocol_version = registration
            .negotiated_protocol_version()
            .map(ToOwned::to_owned);
        let tool = registration.into_tool();
        let remote_tool_name = tool.name;
        let name = mcp_tool_reference(server_name, &remote_tool_name);
        let schema_hash = mcp_schema_hash(&name, &tool.input_schema, protocol_version.as_deref());
        Self {
            definition: ToolDefinition {
                name: name.clone(),
                description: tool.description,
                schema: tool.input_schema,
                policy: ToolPolicySpec {
                    risk_level: moa_core::types::action_policy::RiskLevel::High,
                    // MCP/third-party tools have no considered per-tool descriptor
                    // gate (unlike builtins), so they default to admin review rather
                    // than a bare allow: unvetted external code should not execute
                    // unattended.
                    //
                    // This is a *cautious default*, not a floor. `ActionPolicies::check`
                    // implements two tiers: an explicit matched persisted rule may lift
                    // an intrinsic `AdminReview`, while intrinsic `Deny` and configured
                    // `permissions.always_deny`/`admin_review` overrides are unliftable.
                    // So an operator rule still wins here, and a deployment that wants
                    // external tools review-locked regardless of tenant rules configures
                    // the override instead.
                    default_effect: ActionPolicyEffect::AdminReview,
                    action_class: ActionClass::ExternalWrite,
                    input_shape: ToolInputShape::Json,
                    diff_strategy: ToolDiffStrategy::None,
                },
                idempotency_class,
                max_output_tokens: 8_000,
            },
            execution: ToolExecution::Mcp {
                server_name: server_name.to_string(),
                credential_scope,
                remote_tool_name,
                schema_hash,
            },
        }
    }
}

/// In-memory registry of available tools.
///
/// `default_loadout` is an authored, ordered list: built-ins first, then the
/// default sandbox descriptors in their declared order, then discovered MCP
/// tools in catalog order. That order is the deployment's declared capability
/// priority and is what a consumer with a schema cap must reduce along — never
/// the lexical order of tool names, which is unrelated to how much a loadout
/// needs a tool.
#[derive(Clone)]
pub struct ToolRegistry {
    pub(super) tools: HashMap<String, RegisteredTool>,
    default_loadout: Vec<String>,
}

impl ToolRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            default_loadout: Vec::new(),
        }
    }

    /// Returns the canonical local registry for Step 06.
    pub fn default_local() -> Self {
        let mut registry = Self::new();
        registry.register_builtin(Arc::new(memory::MemoryRememberTool));
        registry.register_builtin(Arc::new(memory::MemoryForgetTool));
        registry.register_builtin(Arc::new(memory::MemorySupersedeTool));
        // Registered so they can execute when the brain gates them onto an
        // agentic turn, but deliberately kept out of `default_loadout` below so
        // they never appear in the default prompt.
        registry.register_builtin(Arc::new(memory::MemorySearchTool));
        registry.register_builtin(Arc::new(memory::MemoryNavigateTool));
        registry.register_builtin(Arc::new(session_search::SessionSearchTool));
        registry.register_builtin(Arc::new(tool_result::ToolResultReadTool));
        registry.register_builtin(Arc::new(tool_result::ToolResultSearchTool));
        for descriptor in sandbox_tool_descriptors() {
            registry.register_sandbox_tool(descriptor);
        }
        registry.default_loadout = [
            "memory_remember".to_string(),
            "memory_forget".to_string(),
            "memory_supersede".to_string(),
            "session_search".to_string(),
            "tool_result_read".to_string(),
            "tool_result_search".to_string(),
        ]
        .into_iter()
        .chain(
            default_sandbox_tool_descriptors()
                .into_iter()
                .map(|descriptor| descriptor.name.to_string()),
        )
        .collect();
        registry
    }

    /// Registers a built-in tool.
    pub fn register_builtin(&mut self, tool: Arc<dyn BuiltInTool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, RegisteredTool::builtin(tool));
    }

    /// Registers a hand-routed tool using the local provider.
    pub fn register_hand(
        &mut self,
        name: &str,
        description: &str,
        schema: Value,
        policy: ToolPolicySpec,
        idempotency_class: IdempotencyClass,
    ) {
        self.tools.insert(
            name.to_string(),
            RegisteredTool::hand(name, description, schema, policy, idempotency_class),
        );
    }

    fn register_sandbox_tool(&mut self, descriptor: &SandboxToolDescriptor) {
        self.tools.insert(
            descriptor.name.to_string(),
            RegisteredTool::sandbox_hand(descriptor),
        );
    }

    /// Registers a discovered MCP tool under its server-qualified reference and
    /// adds it to the default loadout.
    ///
    /// `credential_scope` is the owning server's configured credential scope. It
    /// is recorded on the registration so every invocation of this tool carries a
    /// credential scope derived from operator configuration rather than from the
    /// call.
    ///
    /// Returns the reference the tool registered under. The only rejection left
    /// is a remote name that no provider tool-calling API would accept; a name
    /// that merely matches a local tool or another connector's tool is qualified
    /// apart instead of failing, because one connector's naming choice must not
    /// be able to take down a router serving everyone else's tools.
    pub fn register_mcp_tool(
        &mut self,
        server_name: &str,
        credential_scope: McpServerCredentialScope,
        tool: impl Into<McpDiscoveredToolRegistration>,
    ) -> Result<String> {
        let registration = tool.into();
        let remote_name = registration.tool().name.clone();
        let name = mcp_tool_reference(server_name, &remote_name);
        if !is_model_safe_tool_name(&name) {
            return Err(moa_core::error::MoaError::ConfigError(format!(
                "MCP server {server_name} discovered tool {remote_name}, whose qualified reference \
                 {name} is not a model-safe tool name"
            )));
        }
        self.tools.insert(
            name.clone(),
            RegisteredTool::mcp(server_name, credential_scope, registration),
        );
        if !self
            .default_loadout
            .iter()
            .any(|candidate| candidate == &name)
        {
            self.default_loadout.push(name.clone());
        }
        Ok(name)
    }

    /// Removes every tool currently registered for one MCP server.
    ///
    /// Used by a catalog refresh to replace a connector's tools as a unit: a
    /// tool the server stopped exposing has to disappear, and leaving it
    /// registered would advertise a schema the server will reject.
    pub fn remove_mcp_server_tools(&mut self, server_name: &str) {
        let removed = self
            .tools
            .iter()
            .filter(|(_, tool)| match &tool.execution {
                ToolExecution::Mcp {
                    server_name: owner, ..
                } => owner == server_name,
                ToolExecution::BuiltIn(_) | ToolExecution::Hand { .. } => false,
            })
            .map(|(name, _)| name.clone())
            .collect::<HashSet<_>>();
        self.tools.retain(|name, _| !removed.contains(name));
        self.default_loadout.retain(|name| !removed.contains(name));
    }

    /// Builds the error for a tool name this registry does not know.
    ///
    /// When the name matches a name some connector *publishes*, the message says
    /// so and gives the reference the tool is actually registered under. That is
    /// the one failure mode this shape makes possible: connector tools are
    /// server-qualified, so any caller that resolved a tool through a
    /// connector's own vocabulary instead of the registry's arrives here with a
    /// name that looks correct and is not. A bare "unknown tool" sends the
    /// reader hunting for a typo that does not exist.
    #[must_use]
    pub fn unknown_tool_error(&self, name: &str) -> moa_core::error::MoaError {
        let published_by = self
            .tools
            .iter()
            .find_map(|(reference, tool)| match &tool.execution {
                ToolExecution::Mcp {
                    remote_tool_name, ..
                } if remote_tool_name == name => Some(reference.clone()),
                _ => None,
            });
        match published_by {
            Some(reference) => moa_core::error::MoaError::ToolError(format!(
                "unknown tool: {name}; a connected MCP server publishes a tool with this name,                  but connector tools are registered under their server-qualified reference —                  dispatch `{reference}` instead"
            )),
            None => moa_core::error::MoaError::ToolError(format!("unknown tool: {name}")),
        }
    }

    /// Returns the registered schema revision of one MCP tool, if it is an MCP tool.
    #[must_use]
    pub fn mcp_schema_revision(&self, name: &str) -> Option<&str> {
        match self.tools.get(name).map(|tool| &tool.execution) {
            Some(ToolExecution::Mcp { schema_hash, .. }) => Some(schema_hash.as_str()),
            _ => None,
        }
    }

    /// Returns the declared loadout order used as capability priority.
    ///
    /// Consumers that must reduce a loadout to fit a schema cap drop from the
    /// end of this list, so what is dropped is what the deployment declared
    /// least central rather than whatever sorts last by name.
    #[must_use]
    pub fn default_loadout(&self) -> &[String] {
        &self.default_loadout
    }

    /// Retargets all hand-based tools to a different provider route list.
    pub fn retarget_hand_tools(&mut self, routes: Vec<HandRoute>) {
        for tool in self.tools.values_mut() {
            if let ToolExecution::Hand {
                routes: current_routes,
            } = &mut tool.execution
            {
                *current_routes = routes.clone();
            }
        }
    }

    /// Returns a tool definition by name.
    pub fn get(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools.get(name).map(|tool| &tool.definition)
    }

    /// Returns registered definitions with their executable owners in stable name order.
    pub fn capability_registrations(&self) -> Vec<(ToolDefinition, ToolExecution)> {
        let mut registrations = self
            .tools
            .values()
            .map(|tool| (tool.definition.clone(), tool.execution.clone()))
            .collect::<Vec<_>>();
        registrations.sort_by(|left, right| left.0.name.cmp(&right.0.name));
        registrations
    }

    /// Returns whether the named tool provisions a hand/sandbox to execute.
    ///
    /// Hand-routed tools ([`ToolExecution::Hand`]) are the only tools that
    /// provision a sandbox when invoked; built-in (in-process) and MCP tools
    /// never do. This execution-routing fact is the authoritative signal used to
    /// keep sandbox/compute tools out of the sandbox-free root coordinator's
    /// tool set. Unknown tool names are treated as not requiring a sandbox.
    pub fn tool_requires_sandbox(&self, name: &str) -> bool {
        matches!(
            self.tools.get(name).map(|tool| &tool.execution),
            Some(ToolExecution::Hand { .. })
        )
    }

    /// Returns the ordered default tool schemas for prompt compilation.
    pub fn default_tool_schemas(&self) -> Vec<Value> {
        self.default_loadout
            .iter()
            .filter_map(|name| self.tools.get(name))
            .map(|tool| tool.definition.anthropic_schema())
            .collect()
    }

    /// Returns the prompt schemas for the named registered tools, in the given
    /// order, skipping any that are not registered.
    ///
    /// Used to surface gated tools (registered but excluded from the default
    /// loadout) onto a specific turn.
    pub fn tool_schemas_for<'a, I>(&self, names: I) -> Vec<Value>
    where
        I: IntoIterator<Item = &'a str>,
    {
        names
            .into_iter()
            .filter_map(|name| self.tools.get(name))
            .map(|tool| tool.definition.anthropic_schema())
            .collect()
    }

    /// Retains only the registered tools whose names are present in the allowlist.
    pub fn retain_only<I, S>(&mut self, tool_names: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let allowed = tool_names
            .into_iter()
            .map(|name| name.as_ref().to_string())
            .collect::<HashSet<_>>();
        self.tools.retain(|name, _| allowed.contains(name));
        self.default_loadout.retain(|name| allowed.contains(name));
    }

    /// Applies configured per-tool output budgets to all registered tools.
    pub fn apply_budgets(&mut self, tool_budgets: &ToolBudgetConfig) {
        for (name, registered_tool) in &mut self.tools {
            registered_tool.definition.max_output_tokens = tool_budgets.for_tool(name);
        }
    }
}

impl ToolRouter {
    /// Returns live registered definitions with their executable owners in stable name order.
    pub fn capability_registrations(&self) -> Vec<(ToolDefinition, ToolExecution)> {
        self.registry().capability_registrations()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::default_local()
    }
}

fn default_budget_for_tool(tool_name: &str) -> u32 {
    ToolBudgetConfig::default().for_tool(tool_name)
}

#[cfg(test)]
mod tests {
    use crate::tools::sandbox_descriptor::{
        default_sandbox_tool_descriptors, sandbox_tool_descriptors,
    };

    use super::ToolRegistry;

    #[test]
    fn default_local_prompt_schemas_keep_structured_hand_tool_guidance() {
        // Pins: prompt-facing hand tool descriptions carry usage policy without changing schemas.
        let registry = ToolRegistry::default_local();

        for descriptor in sandbox_tool_descriptors() {
            let name = descriptor.name;
            let description = registry
                .get(name)
                .expect("default tool should exist")
                .description
                .as_str();
            assert!(
                description.contains("Purpose:"),
                "{name}: missing Purpose guidance"
            );
            assert!(
                description.contains("Use when:"),
                "{name}: missing Use when guidance"
            );
            assert!(
                description.contains("Do not use:"),
                "{name}: missing Do not use guidance"
            );
            assert!(
                description.contains("If blocked:"),
                "{name}: missing blocked/failure guidance"
            );
        }

        let tool_names = registry
            .default_tool_schemas()
            .into_iter()
            .map(|schema| {
                schema
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .expect("schema should include name")
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            tool_names,
            vec![
                "memory_remember",
                "memory_forget",
                "memory_supersede",
                "session_search",
                "tool_result_read",
                "tool_result_search",
                "file_search",
                "grep",
                "file_outline",
                "file_read",
                "str_replace",
                "file_write",
                "bash",
            ],
            "default local loadout order changed"
        );
    }

    #[test]
    fn agentic_memory_tools_are_registered_but_excluded_from_default_loadout() {
        // Pins: memory_search/memory_navigate are executable (registered) yet
        // never appear in the default prompt loadout — the brain gates them onto
        // a turn only when the agentic strategy fires (plan Task 11).
        use crate::tools::memory::AGENTIC_MEMORY_TOOL_NAMES;

        let registry = ToolRegistry::default_local();
        let default_names = registry
            .default_tool_schemas()
            .into_iter()
            .map(|schema| {
                schema
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .expect("schema should include name")
                    .to_string()
            })
            .collect::<Vec<_>>();

        for name in AGENTIC_MEMORY_TOOL_NAMES {
            assert!(
                registry.get(name).is_some(),
                "{name} must be registered so it can execute when gated on"
            );
            assert!(
                !registry.tool_requires_sandbox(name),
                "{name} is a built-in tool and must not require a hand"
            );
            assert!(
                !default_names.contains(&name.to_string()),
                "{name} must not appear in the default prompt loadout"
            );
        }

        let gated = registry
            .tool_schemas_for(AGENTIC_MEMORY_TOOL_NAMES)
            .into_iter()
            .map(|schema| {
                schema
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .expect("schema should include name")
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(gated, vec!["memory_search", "memory_navigate"]);
    }

    #[test]
    fn tool_requires_sandbox_flags_hand_tools_only() {
        // Pins: the coordinator-exclusion predicate tracks `ToolExecution::Hand`, so every
        // sandbox descriptor tool is hand-routed while built-in tools and unknown names are not.
        let registry = ToolRegistry::default_local();

        for descriptor in sandbox_tool_descriptors() {
            assert!(
                registry.tool_requires_sandbox(descriptor.name),
                "{} is a sandbox tool and must require a hand",
                descriptor.name
            );
        }
        assert!(registry.tool_requires_sandbox("bash"));
        assert!(registry.tool_requires_sandbox("file_read"));

        for builtin in [
            "memory_remember",
            "memory_forget",
            "memory_supersede",
            "session_search",
            "tool_result_read",
            "tool_result_search",
        ] {
            assert!(
                !registry.tool_requires_sandbox(builtin),
                "{builtin} is a built-in tool and must not require a hand"
            );
        }
        // Delegation tools are injected at the orchestrator layer and are never registered
        // as hand-routed router tools, so the predicate reports them as coordinator-safe.
        assert!(!registry.tool_requires_sandbox("spawn_worker"));
        assert!(!registry.tool_requires_sandbox("nonexistent_tool"));
    }

    #[test]
    fn default_local_uses_sandbox_descriptors_as_source_of_truth() {
        // Pins: default registry metadata is generated from sandbox descriptors.
        let registry = ToolRegistry::default_local();

        for descriptor in sandbox_tool_descriptors() {
            let definition = registry
                .get(descriptor.name)
                .expect("descriptor-owned tool should be registered");
            assert_eq!(definition.name, descriptor.name);
            assert_eq!(definition.description, descriptor.description);
            assert_eq!(definition.schema, (descriptor.schema)());
            assert_eq!(definition.policy, descriptor.policy);
            assert_eq!(definition.idempotency_class, descriptor.idempotency_class);
        }

        let registered_loadout = registry
            .default_tool_schemas()
            .into_iter()
            .map(|schema| {
                schema
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .expect("schema should include name")
                    .to_string()
            })
            .skip(6)
            .collect::<Vec<_>>();
        let descriptor_loadout = default_sandbox_tool_descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(registered_loadout, descriptor_loadout);
    }
}
