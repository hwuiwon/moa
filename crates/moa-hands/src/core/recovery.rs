//! Retry and re-provision behavior for hand and MCP tool execution.

use std::time::Duration;

use moa_core::{
    HandHandle, MoaError, Result, SandboxTier, SessionMeta, ToolDefinition, ToolFailureClass,
    ToolInvocation, ToolOutput, classify_tool_error, record_tool_failure, record_tool_reprovision,
    record_tool_retry,
};
use tracing::Instrument;

use crate::adapters::mcp::MCPClient;

use super::lifecycle::hand_id;
use super::{ToolExecution, ToolRouter};

const MAX_TOOL_RETRIES: u32 = 3;
const MAX_TOOL_REPROVISIONS: u32 = 2;

struct HandFailureContext<'a> {
    session: &'a SessionMeta,
    invocation: &'a ToolInvocation,
    provider: &'a str,
    tier: &'a SandboxTier,
    hand: &'a HandHandle,
}

impl ToolRouter {
    pub(super) async fn execute_authorized_with_recovery_inner(
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
mod tests;
