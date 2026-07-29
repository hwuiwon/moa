//! Tool dispatch entry points and single-attempt execution paths.

use std::sync::Arc;
use std::time::Instant;

use moa_core::{
    error::MoaError, error::Result, traits::Identity, types::completion::ToolInvocation,
    types::hands::HandHandle, types::hands::HandStatus, types::identifiers::ToolCallId,
    types::security::ToolCapabilityId, types::session::SessionMeta,
    types::tools::SecuredToolOutput, types::tools::ToolDefinition, types::tools::ToolOutput,
};
use moa_observability::current_turn_root_span;
use moa_security::{OutputClassification, classify_tool_output};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::adapters::mcp::MCPClient;

use super::lifecycle::hand_id;
use super::policy::validate_tool_invocation;
use super::telemetry::{
    record_tool_execution_result, record_tool_invocation_metadata, tool_execution_span,
};
use super::{DEFAULT_PROVIDER_NAME, HandRoute, ToolExecution, ToolRouter};

/// Everything one MCP dispatch needs beyond the invocation and its definition.
///
/// The durable `tool_call_id` is reused across retries and sent to the server.
#[derive(Clone, Copy)]
pub(super) struct McpDispatch<'a> {
    /// Configured MCP server that owns the remote tool.
    pub(super) server_name: &'a str,
    /// Tool name as the owning server knows it.
    ///
    /// Registered tool names are server-qualified, so the qualified name would
    /// be rejected by the server. Carrying the remote name explicitly is what
    /// keeps qualification a local concern instead of something every connector
    /// has to be taught about.
    pub(super) remote_tool_name: &'a str,
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
    /// than generated here so retry/recovery preserves one logical call identity.
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
            // The durable path deliberately does not re-run action policy: the
            // caller cleared it before enqueuing the call. Origin admission is
            // not part of that clearance — it is a property of the runtime this
            // router serves and of the session it serves it for, not of the
            // decision the caller made — so it is enforced here too. Without it,
            // an experiment trial would reach every production capability simply
            // by taking the recovery path, which is the path every orchestrated
            // tool call takes.
            moa_security::admit_capability_for_origin(
                self.effective_call_origin(session),
                &registered_tool.execution.capability_id(&invocation.name),
                registered_tool.definition.policy.action_class,
            )?;
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
                self.execute_mcp_once_with_cancel(
                    session,
                    invocation,
                    &registered_tool.definition,
                    McpDispatch {
                        server_name,
                        remote_tool_name,
                        tool_call_id,
                    },
                    active_canary,
                    hard_cancel_token.or(cancel_token),
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
        self.execute_mcp_once_with_cancel(
            session,
            invocation,
            tool_definition,
            dispatch,
            active_canary,
            None,
        )
        .await
    }

    async fn execute_mcp_once_with_cancel(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
        tool_definition: &ToolDefinition,
        dispatch: McpDispatch<'_>,
        active_canary: Option<&str>,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<SecuredToolOutput> {
        const MCP_DISPATCH_METHOD: &str = "tools/call";
        let server_name = dispatch.server_name;
        let span = mcp_dispatch_span(server_name, MCP_DISPATCH_METHOD);
        let record_span = span.clone();
        async move {
            let started_at = Instant::now();
            let server = self.mcp_servers.get(server_name).ok_or_else(|| {
                MoaError::ProviderError(format!("unknown MCP server: {server_name}"))
            })?;
            let client = if let Some(cancel_token) = cancel_token {
                tokio::select! {
                    client = self.mcp_client(server_name) => client?,
                    _ = cancel_token.cancelled() => return Err(MoaError::Cancelled),
                }
            } else {
                self.mcp_client(server_name).await?
            };
            // Data-class egress governance: before the payload leaves the trust
            // boundary, classify the serialized tool arguments against this
            // server's `allowed_data_classes` allowlist. Fails closed — a
            // disallowed class or a classification error is a permission denial
            // and the tool is never called. Constructor validation guarantees a
            // guard for every configured MCP server; keep the dispatch check
            // fail-closed as defense in depth for manually assembled routers.
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
            let tool_invocation_id = dispatch.tool_call_id.to_string();
            let output = client
                .call_tool(
                    dispatch.remote_tool_name,
                    invocation.input.clone(),
                    Some(&tool_invocation_id),
                    cancel_token,
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
        let headers = self.mcp_credentials.headers_for(server)?;
        let client = Arc::new(MCPClient::connect(server, headers).await?);
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
        let headers = self.mcp_credentials.headers_for(server)?;
        let client = Arc::new(MCPClient::connect(server, headers).await?);
        self.mcp_clients
            .write()
            .await
            .insert(server_name.to_string(), client);
        Ok(())
    }
}

/// Builds an MCP dispatch span parented to the active turn root when present.
///
/// `server` and `method` are configuration-bounded values.
fn mcp_dispatch_span(server: &str, method: &'static str) -> tracing::Span {
    match current_turn_root_span() {
        Some(parent) => tracing::info_span!(
            parent: &parent,
            "mcp_dispatch",
            moa.mcp.server = %server,
            moa.mcp.method = method,
            moa.mcp.latency_ms = tracing::field::Empty,
        ),
        None => tracing::info_span!(
            "mcp_dispatch",
            moa.mcp.server = %server,
            moa.mcp.method = method,
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
    use crate::core::{ToolRegistry, ToolRouter};

    use super::McpDispatch;

    const SERVER_NAME: &str = "external-search";
    const MCP_TOOL_INVOCATION_ID: &str = "00000000-0000-0000-0000-000000000f01";

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
                let request_json = request
                    .split_once("\r\n\r\n")
                    .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok());
                let method = request_json
                    .as_ref()
                    .and_then(|value| value.get("method"))
                    .and_then(|method| method.as_str());
                let body = match method {
                    Some("initialize") => r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}"#.to_string(),
                    Some("tools/call") => {
                        let invocation_id = request_json
                            .as_ref()
                            .and_then(|value| value.get("params"))
                            .and_then(|params| params.get("_meta"))
                            .and_then(|metadata| metadata.get("moa/toolInvocationId"))
                            .and_then(serde_json::Value::as_str);
                        if invocation_id == Some(MCP_TOOL_INVOCATION_ID) {
                            seen.store(true, Ordering::SeqCst);
                            r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"pong"}]}}"#.to_string()
                        } else {
                            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"tools/call _meta.moa/toolInvocationId must equal the durable tool-call ID"}}"#.to_string()
                        }
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
            MCPClient::connect(&server, HashMap::new())
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
            url,
            credentials: None,
            trust_tool_annotations: false,
            allowed_data_classes: allowed,
        }
    }

    fn deployment_dispatch() -> McpDispatch<'static> {
        McpDispatch {
            server_name: SERVER_NAME,
            remote_tool_name: "external_tool",
            tool_call_id: ToolCallId(Uuid::from_u128(0x0f01)),
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
        let error = router
            .execute_mcp_once(
                &session,
                &tool_invocation(),
                &external_tool_definition(),
                deployment_dispatch(),
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
        let secured = router
            .execute_mcp_once(
                &session,
                &tool_invocation(),
                &external_tool_definition(),
                deployment_dispatch(),
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
        let error = router
            .execute_mcp_once(
                &session,
                &tool_invocation(),
                &external_tool_definition(),
                deployment_dispatch(),
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
}
