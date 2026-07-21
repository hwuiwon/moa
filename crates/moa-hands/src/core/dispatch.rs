//! Tool dispatch entry points and single-attempt execution paths.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use moa_core::{
    error::MoaError, error::Result, traits::Identity, types::completion::ToolInvocation,
    types::hands::HandHandle, types::hands::HandStatus, types::hands::SandboxTier,
    types::session::SessionMeta, types::tools::ToolDefinition, types::tools::ToolOutput,
};
use moa_observability::current_turn_root_span;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::adapters::mcp::MCPClient;

use super::lifecycle::hand_id;
use super::policy::validate_tool_invocation;
use super::telemetry::{
    record_tool_execution_result, record_tool_invocation_metadata, tool_execution_span,
};
use super::{DEFAULT_PROVIDER_NAME, HandRoute, ToolExecution, ToolRouter};

impl ToolRouter {
    /// Executes a tool invocation that has already cleared action policy.
    pub async fn execute_authorized(
        &self,
        session: &SessionMeta,
        caller_identity: &Identity,
        invocation: &ToolInvocation,
    ) -> Result<(Option<String>, ToolOutput)> {
        self.execute_authorized_with_cancel(session, caller_identity, invocation, None, None)
            .await
    }

    /// Executes a tool invocation that has already cleared action policy with cancellation hooks.
    pub async fn execute_authorized_with_cancel(
        &self,
        session: &SessionMeta,
        caller_identity: &Identity,
        invocation: &ToolInvocation,
        cancel_token: Option<&CancellationToken>,
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<(Option<String>, ToolOutput)> {
        let tool_span = tool_execution_span(session, invocation);

        let instrument_tool_span = tool_span.clone();
        async move {
            let started_at = Instant::now();
            let prepared = self.prepare_invocation(session, invocation).await?;
            let registered_tool =
                self.registry.tools.get(&invocation.name).ok_or_else(|| {
                    MoaError::ToolError(format!("unknown tool: {}", invocation.name))
                })?;
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
    pub async fn execute_authorized_with_recovery(
        &self,
        session: &SessionMeta,
        caller_identity: &Identity,
        worker_id: Option<&str>,
        invocation: &ToolInvocation,
    ) -> Result<(Option<String>, ToolOutput)> {
        let tool_span = tool_execution_span(session, invocation);

        let instrument_tool_span = tool_span.clone();
        async move {
            let started_at = Instant::now();
            let registered_tool =
                self.registry.tools.get(&invocation.name).ok_or_else(|| {
                    MoaError::ToolError(format!("unknown tool: {}", invocation.name))
                })?;
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

    async fn execute_authorized_inner(
        &self,
        session: &SessionMeta,
        caller_identity: &Identity,
        invocation: &ToolInvocation,
        cancel_token: Option<&CancellationToken>,
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<(Option<String>, ToolOutput)> {
        let registered_tool = self
            .registry
            .tools
            .get(&invocation.name)
            .ok_or_else(|| MoaError::ToolError(format!("unknown tool: {}", invocation.name)))?;

        match &registered_tool.execution {
            ToolExecution::BuiltIn(_) => {
                self.execute_builtin_once(
                    session,
                    caller_identity,
                    invocation,
                    &registered_tool.definition,
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
                    &route.provider,
                    &route.tier,
                    hard_cancel_token,
                )
                .await
            }
            ToolExecution::Mcp { server_name } => {
                self.execute_mcp_once(
                    session,
                    invocation,
                    &registered_tool.definition,
                    server_name,
                )
                .await
            }
        }
    }

    pub(super) async fn execute_builtin_once(
        &self,
        session: &SessionMeta,
        caller_identity: &Identity,
        invocation: &ToolInvocation,
        tool_definition: &ToolDefinition,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<(Option<String>, ToolOutput)> {
        let registered_tool = self
            .registry
            .tools
            .get(&invocation.name)
            .ok_or_else(|| MoaError::ToolError(format!("unknown tool: {}", invocation.name)))?;
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
        let output = tool.execute(&invocation.input, &ctx).await?;
        Ok((
            None,
            self.apply_output_budget(session, tool_definition, output)
                .await,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_hand_once(
        &self,
        session: &SessionMeta,
        worker_id: Option<&str>,
        invocation: &ToolInvocation,
        tool_definition: &ToolDefinition,
        provider: &str,
        tier: &SandboxTier,
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<(Option<String>, ToolOutput)> {
        let hand = self
            .get_or_provision_hand(provider, tier.clone(), session, worker_id)
            .await?;
        self.execute_hand_on_handle(
            session,
            worker_id,
            invocation,
            tool_definition,
            provider,
            &hand,
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
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<(Option<String>, ToolOutput)> {
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

        Ok((
            Some(hand_id(hand)),
            self.apply_output_budget(session, tool_definition, output)
                .await,
        ))
    }

    pub(super) async fn execute_mcp_once(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
        tool_definition: &ToolDefinition,
        server_name: &str,
    ) -> Result<(Option<String>, ToolOutput)> {
        const MCP_DISPATCH_METHOD: &str = "tools/call";
        let span = mcp_dispatch_span(server_name, MCP_DISPATCH_METHOD);
        let record_span = span.clone();
        async move {
            let started_at = Instant::now();
            let server = self.mcp_servers.get(server_name).ok_or_else(|| {
                MoaError::ProviderError(format!("unknown MCP server: {server_name}"))
            })?;
            let client = self.mcp_client(server_name).await?;
            let extra_headers = if let (Some(proxy), Some(credentials)) =
                (&self.mcp_proxy, server.credentials.as_ref())
            {
                // Trusted host-side credential resolution: read this server's vault
                // credential directly and shape it into request headers. No proxy
                // token is minted because nothing crosses an isolation boundary here.
                proxy
                    .enrich_headers(&session.id, server_name, server_name, Some(credentials))
                    .await?
            } else {
                HashMap::new()
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
            let output = client
                .call_tool(
                    &invocation.name,
                    invocation.input.clone(),
                    invocation.id.as_deref(),
                    extra_headers,
                )
                .await?;
            record_span.record(
                "moa.mcp.latency_ms",
                started_at.elapsed().as_millis() as i64,
            );
            Ok((
                None,
                self.apply_output_budget(session, tool_definition, output)
                    .await,
            ))
        }
        .instrument(span)
        .await
    }

    pub(super) async fn mcp_client(&self, server_name: &str) -> Result<Arc<MCPClient>> {
        self.mcp_clients
            .read()
            .await
            .get(server_name)
            .cloned()
            .ok_or_else(|| {
                MoaError::ProviderError(format!(
                    "missing MCP client for configured server: {server_name}"
                ))
            })
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
/// `server` and `method` are both configuration-bounded values (the configured
/// MCP server name and the fixed JSON-RPC method used for tool calls), so
/// neither can grow unbounded cardinality.
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
    use moa_config::McpTransportConfig;
    use moa_core::types::security::SensitivityClass;
    use moa_core::{
        types::action_policy::ActionClass, types::action_policy::ActionPolicyEffect,
        types::action_policy::RiskLevel, types::completion::ToolInvocation,
        types::identifiers::SessionId, types::identifiers::TenantId, types::session::SessionMeta,
        types::tools::IdempotencyClass, types::tools::ToolDefinition,
        types::tools::ToolDiffStrategy, types::tools::ToolInputShape, types::tools::ToolPolicySpec,
    };
    use moa_memory_pii::{MockClassifier, PiiResult};
    use moa_security::McpEgressGuard;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::adapters::mcp::MCPClient;
    use crate::core::{ToolRegistry, ToolRouter};

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
        let mut router = ToolRouter::new(ToolRegistry::default_local(), HashMap::new());
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

    fn tool_invocation() -> ToolInvocation {
        ToolInvocation {
            id: None,
            name: "external_tool".to_string(),
            input: json!({ "note": "patient record" }),
        }
    }

    fn http_server(url: String, allowed: Vec<SensitivityClass>) -> McpServerConfig {
        McpServerConfig {
            name: SERVER_NAME.to_string(),
            transport: McpTransportConfig::Http,
            url: Some(url),
            allowed_data_classes: allowed,
            ..McpServerConfig::default()
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

        let error = router
            .execute_mcp_once(
                &session(),
                &tool_invocation(),
                &external_tool_definition(),
                SERVER_NAME,
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

        let (_, output) = router
            .execute_mcp_once(
                &session(),
                &tool_invocation(),
                &external_tool_definition(),
                SERVER_NAME,
            )
            .await
            .expect("an allowlisted class must dispatch to the MCP server");

        assert_eq!(output.to_text(), "pong");
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

        let error = router
            .execute_mcp_once(
                &session(),
                &tool_invocation(),
                &external_tool_definition(),
                SERVER_NAME,
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
