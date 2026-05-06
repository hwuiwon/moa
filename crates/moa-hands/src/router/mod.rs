//! Tool routing, local hand provisioning, and built-in/MCP dispatch for MOA.

mod construction;
mod normalization;
mod policy;
mod registration;
mod telemetry;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use moa_core::{
    HandHandle, HandProvider, HandResources, HandSpec, HandStatus, McpServerConfig, MoaError,
    Result, SandboxTier, SessionMeta, SessionStore, ToolBudgetConfig, ToolContent, ToolDefinition,
    ToolFailureClass, ToolInvocation, ToolOutput, ToolOutputArtifact, ToolOutputConfig,
    WorkspaceId, classify_tool_error, record_sandbox_provision_duration, record_tool_failure,
    record_tool_reprovision, record_tool_retry, truncate_head_tail,
};
use moa_security::{ApprovalRuleStore, MCPCredentialProxy, ToolPolicies};
use serde_json::json;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::local::LocalHandProvider;
use crate::mcp::MCPClient;

pub use policy::PreparedToolInvocation;
pub use registration::{ToolExecution, ToolRegistry};
use telemetry::{
    record_tool_execution_result, record_tool_invocation_metadata, record_tool_output_truncated,
    tool_execution_span,
};

const DEFAULT_PROVIDER_NAME: &str = "local";
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_TOOL_RETRIES: u32 = 3;
const MAX_TOOL_REPROVISIONS: u32 = 2;

struct HandFailureContext<'a> {
    session: &'a SessionMeta,
    invocation: &'a ToolInvocation,
    provider: &'a str,
    tier: &'a SandboxTier,
    hand: &'a HandHandle,
}

/// Routes tool invocations to built-ins, local hands, or MCP backends.
pub struct ToolRouter {
    registry: ToolRegistry,
    providers: HashMap<String, Arc<dyn HandProvider>>,
    local_provider: Option<Arc<LocalHandProvider>>,
    mcp_clients: RwLock<HashMap<String, Arc<MCPClient>>>,
    mcp_servers: HashMap<String, McpServerConfig>,
    mcp_proxy: Option<Arc<MCPCredentialProxy>>,
    active_hands: RwLock<HashMap<String, HandHandle>>,
    workspace_roots: RwLock<HashMap<WorkspaceId, PathBuf>>,
    policies: ToolPolicies,
    rule_store: Option<Arc<dyn ApprovalRuleStore>>,
    session_store: Option<Arc<dyn SessionStore>>,
    sandbox_root: Option<PathBuf>,
    tool_output: ToolOutputConfig,
    tool_budgets: ToolBudgetConfig,
}

impl ToolRouter {
    /// Remembers the filesystem root for a logical workspace id.
    pub async fn remember_workspace_root(
        &self,
        workspace_id: WorkspaceId,
        workspace_root: PathBuf,
    ) {
        self.workspace_roots
            .write()
            .await
            .insert(workspace_id, workspace_root);
    }

    /// Returns the remembered filesystem root for a logical workspace id.
    pub async fn workspace_root(&self, workspace_id: &WorkspaceId) -> Option<PathBuf> {
        self.workspace_roots.read().await.get(workspace_id).cloned()
    }

    /// Destroys and removes all cached hands associated with the provided session.
    pub async fn destroy_session_hands(&self, session_id: &moa_core::SessionId) {
        let session_prefix = format!("{session_id}:");
        let cached = {
            let mut active_hands = self.active_hands.write().await;
            let keys = active_hands
                .keys()
                .filter(|key| key.starts_with(&session_prefix))
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| active_hands.remove(&key).map(|handle| (key, handle)))
                .collect::<Vec<_>>()
        };

        for (key, handle) in cached {
            let provider_name = key
                .strip_prefix(&session_prefix)
                .unwrap_or_default()
                .to_string();
            let handle_id = hand_id(&handle);
            let Some(provider) = self.providers.get(&provider_name) else {
                tracing::warn!(
                    session_id = %session_id,
                    provider = %provider_name,
                    hand_id = %handle_id,
                    "cached hand provider missing during cleanup"
                );
                continue;
            };

            match provider.destroy(&handle).await {
                Ok(()) => {
                    tracing::info!(
                        session_id = %session_id,
                        provider = %provider_name,
                        hand_id = %handle_id,
                        "destroyed cached session hand"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        provider = %provider_name,
                        hand_id = %handle_id,
                        error = %error,
                        "failed to destroy cached session hand"
                    );
                }
            }
        }
    }

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

    async fn execute_authorized_with_recovery_inner(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
    ) -> Result<(Option<String>, ToolOutput)> {
        let Some(registered_tool) = self.registry.tools.get(&invocation.name) else {
            let class = ToolFailureClass::Fatal {
                reason: format!("unknown tool: {}", invocation.name),
            };
            return Ok((None, ToolOutput::from(class)));
        };

        match &registered_tool.execution {
            ToolExecution::BuiltIn(_) => {
                let result = self
                    .execute_builtin_once(session, invocation, &registered_tool.definition, None)
                    .await;
                Ok(match result {
                    Ok(output) => output,
                    Err(MoaError::Cancelled) => return Err(MoaError::Cancelled),
                    Err(error) => (None, ToolOutput::from(classify_tool_error(&error, 0))),
                })
            }
            ToolExecution::Hand { provider, tier } => {
                self.execute_hand_with_recovery(
                    session,
                    invocation,
                    &registered_tool.definition,
                    provider,
                    tier,
                )
                .await
            }
            ToolExecution::Mcp { server_name } => {
                self.execute_mcp_with_recovery(
                    session,
                    invocation,
                    &registered_tool.definition,
                    server_name,
                )
                .await
            }
        }
    }

    async fn execute_builtin_once(
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

        let ctx = moa_core::ToolContext {
            session,
            session_store: self.session_store.as_deref(),
            cancel_token,
        };
        let output = tool.execute(&invocation.input, &ctx).await?;
        Ok((
            None,
            self.apply_output_budget(session, tool_definition, output)
                .await,
        ))
    }

    async fn execute_hand_once(
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

    async fn execute_mcp_once(
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

    async fn execute_hand_with_recovery(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
        tool_definition: &ToolDefinition,
        provider: &str,
        tier: &SandboxTier,
    ) -> Result<(Option<String>, ToolOutput)> {
        let provider_impl = self
            .providers
            .get(provider)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown hand provider: {provider}")))?;
        let mut retry_attempts = 0_u32;
        let mut reprovisions = 0_u32;
        let mut consecutive_timeouts = 0_u32;
        let mut consecutive_gateway_failures = 0_u32;

        loop {
            let hand = self
                .get_or_provision_hand(provider, tier.clone(), session)
                .await?;

            match provider_impl.health_check(&hand).await {
                Ok(true) => {}
                Ok(false) => {
                    let class = ToolFailureClass::ReProvision {
                        reason: format!("{provider} sandbox failed its health check"),
                    };
                    if let Some(result) = self
                        .handle_hand_failure(
                            HandFailureContext {
                                session,
                                invocation,
                                provider,
                                tier,
                                hand: &hand,
                            },
                            class,
                            retry_attempts,
                            reprovisions,
                        )
                        .await?
                    {
                        return Ok(result);
                    }
                    reprovisions += 1;
                    consecutive_timeouts = 0;
                    consecutive_gateway_failures = 0;
                    continue;
                }
                Err(error) => {
                    if matches!(error, MoaError::Cancelled) {
                        return Err(error);
                    }
                    let mut class = provider_impl
                        .classify_error(&hand, &error, consecutive_timeouts)
                        .await;
                    if matches!(class, ToolFailureClass::Retryable { .. })
                        && is_gateway_unavailable_error(&error)
                        && consecutive_gateway_failures >= 1
                    {
                        class = ToolFailureClass::ReProvision {
                            reason: class.reason().to_string(),
                        };
                    }
                    let retried_in_place = matches!(class, ToolFailureClass::Retryable { .. });
                    if let Some(result) = self
                        .handle_hand_failure(
                            HandFailureContext {
                                session,
                                invocation,
                                provider,
                                tier,
                                hand: &hand,
                            },
                            class,
                            retry_attempts,
                            reprovisions,
                        )
                        .await?
                    {
                        return Ok(result);
                    }
                    if is_timeout_error(&error) {
                        consecutive_timeouts += 1;
                    } else {
                        consecutive_timeouts = 0;
                    }
                    if is_gateway_unavailable_error(&error) {
                        consecutive_gateway_failures += 1;
                    } else {
                        consecutive_gateway_failures = 0;
                    }
                    if retried_in_place {
                        retry_attempts += 1;
                    } else {
                        reprovisions += 1;
                        consecutive_timeouts = 0;
                        consecutive_gateway_failures = 0;
                    }
                    continue;
                }
            }

            match self
                .execute_hand_once(session, invocation, tool_definition, provider, tier, None)
                .await
            {
                Ok(output) => return Ok(output),
                Err(MoaError::Cancelled) => return Err(MoaError::Cancelled),
                Err(error) => {
                    let mut class = provider_impl
                        .classify_error(&hand, &error, consecutive_timeouts)
                        .await;
                    if matches!(class, ToolFailureClass::Retryable { .. })
                        && is_gateway_unavailable_error(&error)
                        && consecutive_gateway_failures >= 1
                    {
                        class = ToolFailureClass::ReProvision {
                            reason: class.reason().to_string(),
                        };
                    }
                    let retried_in_place = matches!(class, ToolFailureClass::Retryable { .. });
                    if let Some(result) = self
                        .handle_hand_failure(
                            HandFailureContext {
                                session,
                                invocation,
                                provider,
                                tier,
                                hand: &hand,
                            },
                            class,
                            retry_attempts,
                            reprovisions,
                        )
                        .await?
                    {
                        return Ok(result);
                    }
                    if is_timeout_error(&error) {
                        consecutive_timeouts += 1;
                    } else {
                        consecutive_timeouts = 0;
                    }
                    if is_gateway_unavailable_error(&error) {
                        consecutive_gateway_failures += 1;
                    } else {
                        consecutive_gateway_failures = 0;
                    }
                    if retried_in_place {
                        retry_attempts += 1;
                    } else {
                        reprovisions += 1;
                        consecutive_timeouts = 0;
                        consecutive_gateway_failures = 0;
                    }
                }
            }
        }
    }

    async fn execute_mcp_with_recovery(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
        tool_definition: &ToolDefinition,
        server_name: &str,
    ) -> Result<(Option<String>, ToolOutput)> {
        let mut retry_attempts = 0_u32;
        let mut reprovisions = 0_u32;
        let mut consecutive_timeouts = 0_u32;
        let mut consecutive_gateway_failures = 0_u32;

        loop {
            let client = self.mcp_client(server_name).await?;
            match client.health_check().await {
                Ok(true) => {}
                Ok(false) => {
                    let class = ToolFailureClass::ReProvision {
                        reason: format!("MCP server {server_name} is disconnected"),
                    };
                    if let Some(result) = self
                        .handle_mcp_failure(
                            invocation,
                            server_name,
                            class,
                            retry_attempts,
                            reprovisions,
                        )
                        .await?
                    {
                        return Ok(result);
                    }
                    reprovisions += 1;
                    consecutive_timeouts = 0;
                    consecutive_gateway_failures = 0;
                    continue;
                }
                Err(error) => {
                    if matches!(error, MoaError::Cancelled) {
                        return Err(error);
                    }
                    let mut class = MCPClient::classify_error(&error, consecutive_timeouts);
                    if matches!(class, ToolFailureClass::Retryable { .. })
                        && is_gateway_unavailable_error(&error)
                        && consecutive_gateway_failures >= 1
                    {
                        class = ToolFailureClass::ReProvision {
                            reason: class.reason().to_string(),
                        };
                    }
                    let retried_in_place = matches!(class, ToolFailureClass::Retryable { .. });
                    if let Some(result) = self
                        .handle_mcp_failure(
                            invocation,
                            server_name,
                            class,
                            retry_attempts,
                            reprovisions,
                        )
                        .await?
                    {
                        return Ok(result);
                    }
                    if is_timeout_error(&error) {
                        consecutive_timeouts += 1;
                    } else {
                        consecutive_timeouts = 0;
                    }
                    if is_gateway_unavailable_error(&error) {
                        consecutive_gateway_failures += 1;
                    } else {
                        consecutive_gateway_failures = 0;
                    }
                    if retried_in_place {
                        retry_attempts += 1;
                    } else {
                        reprovisions += 1;
                        consecutive_timeouts = 0;
                        consecutive_gateway_failures = 0;
                    }
                    continue;
                }
            }

            match self
                .execute_mcp_once(session, invocation, tool_definition, server_name)
                .await
            {
                Ok(output) => return Ok(output),
                Err(MoaError::Cancelled) => return Err(MoaError::Cancelled),
                Err(error) => {
                    let mut class = MCPClient::classify_error(&error, consecutive_timeouts);
                    if matches!(class, ToolFailureClass::Retryable { .. })
                        && is_gateway_unavailable_error(&error)
                        && consecutive_gateway_failures >= 1
                    {
                        class = ToolFailureClass::ReProvision {
                            reason: class.reason().to_string(),
                        };
                    }
                    let retried_in_place = matches!(class, ToolFailureClass::Retryable { .. });
                    if let Some(result) = self
                        .handle_mcp_failure(
                            invocation,
                            server_name,
                            class,
                            retry_attempts,
                            reprovisions,
                        )
                        .await?
                    {
                        return Ok(result);
                    }
                    if is_timeout_error(&error) {
                        consecutive_timeouts += 1;
                    } else {
                        consecutive_timeouts = 0;
                    }
                    if is_gateway_unavailable_error(&error) {
                        consecutive_gateway_failures += 1;
                    } else {
                        consecutive_gateway_failures = 0;
                    }
                    if retried_in_place {
                        retry_attempts += 1;
                    } else {
                        reprovisions += 1;
                        consecutive_timeouts = 0;
                        consecutive_gateway_failures = 0;
                    }
                }
            }
        }
    }

    async fn handle_hand_failure(
        &self,
        ctx: HandFailureContext<'_>,
        class: ToolFailureClass,
        retry_attempts: u32,
        reprovisions: u32,
    ) -> Result<Option<(Option<String>, ToolOutput)>> {
        record_tool_failure(ctx.provider, &ctx.invocation.name, class.label());
        tracing::warn!(
            provider = ctx.provider,
            tool = %ctx.invocation.name,
            class = class.label(),
            retry_attempts,
            reprovisions,
            reason = %class.reason(),
            "tool execution failed"
        );

        match class.clone() {
            ToolFailureClass::Fatal { .. } => {
                Ok(Some((Some(hand_id(ctx.hand)), ToolOutput::from(class))))
            }
            ToolFailureClass::Retryable { backoff_hint, .. }
                if retry_attempts + 1 < MAX_TOOL_RETRIES =>
            {
                self.retry_tool(
                    ctx.provider,
                    &ctx.invocation.name,
                    retry_attempts + 1,
                    backoff_hint,
                )
                .await;
                Ok(None)
            }
            ToolFailureClass::ReProvision { .. } if reprovisions < MAX_TOOL_REPROVISIONS => {
                if let Err(error) = self
                    .reprovision_hand(ctx.session, ctx.provider, ctx.tier)
                    .await
                {
                    return Ok(Some((
                        Some(hand_id(ctx.hand)),
                        ToolOutput::from(classify_tool_error(&error, 0)),
                    )));
                }
                self.record_reprovision(ctx.provider, &ctx.invocation.name, class.reason())
                    .await;
                Ok(None)
            }
            _ => Ok(Some((Some(hand_id(ctx.hand)), ToolOutput::from(class)))),
        }
    }

    async fn handle_mcp_failure(
        &self,
        invocation: &ToolInvocation,
        server_name: &str,
        class: ToolFailureClass,
        retry_attempts: u32,
        reprovisions: u32,
    ) -> Result<Option<(Option<String>, ToolOutput)>> {
        record_tool_failure(server_name, &invocation.name, class.label());
        tracing::warn!(
            provider = server_name,
            tool = %invocation.name,
            class = class.label(),
            retry_attempts,
            reprovisions,
            reason = %class.reason(),
            "MCP tool execution failed"
        );

        match class.clone() {
            ToolFailureClass::Fatal { .. } => Ok(Some((None, ToolOutput::from(class)))),
            ToolFailureClass::Retryable { backoff_hint, .. }
                if retry_attempts + 1 < MAX_TOOL_RETRIES =>
            {
                self.retry_tool(
                    server_name,
                    &invocation.name,
                    retry_attempts + 1,
                    backoff_hint,
                )
                .await;
                Ok(None)
            }
            ToolFailureClass::ReProvision { .. } if reprovisions < MAX_TOOL_REPROVISIONS => {
                if let Err(error) = self.reconnect_mcp_client(server_name).await {
                    return Ok(Some((
                        None,
                        ToolOutput::from(classify_tool_error(&error, 0)),
                    )));
                }
                self.record_reprovision(server_name, &invocation.name, class.reason())
                    .await;
                Ok(None)
            }
            _ => Ok(Some((None, ToolOutput::from(class)))),
        }
    }

    async fn mcp_client(&self, server_name: &str) -> Result<Arc<MCPClient>> {
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

    async fn retry_tool(
        &self,
        provider: &str,
        tool_name: &str,
        attempt: u32,
        backoff_hint: Duration,
    ) {
        record_tool_retry(provider, attempt);
        let retry_span = tracing::info_span!(
            "tool_retry",
            provider,
            tool = %tool_name,
            attempt,
            backoff_ms = backoff_hint.as_millis() as u64
        );
        async move {
            tokio::time::sleep(backoff_hint).await;
        }
        .instrument(retry_span)
        .await;
    }

    async fn record_reprovision(&self, provider: &str, tool_name: &str, reason: &str) {
        record_tool_reprovision(provider);
        let reprovision_span = tracing::info_span!(
            "tool_reprovision",
            provider,
            tool = %tool_name,
            reason
        );
        async {}.instrument(reprovision_span).await;
    }

    async fn reprovision_hand(
        &self,
        session: &SessionMeta,
        provider: &str,
        tier: &SandboxTier,
    ) -> Result<HandHandle> {
        let key = session_provider_key(session, provider);
        let old_handle = self.active_hands.write().await.remove(&key);
        let provider_impl = self
            .providers
            .get(provider)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown hand provider: {provider}")))?;

        if let Some(handle) = old_handle.as_ref()
            && let Err(error) = provider_impl.destroy(handle).await
        {
            tracing::warn!(
                session_id = %session.id,
                provider,
                hand_id = %hand_id(handle),
                error = %error,
                "failed to destroy unhealthy hand before re-provisioning"
            );
        }

        let workspace_mount =
            if provider == DEFAULT_PROVIDER_NAME && matches!(tier, SandboxTier::Local) {
                self.workspace_roots
                    .read()
                    .await
                    .get(&session.workspace_id)
                    .cloned()
            } else {
                None
            };
        let started_at = Instant::now();
        let handle = provider_impl
            .provision(HandSpec {
                sandbox_tier: tier.clone(),
                image: None,
                resources: HandResources::default(),
                env: HashMap::new(),
                workspace_mount,
                idle_timeout: DEFAULT_TOOL_TIMEOUT,
                max_lifetime: DEFAULT_TOOL_TIMEOUT,
            })
            .await?;
        record_sandbox_provision_duration(provider, sandbox_tier_label(tier), started_at.elapsed());
        self.active_hands.write().await.insert(key, handle.clone());
        Ok(handle)
    }

    async fn reconnect_mcp_client(&self, server_name: &str) -> Result<()> {
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

    /// Overrides the router's replay truncation settings used for head/tail shaping.
    #[must_use]
    pub fn with_tool_output_config(mut self, tool_output: ToolOutputConfig) -> Self {
        self.tool_output = tool_output;
        self
    }

    /// Overrides the router's per-tool output budgets.
    #[must_use]
    pub fn with_tool_budgets(mut self, tool_budgets: ToolBudgetConfig) -> Self {
        self.registry.apply_budgets(&tool_budgets);
        self.tool_budgets = tool_budgets;
        self
    }

    async fn apply_output_budget(
        &self,
        session: &SessionMeta,
        tool_definition: &ToolDefinition,
        output: ToolOutput,
    ) -> ToolOutput {
        if output.is_error {
            return output.with_original_output_tokens(None);
        }

        let existing_truncated = output.truncated;
        let original_output_tokens = estimate_tokens(&output.to_text());
        if let Some(artifactized_output) = self
            .artifactize_output(session, tool_definition, &output, original_output_tokens)
            .await
        {
            record_tool_output_truncated(&tool_definition.name);
            return artifactized_output.with_truncated(true);
        }

        let (stream_budgeted_output, stream_truncated) =
            self.apply_stream_budget(tool_definition, output);

        let (mut final_output, text_truncated) = self.apply_text_budget(
            tool_definition,
            original_output_tokens,
            stream_budgeted_output,
        );
        let router_truncated = stream_truncated || text_truncated;
        let truncated = existing_truncated || router_truncated;
        if router_truncated && !text_truncated {
            let footer =
                truncation_footer(original_output_tokens, tool_definition.max_output_tokens);
            let rendered = final_output.to_text();
            let with_footer = append_footer(&rendered, &footer);
            if estimate_tokens(&with_footer) > tool_definition.max_output_tokens {
                let available_chars = tool_definition
                    .max_output_tokens
                    .saturating_mul(4)
                    .saturating_sub(footer.chars().count() as u32)
                    as usize;
                let (truncated_text, _) = truncate_head_tail(
                    &rendered,
                    available_chars.max(1),
                    self.tool_output.head_ratio,
                );
                final_output.content = vec![ToolContent::Text {
                    text: append_footer(&truncated_text, &footer),
                }];
                final_output.structured = None;
            } else {
                final_output.content = vec![ToolContent::Text { text: with_footer }];
            }
        }
        final_output.truncated = truncated;
        final_output.original_output_tokens = router_truncated.then_some(original_output_tokens);

        if router_truncated {
            record_tool_output_truncated(&tool_definition.name);
        }

        final_output
    }

    async fn artifactize_output(
        &self,
        session: &SessionMeta,
        tool_definition: &ToolDefinition,
        output: &ToolOutput,
        original_output_tokens: u32,
    ) -> Option<ToolOutput> {
        if original_output_tokens <= tool_definition.max_output_tokens {
            return None;
        }

        let session_store = self.session_store.as_ref()?;

        let rendered = output.to_text();
        let combined = match session_store
            .store_text_artifact(session.id, &rendered)
            .await
        {
            Ok(claim_check) => claim_check,
            Err(error) => {
                tracing::warn!(
                    session_id = %session.id,
                    tool_name = %tool_definition.name,
                    error = %error,
                    "failed to persist oversized tool output artifact; falling back to inline truncation"
                );
                return None;
            }
        };
        let stdout = match output.process_stdout() {
            Some(stdout) if !stdout.is_empty() => {
                match session_store.store_text_artifact(session.id, stdout).await {
                    Ok(claim_check) => Some(claim_check),
                    Err(error) => {
                        tracing::warn!(
                            session_id = %session.id,
                            tool_name = %tool_definition.name,
                            error = %error,
                            "failed to persist tool stdout artifact; continuing with combined artifact only"
                        );
                        None
                    }
                }
            }
            _ => None,
        };
        let stderr = match output.process_stderr() {
            Some(stderr) if !stderr.is_empty() => {
                match session_store.store_text_artifact(session.id, stderr).await {
                    Ok(claim_check) => Some(claim_check),
                    Err(error) => {
                        tracing::warn!(
                            session_id = %session.id,
                            tool_name = %tool_definition.name,
                            error = %error,
                            "failed to persist tool stderr artifact; continuing with combined artifact only"
                        );
                        None
                    }
                }
            }
            _ => None,
        };

        let artifact = ToolOutputArtifact {
            combined,
            estimated_tokens: original_output_tokens,
            line_count: count_lines(&rendered),
            stdout,
            stderr,
        };
        let inline_preview_tokens =
            inline_artifact_preview_budget(tool_definition.max_output_tokens);
        let preview_footer = artifact_storage_footer(&artifact);
        let preview_budget_chars = inline_preview_tokens
            .saturating_mul(4)
            .saturating_sub(preview_footer.chars().count() as u32)
            as usize;
        let (preview, _) = truncate_head_tail(
            &rendered,
            preview_budget_chars.max(1),
            self.tool_output.head_ratio,
        );
        let summary = format_artifact_summary(
            output.process_exit_code(),
            artifact.available_streams(),
            append_footer(&preview, &preview_footer),
        );

        Some(ToolOutput {
            content: vec![ToolContent::Text { text: summary }],
            is_error: false,
            structured: Some(json!({
                "artifact_available": true,
                "estimated_tokens": artifact.estimated_tokens,
                "line_count": artifact.line_count,
                "available_streams": artifact.available_streams(),
                "exit_code": output.process_exit_code(),
            })),
            duration: output.duration,
            truncated: true,
            original_output_tokens: Some(original_output_tokens),
            artifact: Some(artifact),
        })
    }

    fn apply_stream_budget(
        &self,
        tool_definition: &ToolDefinition,
        output: ToolOutput,
    ) -> (ToolOutput, bool) {
        if tool_definition.name != "bash" {
            return (output, false);
        }

        let Some(exit_code) = output.process_exit_code() else {
            return (output, false);
        };
        let stdout = output.process_stdout().unwrap_or_default();
        let stderr = output.process_stderr().unwrap_or_default();

        let stdout_budget = self.tool_budgets.bash_stdout;
        let stderr_budget = self.tool_budgets.bash_stderr;
        let (stdout, stdout_truncated) =
            truncate_text_for_budget(stdout, stdout_budget, self.tool_output.head_ratio);
        let (stderr, stderr_truncated) =
            truncate_text_for_budget(stderr, stderr_budget, self.tool_output.head_ratio);

        if !stdout_truncated && !stderr_truncated {
            return (output, false);
        }

        (
            ToolOutput::from_process(stdout, stderr, exit_code, output.duration),
            true,
        )
    }

    fn apply_text_budget(
        &self,
        tool_definition: &ToolDefinition,
        original_output_tokens: u32,
        output: ToolOutput,
    ) -> (ToolOutput, bool) {
        let rendered = output.to_text();
        let budget = tool_definition.max_output_tokens;
        if estimate_tokens(&rendered) <= budget {
            return (output, false);
        }

        let footer = truncation_footer(original_output_tokens, budget);
        let available_chars = budget
            .saturating_mul(4)
            .saturating_sub(footer.chars().count() as u32) as usize;
        let available_chars = available_chars.max(1);
        let (truncated_text, _) =
            truncate_head_tail(&rendered, available_chars, self.tool_output.head_ratio);

        (
            ToolOutput {
                content: vec![ToolContent::Text {
                    text: append_footer(&truncated_text, &footer),
                }],
                structured: None,
                ..output
            },
            true,
        )
    }

    async fn get_or_provision_hand(
        &self,
        provider: &str,
        tier: SandboxTier,
        session: &SessionMeta,
    ) -> Result<HandHandle> {
        let key = session_provider_key(session, provider);
        if let Some(handle) = self.active_hands.read().await.get(&key) {
            return Ok(handle.clone());
        }

        let provider_impl = self
            .providers
            .get(provider)
            .ok_or_else(|| MoaError::ProviderError(format!("unknown hand provider: {provider}")))?;
        let workspace_mount =
            if provider == DEFAULT_PROVIDER_NAME && matches!(tier, SandboxTier::Local) {
                self.workspace_roots
                    .read()
                    .await
                    .get(&session.workspace_id)
                    .cloned()
            } else {
                None
            };
        let tier_label = sandbox_tier_label(&tier);
        let started_at = Instant::now();
        let handle = provider_impl
            .provision(HandSpec {
                sandbox_tier: tier,
                image: None,
                resources: HandResources::default(),
                env: HashMap::new(),
                workspace_mount,
                idle_timeout: DEFAULT_TOOL_TIMEOUT,
                max_lifetime: DEFAULT_TOOL_TIMEOUT,
            })
            .await?;
        record_sandbox_provision_duration(provider, tier_label, started_at.elapsed());

        self.active_hands.write().await.insert(key, handle.clone());
        Ok(handle)
    }
}

fn estimate_tokens(text: &str) -> u32 {
    let char_count = text.chars().count() as u32;
    if char_count == 0 {
        0
    } else {
        char_count.div_ceil(4)
    }
}

fn count_lines(text: &str) -> usize {
    text.lines().count()
}

fn inline_artifact_preview_budget(tool_budget_tokens: u32) -> u32 {
    tool_budget_tokens.div_ceil(4).clamp(256, 1_024)
}

fn artifact_storage_footer(artifact: &ToolOutputArtifact) -> String {
    format!(
        "[full output stored separately: ~{} tokens, {} lines, {} bytes; use tool_result_search first to locate exact matches, then tool_result_read to inspect a narrow span or stream]",
        artifact.estimated_tokens, artifact.line_count, artifact.combined.size
    )
}

fn format_artifact_summary(
    exit_code: Option<i32>,
    available_streams: Vec<&'static str>,
    preview: String,
) -> String {
    let mut lines = Vec::new();
    if let Some(exit_code) = exit_code {
        lines.push(format!("exit_code: {exit_code}"));
    }
    lines.push(format!(
        "available_streams: {}",
        available_streams.join(", ")
    ));
    lines.push(
        "recovery_hint: use the tool_result id from this message; call tool_result_search for exact patterns, then tool_result_read for a narrow range or a specific stream".to_string(),
    );
    lines.push(preview);
    lines.join("\n")
}

fn truncate_text_for_budget(text: &str, budget_tokens: u32, head_ratio: f64) -> (String, bool) {
    if estimate_tokens(text) <= budget_tokens {
        return (text.to_string(), false);
    }

    let max_chars = budget_tokens.saturating_mul(4) as usize;
    truncate_head_tail(text, max_chars.max(1), head_ratio)
}

fn truncation_footer(original_output_tokens: u32, budget_tokens: u32) -> String {
    format!("[output truncated from ~{original_output_tokens} to ~{budget_tokens} tokens]")
}

fn append_footer(text: &str, footer: &str) -> String {
    if text.trim().is_empty() {
        footer.to_string()
    } else {
        format!("{text}\n{footer}")
    }
}

fn session_provider_key(session: &SessionMeta, provider: &str) -> String {
    format!("{}:{provider}", session.id)
}

fn sandbox_tier_label(tier: &SandboxTier) -> &'static str {
    match tier {
        SandboxTier::None => "none",
        SandboxTier::Container => "container",
        SandboxTier::MicroVM => "microvm",
        SandboxTier::Local => "local",
    }
}

fn hand_id(handle: &HandHandle) -> String {
    match handle {
        HandHandle::Local { sandbox_dir } => sandbox_dir.display().to_string(),
        HandHandle::Docker { container_id } => container_id.clone(),
        HandHandle::Daytona { workspace_id } => workspace_id.clone(),
        HandHandle::E2B { sandbox_id } => sandbox_id.clone(),
    }
}

fn is_timeout_error(error: &MoaError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("timed out")
        || message.contains("timeout")
        || message.contains("deadline_exceeded")
}

fn is_gateway_unavailable_error(error: &MoaError) -> bool {
    matches!(
        error,
        MoaError::HttpStatus {
            status: 502..=504,
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use moa_core::{
        HandHandle, HandProvider, HandSpec, HandStatus, MoaError, Result, SessionId, SessionMeta,
        ToolFailureClass, ToolInvocation, ToolOutput, UserId, WorkspaceId,
    };
    use serde_json::json;

    use super::{SandboxTier, ToolRegistry, ToolRouter};

    #[derive(Default)]
    struct MockProviderState {
        next_handle: u32,
        provision_calls: u32,
        destroy_calls: u32,
        execute_calls: u32,
        execute_results: VecDeque<Result<ToolOutput>>,
        classifications: VecDeque<ToolFailureClass>,
        health_checks: VecDeque<Result<bool>>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct MockProviderSnapshot {
        provision_calls: u32,
        destroy_calls: u32,
        execute_calls: u32,
    }

    #[derive(Clone)]
    struct MockHandProvider {
        name: &'static str,
        state: Arc<Mutex<MockProviderState>>,
    }

    impl MockHandProvider {
        fn new(name: &'static str, state: MockProviderState) -> Self {
            Self {
                name,
                state: Arc::new(Mutex::new(state)),
            }
        }

        fn snapshot(&self) -> MockProviderSnapshot {
            let state = self.state.lock().expect("lock mock provider state");
            MockProviderSnapshot {
                provision_calls: state.provision_calls,
                destroy_calls: state.destroy_calls,
                execute_calls: state.execute_calls,
            }
        }
    }

    #[async_trait]
    impl HandProvider for MockHandProvider {
        fn provider_name(&self) -> &str {
            self.name
        }

        async fn provision(&self, _spec: HandSpec) -> Result<HandHandle> {
            let mut state = self.state.lock().expect("lock mock provider state");
            state.provision_calls += 1;
            state.next_handle += 1;
            Ok(HandHandle::docker(format!(
                "{}-{}",
                self.name, state.next_handle
            )))
        }

        async fn execute(
            &self,
            _handle: &HandHandle,
            _tool: &str,
            _input: &str,
        ) -> Result<ToolOutput> {
            let mut state = self.state.lock().expect("lock mock provider state");
            state.execute_calls += 1;
            state
                .execute_results
                .pop_front()
                .unwrap_or_else(|| Ok(ToolOutput::text("ok", Duration::from_millis(1))))
        }

        async fn classify_error(
            &self,
            _handle: &HandHandle,
            error: &MoaError,
            consecutive_timeouts: u32,
        ) -> ToolFailureClass {
            let mut state = self.state.lock().expect("lock mock provider state");
            state
                .classifications
                .pop_front()
                .unwrap_or_else(|| moa_core::classify_tool_error(error, consecutive_timeouts))
        }

        async fn health_check(&self, _handle: &HandHandle) -> Result<bool> {
            let mut state = self.state.lock().expect("lock mock provider state");
            state.health_checks.pop_front().unwrap_or(Ok(true))
        }

        async fn status(&self, _handle: &HandHandle) -> Result<HandStatus> {
            Ok(HandStatus::Running)
        }

        async fn pause(&self, _handle: &HandHandle) -> Result<()> {
            Ok(())
        }

        async fn resume(&self, _handle: &HandHandle) -> Result<()> {
            Ok(())
        }

        async fn destroy(&self, _handle: &HandHandle) -> Result<()> {
            let mut state = self.state.lock().expect("lock mock provider state");
            state.destroy_calls += 1;
            Ok(())
        }
    }

    async fn router_with_provider(provider: Arc<dyn HandProvider>) -> ToolRouter {
        let mut registry = ToolRegistry::default_local();
        registry.retarget_hand_tools(provider.provider_name(), SandboxTier::Container);
        registry.retain_only(["bash"]);
        let mut providers = HashMap::new();
        providers.insert(provider.provider_name().to_string(), provider);
        ToolRouter::new(registry, providers)
    }

    fn session() -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace-router-tests"),
            user_id: UserId::new("user-router-tests"),
            ..SessionMeta::default()
        }
    }

    fn bash_invocation() -> ToolInvocation {
        ToolInvocation {
            id: None,
            name: "bash".to_string(),
            input: json!({ "cmd": "printf ok" }),
        }
    }

    #[tokio::test]
    async fn recovery_retries_retryable_failures_up_to_three_attempts() {
        let provider = Arc::new(MockHandProvider::new(
            "mock-retry",
            MockProviderState {
                execute_results: VecDeque::from([
                    Err(MoaError::ProviderError("temporary outage".to_string())),
                    Err(MoaError::ProviderError("temporary outage".to_string())),
                    Err(MoaError::ProviderError("temporary outage".to_string())),
                ]),
                classifications: VecDeque::from([
                    ToolFailureClass::Retryable {
                        reason: "temporary outage".to_string(),
                        backoff_hint: Duration::ZERO,
                    },
                    ToolFailureClass::Retryable {
                        reason: "temporary outage".to_string(),
                        backoff_hint: Duration::ZERO,
                    },
                    ToolFailureClass::Retryable {
                        reason: "temporary outage".to_string(),
                        backoff_hint: Duration::ZERO,
                    },
                ]),
                ..MockProviderState::default()
            },
        ));
        let router = router_with_provider(provider.clone()).await;

        let (_hand_id, output) = router
            .execute_authorized_with_recovery(&session(), &bash_invocation())
            .await
            .expect("recovery path should return a tool output");

        assert!(output.is_error);
        assert!(
            output
                .to_text()
                .contains("automatic retries were exhausted")
        );
        let snapshot = provider.snapshot();
        assert_eq!(snapshot.execute_calls, 3);
        assert_eq!(snapshot.provision_calls, 1);
        assert_eq!(snapshot.destroy_calls, 0);
    }

    #[tokio::test]
    async fn recovery_reprovisions_and_succeeds_after_transient_sandbox_death() {
        let provider = Arc::new(MockHandProvider::new(
            "mock-reprovision",
            MockProviderState {
                execute_results: VecDeque::from([
                    Err(MoaError::ProviderError("sandbox died".to_string())),
                    Ok(ToolOutput::text("recovered", Duration::from_millis(1))),
                ]),
                classifications: VecDeque::from([ToolFailureClass::ReProvision {
                    reason: "sandbox died".to_string(),
                }]),
                ..MockProviderState::default()
            },
        ));
        let router = router_with_provider(provider.clone()).await;

        let (_hand_id, output) = router
            .execute_authorized_with_recovery(&session(), &bash_invocation())
            .await
            .expect("recovery path should return a tool output");

        assert!(!output.is_error);
        assert_eq!(output.to_text(), "recovered");
        let snapshot = provider.snapshot();
        assert_eq!(snapshot.execute_calls, 2);
        assert_eq!(snapshot.provision_calls, 2);
        assert_eq!(snapshot.destroy_calls, 1);
    }

    #[tokio::test]
    async fn recovery_returns_fatal_failures_immediately() {
        let provider = Arc::new(MockHandProvider::new(
            "mock-fatal",
            MockProviderState {
                execute_results: VecDeque::from([Err(MoaError::ToolError(
                    "unknown tool: bad".to_string(),
                ))]),
                classifications: VecDeque::from([ToolFailureClass::Fatal {
                    reason: "unknown tool: bad".to_string(),
                }]),
                ..MockProviderState::default()
            },
        ));
        let router = router_with_provider(provider.clone()).await;

        let (_hand_id, output) = router
            .execute_authorized_with_recovery(&session(), &bash_invocation())
            .await
            .expect("recovery path should return a tool output");

        assert!(output.is_error);
        assert!(output.to_text().contains("tool execution failed"));
        let snapshot = provider.snapshot();
        assert_eq!(snapshot.execute_calls, 1);
        assert_eq!(snapshot.provision_calls, 1);
        assert_eq!(snapshot.destroy_calls, 0);
    }

    #[tokio::test]
    async fn recovery_caps_reprovision_attempts_per_session() {
        let provider = Arc::new(MockHandProvider::new(
            "mock-cap",
            MockProviderState {
                execute_results: VecDeque::from([
                    Err(MoaError::ProviderError("sandbox died".to_string())),
                    Err(MoaError::ProviderError("sandbox died again".to_string())),
                    Err(MoaError::ProviderError("sandbox died forever".to_string())),
                ]),
                classifications: VecDeque::from([
                    ToolFailureClass::ReProvision {
                        reason: "sandbox died".to_string(),
                    },
                    ToolFailureClass::ReProvision {
                        reason: "sandbox died again".to_string(),
                    },
                    ToolFailureClass::ReProvision {
                        reason: "sandbox died forever".to_string(),
                    },
                ]),
                ..MockProviderState::default()
            },
        ));
        let router = router_with_provider(provider.clone()).await;

        let (_hand_id, output) = router
            .execute_authorized_with_recovery(&session(), &bash_invocation())
            .await
            .expect("recovery path should return a tool output");

        assert!(output.is_error);
        assert!(output.to_text().contains("tool sandbox became unavailable"));
        let snapshot = provider.snapshot();
        assert_eq!(snapshot.execute_calls, 3);
        assert_eq!(snapshot.provision_calls, 3);
        assert_eq!(snapshot.destroy_calls, 2);
    }
}
