//! Tool dispatch entry points and single-attempt execution paths.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use moa_core::{
    HandStatus, MoaError, Result, SandboxTier, SessionMeta, ToolDefinition, ToolInvocation,
    ToolOutput,
};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::adapters::mcp::MCPClient;

use super::lifecycle::hand_id;
use super::telemetry::{
    record_tool_execution_result, record_tool_invocation_metadata, tool_execution_span,
};
use super::{DEFAULT_PROVIDER_NAME, ToolExecution, ToolRouter};

impl ToolRouter {
    /// Executes a single tool invocation for a session.
    pub async fn execute(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
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
                &prepared.policy().action,
            );
            let result = match &prepared.policy().action {
                moa_core::PolicyAction::Allow => {
                    self.execute_authorized_inner(session, invocation, None, None)
                        .await
                }
                moa_core::PolicyAction::Deny => {
                    tool_span.set_attribute("moa.tool.denied", true);
                    Err(MoaError::PermissionDenied(format!(
                        "tool {} denied by policy",
                        invocation.name
                    )))
                }
                moa_core::PolicyAction::RequireApproval => {
                    Err(MoaError::PermissionDenied(format!(
                        "tool {} requires approval: {}",
                        invocation.name,
                        prepared.input_summary()
                    )))
                }
            };

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

    /// Executes a tool invocation after approval has already been granted.
    pub async fn execute_authorized(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<(Option<String>, ToolOutput)> {
        self.execute_authorized_with_cancel(session, invocation, None, None)
            .await
    }

    /// Executes a tool invocation after approval has already been granted with cancellation hooks.
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
                &prepared.policy().action,
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

    /// Executes a single tool invocation with retry and sandbox recovery enabled.
    pub async fn execute_with_recovery(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
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
                &prepared.policy().action,
            );
            let result = match &prepared.policy().action {
                moa_core::PolicyAction::Allow => {
                    self.execute_authorized_with_recovery_inner(session, invocation)
                        .await
                }
                moa_core::PolicyAction::Deny => {
                    tool_span.set_attribute("moa.tool.denied", true);
                    Err(MoaError::PermissionDenied(format!(
                        "tool {} denied by policy",
                        invocation.name
                    )))
                }
                moa_core::PolicyAction::RequireApproval => {
                    Err(MoaError::PermissionDenied(format!(
                        "tool {} requires approval: {}",
                        invocation.name,
                        prepared.input_summary()
                    )))
                }
            };
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
    pub async fn execute_authorized_with_recovery(
        &self,
        session: &SessionMeta,
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
                &moa_core::PolicyAction::Allow,
            );
            let result = self
                .execute_authorized_with_recovery_inner(session, invocation)
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
            ToolExecution::Hand { provider, tier } => {
                self.execute_hand_once(
                    session,
                    invocation,
                    &registered_tool.definition,
                    provider,
                    tier,
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
        let ctx = moa_core::ToolContext {
            session,
            lineage: self.lineage.as_ref(),
            session_store: self.session_store.as_deref(),
            cancel_token,
            memory_tool_executor: memory_tool_executor.as_deref(),
        };
        let output = tool.execute(&invocation.input, &ctx).await?;
        Ok((
            None,
            self.apply_output_budget(session, tool_definition, output)
                .await,
        ))
    }

    pub(super) async fn execute_hand_once(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
        tool_definition: &ToolDefinition,
        provider: &str,
        tier: &SandboxTier,
        hard_cancel_token: Option<&CancellationToken>,
    ) -> Result<(Option<String>, ToolOutput)> {
        let hand = self
            .get_or_provision_hand(provider, tier.clone(), session)
            .await?;
        let provider_impl = self
            .providers
            .get(provider)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown hand provider: {provider}")))?;
        let status = provider_impl.status(&hand).await?;
        if matches!(status, HandStatus::Paused) {
            provider_impl.resume(&hand).await?;
        }
        self.install_trusted_files_for_hand(session, provider, &hand)
            .await?;

        let serialized_input = serde_json::to_string(&invocation.input)?;
        let output = if provider == DEFAULT_PROVIDER_NAME {
            let local_provider = self.local_provider.as_ref().ok_or_else(|| {
                MoaError::ProviderError("local provider missing from tool router".to_string())
            })?;
            local_provider
                .execute_with_cancel(
                    &hand,
                    &invocation.name,
                    &serialized_input,
                    hard_cancel_token,
                )
                .await?
        } else if let Some(hard_cancel_token) = hard_cancel_token {
            tokio::select! {
                result = provider_impl.execute(&hand, &invocation.name, &serialized_input) => result?,
                _ = hard_cancel_token.cancelled() => return Err(MoaError::Cancelled),
            }
        } else {
            provider_impl
                .execute(&hand, &invocation.name, &serialized_input)
                .await?
        };

        Ok((
            Some(hand_id(&hand)),
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
        let server = self
            .mcp_servers
            .get(server_name)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown MCP server: {server_name}")))?;
        let client = self.mcp_client(server_name).await?;
        let extra_headers = if let (Some(proxy), Some(_credentials)) =
            (&self.mcp_proxy, server.credentials.as_ref())
        {
            let token = proxy
                .create_session_token(&session.id, server_name, server_name)
                .await?;
            let headers = proxy
                .enrich_headers(&token, server.credentials.as_ref())
                .await?;
            proxy.revoke_session_token(&token).await;
            headers
        } else {
            HashMap::new()
        };
        let output = client
            .call_tool(&invocation.name, invocation.input.clone(), extra_headers)
            .await?;
        Ok((
            None,
            self.apply_output_budget(session, tool_definition, output)
                .await,
        ))
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
