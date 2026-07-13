//! Tool dispatch entry points and single-attempt execution paths.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use moa_core::{
    error::MoaError, error::Result, types::completion::ToolInvocation, types::hands::HandHandle,
    types::hands::HandStatus, types::hands::SandboxTier, types::session::SessionMeta,
    types::tools::ToolDefinition, types::tools::ToolOutput,
};
use moa_observability::current_turn_root_span;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::adapters::mcp::MCPClient;

use super::lifecycle::hand_id;
use super::telemetry::{
    record_tool_execution_result, record_tool_invocation_metadata, tool_execution_span,
};
use super::{DEFAULT_PROVIDER_NAME, HandRoute, ToolExecution, ToolRouter};

impl ToolRouter {
    /// Executes a tool invocation that has already cleared action policy.
    pub async fn execute_authorized(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<(Option<String>, ToolOutput)> {
        self.execute_authorized_with_cancel(session, invocation, None, None)
            .await
    }

    /// Executes a tool invocation that has already cleared action policy with cancellation hooks.
    pub async fn execute_authorized_with_cancel(
        &self,
        session: &SessionMeta,
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
                .execute_authorized_inner(session, invocation, cancel_token, hard_cancel_token)
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
            record_tool_invocation_metadata(
                &tool_span,
                session,
                &registered_tool.execution,
                &moa_core::types::action_policy::ActionPolicyEffect::Allow,
            );
            let result = self
                .execute_authorized_with_recovery_inner(session, worker_id, invocation)
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
            let output = client
                .call_tool(&invocation.name, invocation.input.clone(), extra_headers)
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
