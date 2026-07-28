//! Tool dispatch entry points and single-attempt execution paths.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use moa_config::McpServerConfig;
use moa_core::{
    error::MoaError, error::Result, traits::Identity, types::completion::ToolInvocation,
    types::hands::HandHandle, types::hands::HandStatus, types::identifiers::ToolCallId,
    types::security::ToolCapabilityId, types::session::SessionMeta,
    types::tools::SecuredToolOutput, types::tools::ToolDefinition, types::tools::ToolOutput,
};
use moa_observability::current_turn_root_span;
use moa_security::{MCPCredentialProxy, OutputClassification, classify_tool_output};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::adapters::mcp::MCPClient;

use super::lifecycle::hand_id;
use super::mcp_connections::{TenantMcpBindingStatus, ToolCredentialScope, tenant_resolve_context};
use super::policy::validate_tool_invocation;
use super::telemetry::{
    record_tool_execution_result, record_tool_invocation_metadata, tool_execution_span,
};
use super::{DEFAULT_PROVIDER_NAME, HandRoute, ToolExecution, ToolRouter};

/// Everything one MCP dispatch needs beyond the invocation and its definition.
///
/// `credential_scope` is derived from the registered tool, never from the call,
/// and `tool_call_id` is the durable identity a tenant credential resolution is
/// audited under. It is `Copy` so every retry of one tool call re-dispatches
/// under the same identity rather than a fresh one.
#[derive(Clone, Copy)]
pub(super) struct McpDispatch<'a> {
    /// Exact authenticated caller admitted for this invocation.
    pub(super) caller_identity: &'a Identity,
    /// Configured MCP server that owns the remote tool.
    pub(super) server_name: &'a str,
    /// Tool name as the owning server knows it.
    ///
    /// Registered tool names are server-qualified, so the qualified name would
    /// be rejected by the server. Carrying the remote name explicitly is what
    /// keeps qualification a local concern instead of something every connector
    /// has to be taught about.
    pub(super) remote_tool_name: &'a str,
    /// Credential owner this invocation must be served from.
    pub(super) credential_scope: ToolCredentialScope,
    /// Replay-stable durable tool-call identity.
    pub(super) tool_call_id: ToolCallId,
}

impl ToolRouter {
    /// Classifies one raw provider return, then budgets the *safe* output.
    ///
    /// Every raw-output source funnels through here, and the order is the whole
    /// point: classification runs first, so output budgeting, artifactization,
    /// telemetry, and every downstream persistence path only ever see bytes the
    /// detector has already redacted or destroyed. Artifactizing first would
    /// write raw malicious bytes into durable blob storage that nothing later
    /// re-reads through the classifier.
    async fn secure_and_budget(
        &self,
        session: &SessionMeta,
        tool_definition: &ToolDefinition,
        capability: ToolCapabilityId,
        active_canary: Option<&str>,
        raw: ToolOutput,
        hand_id: Option<String>,
    ) -> SecuredToolOutput {
        let secured = classify_tool_output(
            &raw,
            OutputClassification {
                capability: &capability,
                active_canary,
            },
        );
        let safe_output = self
            .apply_output_budget(session, tool_definition, secured.safe_output)
            .await;
        SecuredToolOutput {
            safe_output,
            assessment: secured.assessment,
            capability,
            hand_id,
        }
    }

    /// Classifies a router-created failure output that never reached a provider.
    ///
    /// Recovery-created error text is still output the model will read, so it is
    /// classified on the same path rather than trusted because MOA wrote it.
    pub(super) fn secure_router_output(
        capability: ToolCapabilityId,
        active_canary: Option<&str>,
        raw: ToolOutput,
        hand_id: Option<String>,
    ) -> SecuredToolOutput {
        classify_tool_output(
            &raw,
            OutputClassification {
                capability: &capability,
                active_canary,
            },
        )
        .with_hand_id(hand_id)
    }

    /// Executes a tool invocation that has already cleared action policy.
    ///
    /// `tool_call_id` is the durable tool-call identity. It is required rather
    /// than generated here because a tenant-owned MCP call resolves a credential
    /// under it, and that resolution must be replay-stable: the same logical call
    /// retried must replay one audit row, not append a new one.
    ///
    /// `active_canary` is the caller's per-turn canary; output that echoes it is
    /// a leak, which is why the canary must reach the classifier and not stop at
    /// the input screen.
    pub async fn execute_authorized(
        &self,
        session: &SessionMeta,
        caller_identity: &Identity,
        invocation: &ToolInvocation,
        tool_call_id: ToolCallId,
        active_canary: Option<&str>,
    ) -> Result<SecuredToolOutput> {
        self.execute_authorized_with_cancel(
            session,
            caller_identity,
            invocation,
            tool_call_id,
            active_canary,
            None,
            None,
        )
        .await
    }

    /// Executes a tool invocation that has already cleared action policy with cancellation hooks.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_authorized_with_cancel(
        &self,
        session: &SessionMeta,
        caller_identity: &Identity,
        invocation: &ToolInvocation,
        tool_call_id: ToolCallId,
        active_canary: Option<&str>,
        cancel_token: Option<&CancellationToken>,
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<SecuredToolOutput> {
        let tool_span = tool_execution_span(session, invocation);

        let instrument_tool_span = tool_span.clone();
        async move {
            let started_at = Instant::now();
            let prepared = self.prepare_invocation(session, invocation).await?;
            let registry = self.registry();
            let registered_tool = registry
                .tools
                .get(&invocation.name)
                .ok_or_else(|| registry.unknown_tool_error(&invocation.name))?;
            record_tool_invocation_metadata(
                &tool_span,
                session,
                &registered_tool.execution,
                &prepared.policy().effect,
            );
            let result = self
                .execute_authorized_inner(
                    session,
                    caller_identity,
                    invocation,
                    tool_call_id,
                    active_canary,
                    cancel_token,
                    hard_cancel_token,
                )
                .await;
            record_tool_execution_result(
                &tool_span,
                &invocation.name,
                started_at.elapsed(),
                &result,
            );
            result
        }
        .instrument(instrument_tool_span)
        .await
    }

    /// Executes an already-authorized tool invocation with retry and recovery enabled.
    ///
    /// `worker_id` selects the hand scope: `None` provisions the
    /// session-level (coordinator) hand used today; `Some(id)` provisions and
    /// reuses a hand owned exclusively by that worker.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_authorized_with_recovery(
        &self,
        session: &SessionMeta,
        caller_identity: &Identity,
        worker_id: Option<&str>,
        invocation: &ToolInvocation,
        tool_call_id: ToolCallId,
        active_canary: Option<&str>,
    ) -> Result<SecuredToolOutput> {
        let tool_span = tool_execution_span(session, invocation);

        let instrument_tool_span = tool_span.clone();
        async move {
            let started_at = Instant::now();
            let registry = self.registry();
            let registered_tool = registry
                .tools
                .get(&invocation.name)
                .ok_or_else(|| registry.unknown_tool_error(&invocation.name))?;
            validate_tool_invocation(&registered_tool.definition, invocation)?;
            record_tool_invocation_metadata(
                &tool_span,
                session,
                &registered_tool.execution,
                &moa_core::types::action_policy::ActionPolicyEffect::Allow,
            );
            let result = self
                .execute_authorized_with_recovery_inner(
                    session,
                    caller_identity,
                    worker_id,
                    invocation,
                    tool_call_id,
                    active_canary,
                )
                .await;
            record_tool_execution_result(
                &tool_span,
                &invocation.name,
                started_at.elapsed(),
                &result,
            );
            result
        }
        .instrument(instrument_tool_span)
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_authorized_inner(
        &self,
        session: &SessionMeta,
        caller_identity: &Identity,
        invocation: &ToolInvocation,
        tool_call_id: ToolCallId,
        active_canary: Option<&str>,
        cancel_token: Option<&CancellationToken>,
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<SecuredToolOutput> {
        let registry = self.registry();
        let registered_tool = registry
            .tools
            .get(&invocation.name)
            .ok_or_else(|| registry.unknown_tool_error(&invocation.name))?;
        let credential_scope = registered_tool.execution.credential_scope();

        match &registered_tool.execution {
            ToolExecution::BuiltIn(_) => {
                self.execute_builtin_once(
                    session,
                    caller_identity,
                    invocation,
                    &registered_tool.definition,
                    active_canary,
                    cancel_token,
                )
                .await
            }
            ToolExecution::Hand { routes } => {
                let route = primary_hand_route(routes)?;
                self.execute_hand_once(
                    session,
                    // The local (non-durable) dispatch path has no worker
                    // scope; it provisions the session-level hand.
                    None,
                    invocation,
                    &registered_tool.definition,
                    route,
                    active_canary,
                    hard_cancel_token,
                )
                .await
            }
            ToolExecution::Mcp {
                server_name,
                remote_tool_name,
                ..
            } => {
                self.execute_mcp_once(
                    session,
                    invocation,
                    &registered_tool.definition,
                    McpDispatch {
                        caller_identity,
                        server_name,
                        remote_tool_name,
                        credential_scope,
                        tool_call_id,
                    },
                    active_canary,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_builtin_once(
        &self,
        session: &SessionMeta,
        caller_identity: &Identity,
        invocation: &ToolInvocation,
        tool_definition: &ToolDefinition,
        active_canary: Option<&str>,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<SecuredToolOutput> {
        let registry = self.registry();
        let registered_tool = registry
            .tools
            .get(&invocation.name)
            .ok_or_else(|| registry.unknown_tool_error(&invocation.name))?;
        let ToolExecution::BuiltIn(tool) = &registered_tool.execution else {
            return Err(MoaError::ToolError(format!(
                "tool {} is not registered as a built-in tool",
                invocation.name
            )));
        };

        let memory_tool_executor = self.memory_tool_executor.read().await.clone();
        let memory_retrieval_executor = self.memory_retrieval_executor.read().await.clone();
        let ctx = moa_core::traits::ToolContext {
            session,
            caller_identity,
            tool_call_id: invocation.id.as_deref(),
            lineage: self.lineage.as_ref(),
            session_store: self.session_store.as_deref(),
            cancel_token,
            memory_tool_executor: memory_tool_executor.as_deref(),
            memory_retrieval_executor: memory_retrieval_executor.as_deref(),
        };
        let capability = registered_tool.execution.capability_id(&invocation.name);
        let output = tool.execute(&invocation.input, &ctx).await?;
        Ok(self
            .secure_and_budget(
                session,
                tool_definition,
                capability,
                active_canary,
                output,
                None,
            )
            .await)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_hand_once(
        &self,
        session: &SessionMeta,
        worker_id: Option<&str>,
        invocation: &ToolInvocation,
        tool_definition: &ToolDefinition,
        route: &HandRoute,
        active_canary: Option<&str>,
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<SecuredToolOutput> {
        let hand = self
            .get_or_provision_hand(route, session, worker_id)
            .await?;
        self.execute_hand_on_handle(
            session,
            worker_id,
            invocation,
            tool_definition,
            &route.provider,
            &hand,
            active_canary,
            hard_cancel_token,
        )
        .await
    }

    /// Executes a tool on an already-provisioned hand handle.
    ///
    /// The recovery path provisions and health-checks the hand once and passes it
    /// here, so it does not re-provision per attempt.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_hand_on_handle(
        &self,
        session: &SessionMeta,
        worker_id: Option<&str>,
        invocation: &ToolInvocation,
        tool_definition: &ToolDefinition,
        provider: &str,
        hand: &HandHandle,
        active_canary: Option<&str>,
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<SecuredToolOutput> {
        let provider_impl = self
            .providers
            .get(provider)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown hand provider: {provider}")))?;
        let status = provider_impl.status(hand).await?;
        if matches!(status, HandStatus::Paused) {
            provider_impl.resume(hand).await?;
        }
        self.install_trusted_files_for_hand(session, worker_id, provider, hand)
            .await?;

        let serialized_input = serde_json::to_string(&invocation.input)?;
        let output = if provider == DEFAULT_PROVIDER_NAME {
            let local_provider = self.local_provider.as_ref().ok_or_else(|| {
                MoaError::ProviderError("local provider missing from tool router".to_string())
            })?;
            local_provider
                .execute_with_cancel(hand, &invocation.name, &serialized_input, hard_cancel_token)
                .await?
        } else if let Some(hard_cancel_token) = hard_cancel_token {
            tokio::select! {
                result = provider_impl.execute(hand, &invocation.name, &serialized_input) => result?,
                _ = hard_cancel_token.cancelled() => return Err(MoaError::Cancelled),
            }
        } else {
            provider_impl
                .execute(hand, &invocation.name, &serialized_input)
                .await?
        };

        // Keyed on the logical tool, not on `provider`: a fallback from one
        // sandbox provider to another must not mint a second capability identity.
        let capability = ToolCapabilityId::hand(&invocation.name);
        Ok(self
            .secure_and_budget(
                session,
                tool_definition,
                capability,
                active_canary,
                output,
                Some(hand_id(hand)),
            )
            .await)
    }

    pub(super) async fn execute_mcp_once(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
        tool_definition: &ToolDefinition,
        dispatch: McpDispatch<'_>,
        active_canary: Option<&str>,
    ) -> Result<SecuredToolOutput> {
        const MCP_DISPATCH_METHOD: &str = "tools/call";
        let server_name = dispatch.server_name;
        let span = mcp_dispatch_span(server_name, MCP_DISPATCH_METHOD, dispatch.credential_scope);
        let record_span = span.clone();
        async move {
            let started_at = Instant::now();
            let server = self.mcp_servers.get(server_name).ok_or_else(|| {
                MoaError::ProviderError(format!("unknown MCP server: {server_name}"))
            })?;
            // The scope the tool was registered under must still be the scope its
            // server is configured with. A server whose ownership changed under a
            // live registry would otherwise serve one owner's tools with the
            // other owner's credential.
            let configured_scope = ToolCredentialScope::for_server(server.credential_scope);
            if dispatch.credential_scope != configured_scope {
                return Err(MoaError::PermissionDenied(format!(
                    "MCP server '{server_name}' is configured as {} but tool '{}' was registered \
                     as {}",
                    configured_scope.as_str(),
                    invocation.name,
                    dispatch.credential_scope.as_str()
                )));
            }
            let client = self.mcp_client(server_name).await?;
            // Data-class egress governance: before the payload leaves the trust
            // boundary, classify the serialized tool arguments against this
            // server's `allowed_data_classes` allowlist. Fails closed — a
            // disallowed class or a classification error is a permission denial
            // and the tool is never called. Constructor validation guarantees a
            // guard for every configured MCP server; keep the dispatch check
            // fail-closed as defense in depth for manually assembled routers.
            // This runs before credential resolution so a payload that may not
            // leave never causes a credential to be opened or audited.
            let guard = self.mcp_egress_guard.as_ref().ok_or_else(|| {
                MoaError::ConfigError(format!(
                    "MCP server '{}' has no required egress guard",
                    server.name
                ))
            })?;
            let outbound_payload = serde_json::to_string(&invocation.input)?;
            guard
                .check(
                    &server.name,
                    &server.allowed_data_classes,
                    &outbound_payload,
                )
                .await?;
            // Trusted host-side credential resolution: shape this server's
            // credential into request headers immediately before dispatch. No
            // proxy token is minted because nothing crosses an isolation boundary
            // here. Nothing before this point holds plaintext, and nothing after
            // it retains any.
            let extra_headers = self
                .mcp_credential_headers(session, invocation, server, &dispatch)
                .await?;
            let output = client
                .call_tool(
                    dispatch.remote_tool_name,
                    invocation.input.clone(),
                    invocation.id.as_deref(),
                    extra_headers,
                )
                .await?;
            record_span.record(
                "moa.mcp.latency_ms",
                started_at.elapsed().as_millis() as i64,
            );
            let capability = ToolCapabilityId::mcp(server_name, dispatch.remote_tool_name);
            Ok(self
                .secure_and_budget(
                    session,
                    tool_definition,
                    capability,
                    active_canary,
                    output,
                    None,
                )
                .await)
        }
        .instrument(span)
        .await
    }

    /// Resolves the outbound credential headers for one MCP dispatch.
    ///
    /// The two ownership branches are disjoint by construction: the tenant branch
    /// never reads deployment material, and no failure inside it falls through to
    /// the deployment branch. A tenant-owned call that cannot be authorized,
    /// bound, or resolved is an error, never an operator-credentialed call.
    async fn mcp_credential_headers(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
        server: &McpServerConfig,
        dispatch: &McpDispatch<'_>,
    ) -> Result<HashMap<String, String>> {
        match dispatch.credential_scope {
            // Built-in and hand-routed tools never reach MCP dispatch; a value
            // that says otherwise is a routing defect, not an unauthenticated
            // call to make.
            ToolCredentialScope::NonMcp => Err(MoaError::ConfigError(format!(
                "tool '{}' reached MCP dispatch without an MCP credential scope",
                invocation.name
            ))),
            ToolCredentialScope::DeploymentOwnedMcp => {
                let Some(credentials) = server.credentials.as_ref() else {
                    // An explicitly deployment-owned server with no configured
                    // credential is an unauthenticated endpoint by operator
                    // choice.
                    return Ok(HashMap::new());
                };
                let proxy = self.required_mcp_proxy(&server.name)?;
                proxy.deployment_headers(&session.id, &server.name, credentials)
            }
            ToolCredentialScope::TenantOwnedMcp => {
                self.tenant_owned_mcp_headers(session, server, dispatch)
                    .await
            }
        }
    }

    /// Resolves one tenant's own MCP credential through its connection binding.
    ///
    /// Order matters and is part of the contract: delegated tenant-operator
    /// authorization runs *before* the first binding read, so an unauthorized
    /// caller cannot learn whether a tenant has a connection to a server; then
    /// every component of the binding must agree exactly with the dispatch
    /// before the trusted proxy opens anything.
    async fn tenant_owned_mcp_headers(
        &self,
        session: &SessionMeta,
        server: &McpServerConfig,
        dispatch: &McpDispatch<'_>,
    ) -> Result<HashMap<String, String>> {
        let server_name = server.name.as_str();
        let owners = self.tenant_mcp.as_ref().ok_or_else(|| {
            MoaError::ConfigError(format!(
                "tenant-owned MCP server '{server_name}' has no injected credential owners"
            ))
        })?;
        let proxy = self.required_mcp_proxy(server_name)?;
        let credentials = server.credentials.as_ref().ok_or_else(|| {
            MoaError::ConfigError(format!(
                "tenant-owned MCP server '{server_name}' declares no credential header shape"
            ))
        })?;
        if dispatch.caller_identity.tenant_id != session.tenant_id {
            return Err(MoaError::PermissionDenied(
                "tool caller identity does not match the session tenant".to_string(),
            ));
        }

        owners
            .authorizer
            .require_tenant_operator(dispatch.caller_identity, session.tenant_id)
            .await?;

        let binding = owners
            .bindings
            .binding_for_server(session.tenant_id, server_name)
            .await?
            .ok_or_else(|| {
                MoaError::PermissionDenied(format!(
                    "tenant has no MCP connection binding for server '{server_name}'"
                ))
            })?;
        if binding.tenant_id != session.tenant_id || binding.server_name != server_name {
            return Err(MoaError::PermissionDenied(format!(
                "MCP connection binding does not belong to this tenant and server '{server_name}'"
            )));
        }
        if binding.status != TenantMcpBindingStatus::Active {
            return Err(MoaError::PermissionDenied(format!(
                "tenant MCP connection binding for server '{server_name}' is disabled"
            )));
        }
        // The canonical operation is the exact remote tool name this dispatch
        // sends in `tools/call`, so the allowlist governs what the tenant's
        // credential can actually be used to do. It is deliberately the remote
        // name and not `invocation.name`: the registered name is
        // server-qualified, and checking that instead would compare the
        // operator's allowlist against a string this deployment invented while
        // sending the server a different one — an allowlist that no longer
        // describes what the credential can do.
        let operation = dispatch.remote_tool_name;
        if !binding.permits(operation) {
            return Err(MoaError::PermissionDenied(format!(
                "tenant MCP connection binding for server '{server_name}' does not permit \
                 operation '{operation}'"
            )));
        }

        let ctx = tenant_resolve_context(
            &binding,
            operation,
            dispatch.tool_call_id,
            dispatch.caller_identity,
        );
        proxy
            .tenant_headers(
                &session.id,
                binding.credential_identity(),
                binding.credential_ref,
                credentials,
                &ctx,
            )
            .await
    }

    /// Returns the injected credential proxy, failing closed when absent.
    ///
    /// Configured construction always installs one; a missing proxy means a
    /// manually assembled router, which must not dispatch unauthenticated.
    fn required_mcp_proxy(&self, server_name: &str) -> Result<&Arc<MCPCredentialProxy>> {
        self.mcp_proxy.as_ref().ok_or_else(|| {
            MoaError::ConfigError(format!(
                "MCP server '{server_name}' has no injected credential proxy"
            ))
        })
    }

    /// Returns this server's transport client, connecting on first use.
    ///
    /// Connections are opened lazily rather than held from startup, so a
    /// configured connector that is never invoked never costs a socket or a
    /// handshake, and a connector whose transport died between refreshes is
    /// reconnected by the call that needs it instead of failing until the next
    /// catalog refresh.
    pub(super) async fn mcp_client(&self, server_name: &str) -> Result<Arc<MCPClient>> {
        if let Some(client) = self.mcp_clients.read().await.get(server_name).cloned() {
            return Ok(client);
        }
        let server = self
            .mcp_servers
            .get(server_name)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown MCP server: {server_name}")))?;
        let client = Arc::new(MCPClient::connect(server).await?);
        let mut clients = self.mcp_clients.write().await;
        // Another dispatch may have connected while this one was handshaking;
        // keep whichever landed first so both callers share one connection.
        Ok(Arc::clone(
            clients
                .entry(server_name.to_string())
                .or_insert_with(|| Arc::clone(&client)),
        ))
    }

    pub(super) async fn reconnect_mcp_client(&self, server_name: &str) -> Result<()> {
        let server = self
            .mcp_servers
            .get(server_name)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown MCP server: {server_name}")))?;
        let client = Arc::new(MCPClient::connect(server).await?);
        self.mcp_clients
            .write()
            .await
            .insert(server_name.to_string(), client);
        Ok(())
    }
}

/// Builds an MCP dispatch span parented to the active turn root when present.
///
/// `server`, `method`, and `credential_scope` are all configuration-bounded
/// values (the configured MCP server name, the fixed JSON-RPC method used for
/// tool calls, and one of three ownership scopes), so none can grow unbounded
/// cardinality. The scope is payload-safe metadata: it says which owner served
/// the call, never which credential or whose.
fn mcp_dispatch_span(
    server: &str,
    method: &'static str,
    credential_scope: ToolCredentialScope,
) -> tracing::Span {
    let credential_scope = credential_scope.as_str();
    match current_turn_root_span() {
        Some(parent) => tracing::info_span!(
            parent: &parent,
            "mcp_dispatch",
            moa.mcp.server = %server,
            moa.mcp.method = method,
            moa.mcp.credential_scope = credential_scope,
            moa.mcp.latency_ms = tracing::field::Empty,
        ),
        None => tracing::info_span!(
            "mcp_dispatch",
            moa.mcp.server = %server,
            moa.mcp.method = method,
            moa.mcp.credential_scope = credential_scope,
            moa.mcp.latency_ms = tracing::field::Empty,
        ),
    }
}

pub(super) fn primary_hand_route(routes: &[HandRoute]) -> Result<&HandRoute> {
    routes
        .first()
        .ok_or_else(|| MoaError::ConfigError("hand tool has no configured provider route".into()))
}

#[cfg(test)]
mod egress_dispatch_tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use moa_config::McpServerConfig;
    use moa_config::McpServerCredentialScope;
    use moa_config::McpTransportConfig;
    use moa_core::traits::{Identity, IdentityType};
    use moa_core::types::security::SensitivityClass;
    use moa_core::{
        types::action_policy::ActionClass, types::action_policy::ActionPolicyEffect,
        types::action_policy::RiskLevel, types::completion::ToolInvocation,
        types::identifiers::SessionId, types::identifiers::TenantId,
        types::identifiers::ToolCallId, types::session::SessionMeta,
        types::tools::IdempotencyClass, types::tools::ToolDefinition,
        types::tools::ToolDiffStrategy, types::tools::ToolInputShape, types::tools::ToolPolicySpec,
    };
    use moa_memory_pii::{MockClassifier, PiiResult};
    use moa_security::McpEgressGuard;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use uuid::Uuid;

    use crate::adapters::mcp::MCPClient;
    use crate::core::mcp_connections::ToolCredentialScope;
    use crate::core::{ToolRegistry, ToolRouter};

    use super::McpDispatch;

    const SERVER_NAME: &str = "external-search";

    /// Spawns a fake MCP server that answers the initialize handshake and records
    /// whether a `tools/call` request ever arrives. Returns the server URL and a
    /// flag set to `true` the moment an outbound tool call reaches the server, so
    /// a test can assert the underlying `call_tool` was (or was not) invoked.
    async fn spawn_recording_mcp_server() -> (String, Arc<AtomicBool>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake MCP server");
        let addr = listener.local_addr().expect("fake MCP server address");
        let tools_call_seen = Arc::new(AtomicBool::new(false));
        let seen = tools_call_seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = vec![0_u8; 8192];
                let bytes = match socket.read(&mut buffer).await {
                    Ok(0) | Err(_) => continue,
                    Ok(read) => read,
                };
                let request = String::from_utf8_lossy(&buffer[..bytes]).to_string();
                let method = request
                    .split_once("\r\n\r\n")
                    .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok())
                    .and_then(|value| {
                        value
                            .get("method")
                            .and_then(|method| method.as_str())
                            .map(str::to_string)
                    });
                let body = match method.as_deref() {
                    Some("initialize") => r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}"#.to_string(),
                    Some("tools/call") => {
                        seen.store(true, Ordering::SeqCst);
                        r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"pong"}]}}"#.to_string()
                    }
                    // `notifications/initialized` and anything else get an empty ack.
                    _ => "{}".to_string(),
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), tools_call_seen)
    }

    /// Builds a guard whose classifier reports every payload as `restricted`.
    fn restricted_class_guard() -> Arc<McpEgressGuard> {
        let classifier = Arc::new(MockClassifier {
            fixed: PiiResult {
                class: SensitivityClass::Restricted,
                spans: Vec::new(),
                model_version: "test-mock".to_string(),
                abstained: false,
            },
        });
        Arc::new(McpEgressGuard::new(classifier))
    }

    /// Builds a router wired to `server` with the fake client already connected,
    /// optionally carrying an egress guard.
    async fn router_with_mcp_server(
        server: McpServerConfig,
        guard: Option<Arc<McpEgressGuard>>,
    ) -> ToolRouter {
        let client = Arc::new(
            MCPClient::connect(&server)
                .await
                .expect("fake MCP server handshake should succeed"),
        );
        let mut router = ToolRouter::new(
            ToolRegistry::default_local(),
            HashMap::new(),
            crate::core::profile::local_development_sandbox_policy(),
        );
        router
            .mcp_clients
            .write()
            .await
            .insert(server.name.clone(), client);
        router.mcp_servers.insert(server.name.clone(), server);
        router.mcp_egress_guard = guard;
        router
    }

    fn session() -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::new(),
            ..SessionMeta::default()
        }
    }

    fn external_tool_definition() -> ToolDefinition {
        ToolDefinition {
            name: "external_tool".to_string(),
            description: "external MCP tool".to_string(),
            schema: json!({ "type": "object" }),
            policy: ToolPolicySpec {
                risk_level: RiskLevel::Low,
                default_effect: ActionPolicyEffect::Allow,
                action_class: ActionClass::DataExport,
                input_shape: ToolInputShape::Json,
                diff_strategy: ToolDiffStrategy::None,
            },
            idempotency_class: IdempotencyClass::NonIdempotent,
            max_output_tokens: 4096,
        }
    }

    /// The server-qualified reference the discovered tool registers under.
    fn qualified_tool_name() -> String {
        crate::core::mcp_tool_reference(SERVER_NAME, "external_tool")
    }

    fn tool_invocation() -> ToolInvocation {
        ToolInvocation {
            id: None,
            name: qualified_tool_name(),
            input: json!({ "note": "patient record" }),
        }
    }

    fn http_server(url: String, allowed: Vec<SensitivityClass>) -> McpServerConfig {
        McpServerConfig {
            required: false,
            discovery: moa_config::McpDiscoveryMode::Eager,
            name: SERVER_NAME.to_string(),
            transport: McpTransportConfig::Http,
            url: Some(url),
            credential_scope: McpServerCredentialScope::DeploymentOwned,
            credentials: None,
            trust_tool_annotations: false,
            allowed_data_classes: allowed,
        }
    }

    fn identity(tenant_id: TenantId) -> Identity {
        Identity {
            identity_type: IdentityType::Operator,
            id: Uuid::from_u128(0x0f01),
            tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        }
    }

    /// Builds a guard whose classifier reports every payload as unclassified, so
    /// egress never masks what a dispatch test is actually pinning.
    fn permissive_guard() -> Arc<McpEgressGuard> {
        let classifier = Arc::new(MockClassifier {
            fixed: PiiResult {
                class: SensitivityClass::None,
                spans: Vec::new(),
                model_version: "test-mock".to_string(),
                abstained: false,
            },
        });
        Arc::new(McpEgressGuard::new(classifier))
    }

    /// The discovered form of the external tool this module dispatches.
    fn discovered_external_tool() -> crate::adapters::mcp::McpDiscoveredTool {
        crate::adapters::mcp::McpDiscoveredTool {
            name: "external_tool".to_string(),
            description: "external MCP tool".to_string(),
            input_schema: json!({ "type": "object" }),
        }
    }

    fn deployment_dispatch(caller_identity: &Identity) -> McpDispatch<'_> {
        McpDispatch {
            caller_identity,
            server_name: SERVER_NAME,
            remote_tool_name: "external_tool",
            credential_scope: ToolCredentialScope::DeploymentOwnedMcp,
            tool_call_id: ToolCallId::new(),
        }
    }

    #[tokio::test]
    async fn mcp_egress_guard_blocks_restricted_payload_before_dispatch_offline() {
        // Pins: when the egress guard classifies the outbound arguments as
        // restricted and the destination server does not allowlist that class, the
        // dispatch fails closed with a permission denial naming the server, and the
        // underlying MCP `tools/call` is never sent.
        let (url, tools_call_seen) = spawn_recording_mcp_server().await;
        let router =
            router_with_mcp_server(http_server(url, Vec::new()), Some(restricted_class_guard()))
                .await;

        let session = session();
        let caller = identity(session.tenant_id);
        let error = router
            .execute_mcp_once(
                &session,
                &tool_invocation(),
                &external_tool_definition(),
                deployment_dispatch(&caller),
                None,
            )
            .await
            .expect_err("restricted payload to a default-allowlist server must be blocked");

        assert!(
            matches!(
                error,
                moa_core::error::MoaError::PermissionDenied(message)
                    if message.contains(SERVER_NAME) && message.contains("restricted")
            ),
            "a blocked dispatch must be a permission denial naming the server and class"
        );
        assert!(
            !tools_call_seen.load(Ordering::SeqCst),
            "a blocked egress check must not invoke the MCP tool call"
        );
    }

    #[tokio::test]
    async fn mcp_egress_guard_allows_when_server_allowlists_class_offline() {
        // Pins: the identical restricted payload dispatches normally once the
        // destination server explicitly allowlists the restricted class.
        let (url, tools_call_seen) = spawn_recording_mcp_server().await;
        let router = router_with_mcp_server(
            http_server(url, vec![SensitivityClass::Restricted]),
            Some(restricted_class_guard()),
        )
        .await;

        let session = session();
        let caller = identity(session.tenant_id);
        let secured = router
            .execute_mcp_once(
                &session,
                &tool_invocation(),
                &external_tool_definition(),
                deployment_dispatch(&caller),
                None,
            )
            .await
            .expect("an allowlisted class must dispatch to the MCP server");

        assert_eq!(secured.safe_output.to_text(), "pong");
        assert!(
            tools_call_seen.load(Ordering::SeqCst),
            "an allowed egress check must dispatch the MCP tool call"
        );
    }

    #[tokio::test]
    async fn mcp_dispatch_without_required_egress_guard_fails_closed_offline() {
        // Pins: manually assembled routers cannot bypass the required MCP egress
        // check; dispatch fails before the outbound tool call when no guard exists.
        let (url, tools_call_seen) = spawn_recording_mcp_server().await;
        let router = router_with_mcp_server(http_server(url, Vec::new()), None).await;

        let session = session();
        let caller = identity(session.tenant_id);
        let error = router
            .execute_mcp_once(
                &session,
                &tool_invocation(),
                &external_tool_definition(),
                deployment_dispatch(&caller),
                None,
            )
            .await
            .expect_err("MCP dispatch without a guard must fail closed");

        assert!(
            matches!(
                error,
                moa_core::error::MoaError::ConfigError(message)
                    if message.contains(SERVER_NAME) && message.contains("egress guard")
            ),
            "missing guard must be reported as a configuration error"
        );
        assert!(
            !tools_call_seen.load(Ordering::SeqCst),
            "missing guard must prevent the MCP tool call"
        );
    }

    #[tokio::test]
    async fn a_tool_registered_under_the_other_credential_scope_fails_before_dispatch_offline() {
        // Pins: a tool registered while its server was tenant-owned cannot be
        // dispatched against the same server once it is configured as
        // deployment-owned. The scope the tool carries and the scope the server
        // declares must still agree, so an ownership change can never serve one
        // owner's tools with the other owner's credential.
        let (url, tools_call_seen) = spawn_recording_mcp_server().await;
        let router =
            router_with_mcp_server(http_server(url, Vec::new()), Some(permissive_guard())).await;
        let mut registry = (*router.registry()).clone();
        registry
            .register_mcp_tool(
                SERVER_NAME,
                McpServerCredentialScope::TenantOwned,
                discovered_external_tool(),
            )
            .expect("register the discovered MCP tool");
        router.publish_registry(registry);

        let session = session();
        let error = router
            .execute_authorized(
                &session,
                &identity(session.tenant_id),
                &tool_invocation(),
                ToolCallId::new(),
                None,
            )
            .await
            .expect_err("a scope disagreement must fail closed");

        assert!(
            matches!(
                error,
                moa_core::error::MoaError::PermissionDenied(ref message)
                    if message.contains("configured as deployment_owned_mcp")
                        && message.contains("registered as tenant_owned_mcp")
            ),
            "a scope disagreement must be a permission denial naming both scopes, got: {error:?}"
        );
        assert!(
            !tools_call_seen.load(Ordering::SeqCst),
            "a scope disagreement must prevent the MCP tool call"
        );
    }
}
