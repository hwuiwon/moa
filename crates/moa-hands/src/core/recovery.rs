//! Retry and re-provision behavior for hand and MCP tool execution.

use std::time::Duration;

use moa_core::{
    error::MoaError, error::Result, error::ToolFailureClass, error::classify_tool_error,
    traits::Identity, types::completion::ToolInvocation, types::hands::HandHandle,
    types::identifiers::ToolCallId, types::security::ToolCapabilityId, types::session::SessionMeta,
    types::tools::IdempotencyClass, types::tools::SecuredToolOutput, types::tools::ToolDefinition,
    types::tools::ToolOutput,
};
use moa_observability::{record_tool_failure, record_tool_reprovision, record_tool_retry};
use tracing::Instrument;

use super::dispatch::McpDispatch;
use super::lifecycle::{hand_id, scope_key};
use super::{HandRoute, ToolCallScope, ToolExecution, ToolRouter};

const MAX_TOOL_RETRIES: u32 = 3;
const MAX_TOOL_REPROVISIONS: u32 = 2;

struct HandFailureContext<'a> {
    session: &'a SessionMeta,
    worker_id: Option<&'a str>,
    invocation: &'a ToolInvocation,
    tool_definition: &'a ToolDefinition,
    route: &'a HandRoute,
    hand: &'a HandHandle,
    active_canary: Option<&'a str>,
}

struct McpFailureContext<'a> {
    invocation: &'a ToolInvocation,
    server_name: &'a str,
    idempotency_class: IdempotencyClass,
    active_canary: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryStage {
    BeforeExecution,
    AfterUncertainExecution,
}

fn is_terminal_resource_error(error: &MoaError) -> bool {
    matches!(error, MoaError::Cancelled | MoaError::BudgetExhausted(_))
}

impl ToolRouter {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_authorized_with_recovery_inner(
        &self,
        session: &SessionMeta,
        caller_identity: &Identity,
        worker_id: Option<&str>,
        invocation: &ToolInvocation,
        tool_call_id: ToolCallId,
        active_canary: Option<&str>,
        scope: ToolCallScope<'_>,
    ) -> Result<SecuredToolOutput> {
        let registry = self.registry();
        let Some(registered_tool) = registry.tools.get(&invocation.name) else {
            // An unknown tool has no registry entry to resolve a capability from,
            // so the identity is the requested name under the built-in namespace.
            // This output is still classified: it is text the model will read.
            let class = ToolFailureClass::Fatal {
                reason: format!("unknown tool: {}", invocation.name),
            };
            return Ok(Self::secure_router_output(
                ToolCapabilityId::builtin(&invocation.name),
                active_canary,
                ToolOutput::from(class),
                None,
            ));
        };
        let capability = registered_tool.execution.capability_id(&invocation.name);

        match &registered_tool.execution {
            ToolExecution::BuiltIn(_) => {
                let result = self
                    .execute_builtin_once(
                        session,
                        caller_identity,
                        invocation,
                        &registered_tool.definition,
                        active_canary,
                        scope,
                    )
                    .await;
                Ok(match result {
                    Ok(secured) => secured,
                    Err(error) if is_terminal_resource_error(&error) => return Err(error),
                    Err(error) => Self::secure_router_output(
                        capability,
                        active_canary,
                        ToolOutput::from(classify_tool_error(&error, 0)),
                        None,
                    ),
                })
            }
            ToolExecution::Hand { routes } => {
                self.execute_hand_with_recovery(
                    session,
                    worker_id,
                    invocation,
                    &registered_tool.definition,
                    routes,
                    active_canary,
                    scope,
                )
                .await
            }
            ToolExecution::Mcp {
                server_name,
                remote_tool_name,
                ..
            } => {
                self.execute_mcp_with_recovery(
                    session,
                    invocation,
                    &registered_tool.definition,
                    McpDispatch {
                        server_name,
                        remote_tool_name,
                        tool_call_id,
                    },
                    active_canary,
                    scope,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_hand_with_recovery(
        &self,
        session: &SessionMeta,
        worker_id: Option<&str>,
        invocation: &ToolInvocation,
        tool_definition: &ToolDefinition,
        routes: &[HandRoute],
        active_canary: Option<&str>,
        scope: ToolCallScope<'_>,
    ) -> Result<SecuredToolOutput> {
        let routes = self.ordered_hand_routes(session, worker_id, routes).await?;
        let mut route_index = 0_usize;
        let mut retry_attempts = 0_u32;
        let mut reprovisions = 0_u32;
        let mut consecutive_timeouts = 0_u32;
        let mut consecutive_gateway_failures = 0_u32;

        loop {
            // Re-asked on every attempt, not once at entry: recovery retries and
            // re-provisions, so a run cancelled or expired mid-recovery must
            // stop here rather than paying for one more sandbox.
            scope.admit()?;
            let route = routes[route_index].clone();
            let provider = route.provider.as_str();
            let provider_impl = self.providers.get(provider).cloned().ok_or_else(|| {
                MoaError::ProviderError(format!("unknown hand provider: {provider}"))
            })?;
            let next_route = routes.get(route_index + 1);
            let hand = match self
                .get_or_provision_hand_within(&route, session, worker_id, scope.budget)
                .await
            {
                Ok(hand) => hand,
                Err(error) if is_terminal_resource_error(&error) => return Err(error),
                Err(error) => {
                    let class = classify_tool_error(&error, consecutive_timeouts);
                    if self
                        .try_fallback_hand_route(
                            session,
                            worker_id,
                            invocation,
                            &route,
                            next_route,
                            &class,
                            RecoveryStage::BeforeExecution,
                            tool_definition.idempotency_class,
                        )
                        .await
                    {
                        route_index += 1;
                        reset_route_recovery_counters(
                            &mut retry_attempts,
                            &mut reprovisions,
                            &mut consecutive_timeouts,
                            &mut consecutive_gateway_failures,
                        );
                        continue;
                    }
                    return Err(error);
                }
            };

            match provider_impl.health_check(&hand).await {
                Ok(true) => {}
                Ok(false) => {
                    let class = ToolFailureClass::ReProvision {
                        reason: format!("{provider} sandbox failed its health check"),
                    };
                    if self
                        .try_fallback_hand_route(
                            session,
                            worker_id,
                            invocation,
                            &route,
                            next_route,
                            &class,
                            RecoveryStage::BeforeExecution,
                            tool_definition.idempotency_class,
                        )
                        .await
                    {
                        route_index += 1;
                        reset_route_recovery_counters(
                            &mut retry_attempts,
                            &mut reprovisions,
                            &mut consecutive_timeouts,
                            &mut consecutive_gateway_failures,
                        );
                        continue;
                    }
                    if let Some(result) = self
                        .handle_hand_failure(
                            HandFailureContext {
                                session,
                                worker_id,
                                invocation,
                                tool_definition,
                                route: &route,
                                hand: &hand,
                                active_canary,
                            },
                            class,
                            RecoveryStage::BeforeExecution,
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
                    if is_terminal_resource_error(&error) {
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
                    if self
                        .try_fallback_hand_route(
                            session,
                            worker_id,
                            invocation,
                            &route,
                            next_route,
                            &class,
                            RecoveryStage::BeforeExecution,
                            tool_definition.idempotency_class,
                        )
                        .await
                    {
                        route_index += 1;
                        reset_route_recovery_counters(
                            &mut retry_attempts,
                            &mut reprovisions,
                            &mut consecutive_timeouts,
                            &mut consecutive_gateway_failures,
                        );
                        continue;
                    }
                    let retried_in_place = matches!(class, ToolFailureClass::Retryable { .. });
                    if let Some(result) = self
                        .handle_hand_failure(
                            HandFailureContext {
                                session,
                                worker_id,
                                invocation,
                                tool_definition,
                                route: &route,
                                hand: &hand,
                                active_canary,
                            },
                            class,
                            RecoveryStage::BeforeExecution,
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
                .execute_hand_on_handle(
                    session,
                    worker_id,
                    invocation,
                    tool_definition,
                    provider,
                    &hand,
                    active_canary,
                    scope,
                )
                .await
            {
                Ok(output) => {
                    self.remember_preferred_hand_route(session, worker_id, provider)
                        .await;
                    return Ok(output);
                }
                Err(error) if is_terminal_resource_error(&error) => return Err(error),
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
                    if self
                        .try_fallback_hand_route(
                            session,
                            worker_id,
                            invocation,
                            &route,
                            next_route,
                            &class,
                            RecoveryStage::AfterUncertainExecution,
                            tool_definition.idempotency_class,
                        )
                        .await
                    {
                        route_index += 1;
                        reset_route_recovery_counters(
                            &mut retry_attempts,
                            &mut reprovisions,
                            &mut consecutive_timeouts,
                            &mut consecutive_gateway_failures,
                        );
                        continue;
                    }
                    let retried_in_place = matches!(class, ToolFailureClass::Retryable { .. });
                    if let Some(result) = self
                        .handle_hand_failure(
                            HandFailureContext {
                                session,
                                worker_id,
                                invocation,
                                tool_definition,
                                route: &route,
                                hand: &hand,
                                active_canary,
                            },
                            class,
                            RecoveryStage::AfterUncertainExecution,
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

    async fn ordered_hand_routes(
        &self,
        session: &SessionMeta,
        worker_id: Option<&str>,
        routes: &[HandRoute],
    ) -> Result<Vec<HandRoute>> {
        if routes.is_empty() {
            return Err(MoaError::ConfigError(
                "hand tool has no configured provider route".to_string(),
            ));
        }
        let mut ordered = routes.to_vec();
        let scope = scope_key(session, worker_id);
        let preferred = self.preferred_hand_routes.read().await.get(&scope).cloned();
        if let Some(preferred) = preferred
            && let Some(index) = ordered.iter().position(|route| route.provider == preferred)
        {
            let route = ordered.remove(index);
            ordered.insert(0, route);
        }
        Ok(ordered)
    }

    async fn remember_preferred_hand_route(
        &self,
        session: &SessionMeta,
        worker_id: Option<&str>,
        provider: &str,
    ) {
        self.preferred_hand_routes
            .write()
            .await
            .insert(scope_key(session, worker_id), provider.to_string());
    }

    #[allow(clippy::too_many_arguments)]
    async fn try_fallback_hand_route(
        &self,
        session: &SessionMeta,
        worker_id: Option<&str>,
        invocation: &ToolInvocation,
        route: &HandRoute,
        next_route: Option<&HandRoute>,
        class: &ToolFailureClass,
        stage: RecoveryStage,
        idempotency_class: IdempotencyClass,
    ) -> bool {
        let Some(next_route) = next_route else {
            return false;
        };
        if !route_fallback_allowed(class, stage, idempotency_class) {
            return false;
        }

        record_tool_failure(&route.provider, &invocation.name, class.label());
        tracing::warn!(
            provider = %route.provider,
            fallback_provider = %next_route.provider,
            tool = %invocation.name,
            class = class.label(),
            "hand route failed; trying fallback provider"
        );

        let scope = scope_key(session, worker_id);
        let mut preferred = self.preferred_hand_routes.write().await;
        if preferred
            .get(&scope)
            .is_some_and(|provider| provider == &route.provider)
        {
            preferred.remove(&scope);
        }
        true
    }

    async fn execute_mcp_with_recovery(
        &self,
        session: &SessionMeta,
        invocation: &ToolInvocation,
        tool_definition: &ToolDefinition,
        dispatch: McpDispatch<'_>,
        active_canary: Option<&str>,
        scope: ToolCallScope<'_>,
    ) -> Result<SecuredToolOutput> {
        let mut retry_attempts = 0_u32;
        let mut reprovisions = 0_u32;
        let mut consecutive_timeouts = 0_u32;
        let mut consecutive_gateway_failures = 0_u32;
        let server_name = dispatch.server_name;

        loop {
            // Re-asked per attempt: an expired run must stop retrying an
            // outbound connector rather than working through its retry budget.
            scope.admit()?;
            // Every attempt re-resolves the tenant's credential under the same
            // durable tool-call identity, so a retry replays one credential audit
            // row rather than appending one per attempt.
            match self
                .execute_mcp_once_with_scope(
                    session,
                    invocation,
                    tool_definition,
                    dispatch,
                    active_canary,
                    scope,
                )
                .await
            {
                Ok(output) => return Ok(output),
                Err(error) if is_terminal_resource_error(&error) => return Err(error),
                Err(error) => {
                    let mut class = classify_tool_error(&error, consecutive_timeouts);
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
                            McpFailureContext {
                                invocation,
                                server_name,
                                idempotency_class: tool_definition.idempotency_class,
                                active_canary,
                            },
                            class,
                            RecoveryStage::AfterUncertainExecution,
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
        stage: RecoveryStage,
        retry_attempts: u32,
        reprovisions: u32,
    ) -> Result<Option<SecuredToolOutput>> {
        record_tool_failure(&ctx.route.provider, &ctx.invocation.name, class.label());
        tracing::warn!(
            provider = %ctx.route.provider,
            tool = %ctx.invocation.name,
            class = class.label(),
            retry_attempts,
            reprovisions,
            "tool execution failed"
        );

        let capability = ToolCapabilityId::hand(&ctx.invocation.name);
        let secured = |class: ToolFailureClass| {
            Some(Self::secure_router_output(
                capability.clone(),
                ctx.active_canary,
                ToolOutput::from(class),
                Some(hand_id(ctx.hand)),
            ))
        };

        if should_block_automatic_recovery(&class, stage, ctx.tool_definition.idempotency_class) {
            let class = idempotency_blocked_failure(class, ctx.tool_definition.idempotency_class);
            return Ok(secured(class));
        }

        match class.clone() {
            ToolFailureClass::Fatal { .. } => Ok(secured(class)),
            ToolFailureClass::Retryable { backoff_hint, .. }
                if retry_attempts + 1 < MAX_TOOL_RETRIES =>
            {
                self.retry_tool(
                    &ctx.route.provider,
                    &ctx.invocation.name,
                    retry_attempts + 1,
                    backoff_hint,
                )
                .await;
                Ok(None)
            }
            ToolFailureClass::ReProvision { .. } if reprovisions < MAX_TOOL_REPROVISIONS => {
                if let Err(error) = self
                    .reprovision_hand(ctx.session, ctx.worker_id, ctx.route)
                    .await
                {
                    return Ok(secured(classify_tool_error(&error, 0)));
                }
                self.record_reprovision(&ctx.route.provider, &ctx.invocation.name)
                    .await;
                Ok(None)
            }
            _ => Ok(secured(class)),
        }
    }

    async fn handle_mcp_failure(
        &self,
        ctx: McpFailureContext<'_>,
        class: ToolFailureClass,
        stage: RecoveryStage,
        retry_attempts: u32,
        reprovisions: u32,
    ) -> Result<Option<SecuredToolOutput>> {
        record_tool_failure(ctx.server_name, &ctx.invocation.name, class.label());
        tracing::warn!(
            provider = ctx.server_name,
            tool = %ctx.invocation.name,
            class = class.label(),
            retry_attempts,
            reprovisions,
            "MCP tool execution failed"
        );

        let capability = ToolCapabilityId::mcp(ctx.server_name, &ctx.invocation.name);
        let secured = |class: ToolFailureClass| {
            Some(Self::secure_router_output(
                capability.clone(),
                ctx.active_canary,
                ToolOutput::from(class),
                None,
            ))
        };

        if should_block_automatic_recovery(&class, stage, ctx.idempotency_class) {
            let class = idempotency_blocked_failure(class, ctx.idempotency_class);
            return Ok(secured(class));
        }

        match class.clone() {
            ToolFailureClass::Fatal { .. } => Ok(secured(class)),
            ToolFailureClass::Retryable { backoff_hint, .. }
                if retry_attempts + 1 < MAX_TOOL_RETRIES =>
            {
                self.retry_tool(
                    ctx.server_name,
                    &ctx.invocation.name,
                    retry_attempts + 1,
                    backoff_hint,
                )
                .await;
                Ok(None)
            }
            ToolFailureClass::ReProvision { .. } if reprovisions < MAX_TOOL_REPROVISIONS => {
                if let Err(error) = self.reconnect_mcp_client(ctx.server_name).await {
                    return Ok(secured(classify_tool_error(&error, 0)));
                }
                self.record_reprovision(ctx.server_name, &ctx.invocation.name)
                    .await;
                Ok(None)
            }
            _ => Ok(secured(class)),
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

    async fn record_reprovision(&self, provider: &str, tool_name: &str) {
        record_tool_reprovision(provider);
        let reprovision_span = tracing::info_span!(
            "tool_reprovision",
            provider,
            tool = %tool_name
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

fn should_block_automatic_recovery(
    class: &ToolFailureClass,
    stage: RecoveryStage,
    idempotency_class: IdempotencyClass,
) -> bool {
    if matches!(class, ToolFailureClass::Fatal { .. }) {
        return false;
    }
    if stage == RecoveryStage::BeforeExecution {
        return false;
    }
    !matches!(idempotency_class, IdempotencyClass::Idempotent)
}

fn route_fallback_allowed(
    class: &ToolFailureClass,
    stage: RecoveryStage,
    idempotency_class: IdempotencyClass,
) -> bool {
    if matches!(class, ToolFailureClass::Fatal { .. }) {
        return false;
    }
    if stage == RecoveryStage::BeforeExecution {
        return true;
    }
    matches!(idempotency_class, IdempotencyClass::Idempotent)
}

fn reset_route_recovery_counters(
    retry_attempts: &mut u32,
    reprovisions: &mut u32,
    consecutive_timeouts: &mut u32,
    consecutive_gateway_failures: &mut u32,
) {
    *retry_attempts = 0;
    *reprovisions = 0;
    *consecutive_timeouts = 0;
    *consecutive_gateway_failures = 0;
}

fn idempotency_blocked_failure(
    class: ToolFailureClass,
    idempotency_class: IdempotencyClass,
) -> ToolFailureClass {
    let reason = class.reason().to_string();
    let operation = match &class {
        ToolFailureClass::Retryable { .. } => "retry",
        ToolFailureClass::ReProvision { .. } => "re-provision",
        ToolFailureClass::Fatal { .. } => return class,
    };
    ToolFailureClass::Fatal {
        reason: format!(
            "automatic {operation} is disabled for {} tools after uncertain execution: {}",
            idempotency_class_label(idempotency_class),
            reason
        ),
    }
}

fn idempotency_class_label(idempotency_class: IdempotencyClass) -> &'static str {
    match idempotency_class {
        IdempotencyClass::Idempotent => "idempotent",
        IdempotencyClass::NonIdempotent => "non_idempotent",
    }
}

#[cfg(test)]
mod tests;
