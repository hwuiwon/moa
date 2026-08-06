use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    error::MoaError, error::Result, error::ToolFailureClass, traits::HandProvider,
    types::action_policy::ActionClass, types::action_policy::ActionPolicyEffect,
    types::action_policy::RiskLevel, types::completion::ToolInvocation, types::hands::HandHandle,
    types::hands::HandSpec, types::hands::HandStatus, types::hands::SandboxTier,
    types::identifiers::SessionId, types::identifiers::TenantId, types::identifiers::ToolCallId,
    types::session::SessionMeta, types::tools::IdempotencyClass, types::tools::ToolDiffStrategy,
    types::tools::ToolInputShape, types::tools::ToolOutput, types::tools::ToolPolicySpec,
};
use serde_json::json;

use crate::core::{HandRoute, ToolRegistry, ToolRouter};

#[derive(Default)]
struct MockProviderState {
    next_handle: u32,
    provision_calls: u32,
    destroy_calls: u32,
    execute_calls: u32,
    provision_results: VecDeque<Result<()>>,
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

/// Capabilities for the recovery mock: every tier, every dimension left to the
/// policy layers, and the durable reaper named as the deadline owner. Recovery
/// tests exercise route fallback rather than admission, so the double must be
/// admissible on the tiers the routes name.
fn mock_capabilities() -> moa_core::types::hands::HandProviderCapabilities {
    use moa_core::types::hands::{
        DeadlineEnforcement, EgressMode, HandProviderCapabilities, ResourceSupport,
        SandboxTierCapabilities,
    };

    let tier = |tier| SandboxTierCapabilities {
        tier,
        cpu: ResourceSupport::unbounded_only(),
        memory: ResourceSupport::unbounded_only(),
        ephemeral_disk: ResourceSupport::unbounded_only(),
        egress_modes: vec![
            EgressMode::DenyAll,
            EgressMode::AllowList,
            EgressMode::Unrestricted,
        ],
        idle_enforcement: DeadlineEnforcement::DurableReaper,
        max_lifetime_enforcement: DeadlineEnforcement::DurableReaper,
    };
    HandProviderCapabilities {
        revision: "mock-hands-v1".to_string(),
        tiers: vec![
            tier(SandboxTier::Local),
            tier(SandboxTier::None),
            tier(SandboxTier::Container),
            tier(SandboxTier::MicroVM),
        ],
    }
}

#[async_trait]
impl HandProvider for MockHandProvider {
    fn capabilities(&self) -> moa_core::types::hands::HandProviderCapabilities {
        mock_capabilities()
    }
    fn provider_name(&self) -> &str {
        self.name
    }

    async fn provision(&self, _spec: HandSpec) -> Result<HandHandle> {
        let mut state = self.state.lock().expect("lock mock provider state");
        state.provision_calls += 1;
        if let Some(result) = state.provision_results.pop_front() {
            result?;
        }
        state.next_handle += 1;
        Ok(HandHandle::docker(format!(
            "{}-{}",
            self.name, state.next_handle
        )))
    }

    async fn execute(&self, _handle: &HandHandle, _tool: &str, _input: &str) -> Result<ToolOutput> {
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
            .unwrap_or_else(|| moa_core::error::classify_tool_error(error, consecutive_timeouts))
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
    router_with_provider_and_idempotency(provider, IdempotencyClass::NonIdempotent).await
}

async fn router_with_provider_and_idempotency(
    provider: Arc<dyn HandProvider>,
    idempotency_class: IdempotencyClass,
) -> ToolRouter {
    let mut registry = ToolRegistry::default_local();
    registry.register_hand(
        "bash",
        "test shell command",
        json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string" }
            },
            "required": ["cmd"]
        }),
        ToolPolicySpec {
            risk_level: RiskLevel::High,
            default_effect: ActionPolicyEffect::Allow,
            action_class: ActionClass::CommandExecution,
            input_shape: ToolInputShape::Json,
            diff_strategy: ToolDiffStrategy::None,
        },
        idempotency_class,
    );
    registry.retarget_hand_tools(vec![HandRoute {
        provider: provider.provider_name().to_string(),
        tier: SandboxTier::Container,
        policy: moa_core::types::hands::SandboxPolicySnapshot::builtin(
            moa_core::types::hands::BuiltinPolicyRevision::RouteUnset,
        ),
    }]);
    registry.retain_only(["bash"]);
    let mut providers = HashMap::new();
    providers.insert(provider.provider_name().to_string(), provider);
    ToolRouter::new(
        registry,
        providers,
        crate::core::profile::local_development_sandbox_policy(),
    )
}

async fn router_with_providers_and_routes(
    providers: &[Arc<MockHandProvider>],
    routes: Vec<HandRoute>,
    idempotency_class: IdempotencyClass,
) -> ToolRouter {
    let mut registry = ToolRegistry::default_local();
    registry.register_hand(
        "bash",
        "test shell command",
        json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string" }
            },
            "required": ["cmd"]
        }),
        ToolPolicySpec {
            risk_level: RiskLevel::High,
            default_effect: ActionPolicyEffect::Allow,
            action_class: ActionClass::CommandExecution,
            input_shape: ToolInputShape::Json,
            diff_strategy: ToolDiffStrategy::None,
        },
        idempotency_class,
    );
    registry.retarget_hand_tools(routes);
    registry.retain_only(["bash"]);
    let mut provider_map = HashMap::new();
    for provider in providers {
        let provider_trait: Arc<dyn HandProvider> = provider.clone();
        provider_map.insert(provider.provider_name().to_string(), provider_trait);
    }
    ToolRouter::new(
        registry,
        provider_map,
        crate::core::profile::local_development_sandbox_policy(),
    )
}

fn session() -> SessionMeta {
    let identity = identity();
    SessionMeta {
        id: SessionId::new(),
        tenant_id: identity.tenant_id,
        ..SessionMeta::default()
    }
}

fn identity() -> moa_core::traits::Identity {
    moa_core::traits::Identity {
        identity_type: moa_core::traits::IdentityType::Operator,
        id: uuid::Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c321),
        tenant_id: TenantId::from(uuid::Uuid::from_u128(
            0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c322,
        )),
        api_key_id: None,
        acting_on_behalf_of: None,
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
    let router =
        router_with_provider_and_idempotency(provider.clone(), IdempotencyClass::Idempotent).await;

    let secured = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            worker_id: None,
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("recovery path should return a tool output");

    let _hand_id = secured.hand_id.clone();

    let output = secured.safe_output;

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
    let router =
        router_with_provider_and_idempotency(provider.clone(), IdempotencyClass::Idempotent).await;

    let secured_2 = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            worker_id: None,
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("recovery path should return a tool output");

    let _hand_id = secured_2.hand_id.clone();

    let output = secured_2.safe_output;

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
    let router =
        router_with_provider_and_idempotency(provider.clone(), IdempotencyClass::Idempotent).await;

    let secured_3 = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            worker_id: None,
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("recovery path should return a tool output");

    let _hand_id = secured_3.hand_id.clone();

    let output = secured_3.safe_output;

    assert!(output.is_error);
    assert!(output.to_text().contains("tool execution failed"));
    let snapshot = provider.snapshot();
    assert_eq!(snapshot.execute_calls, 1);
    assert_eq!(snapshot.provision_calls, 1);
    assert_eq!(snapshot.destroy_calls, 0);
}

#[tokio::test]
async fn recovery_propagates_budget_exhaustion_from_hand_execution() {
    // Pins: a terminal run-budget error is not converted into a model-visible
    // tool failure or passed through retry classification.
    let provider = Arc::new(MockHandProvider::new(
        "mock-execute-budget",
        MockProviderState {
            execute_results: VecDeque::from([Err(MoaError::BudgetExhausted(
                "run deadline exhausted during hand execution".to_string(),
            ))]),
            classifications: VecDeque::from([ToolFailureClass::Retryable {
                reason: "must not classify terminal budget exhaustion".to_string(),
                backoff_hint: Duration::ZERO,
            }]),
            ..MockProviderState::default()
        },
    ));
    let router =
        router_with_provider_and_idempotency(provider.clone(), IdempotencyClass::Idempotent).await;

    let error = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            worker_id: None,
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect_err("budget exhaustion must escape recovery unchanged");

    assert!(matches!(
        error,
        MoaError::BudgetExhausted(message)
            if message == "run deadline exhausted during hand execution"
    ));
    let snapshot = provider.snapshot();
    assert_eq!(snapshot.provision_calls, 1);
    assert_eq!(snapshot.execute_calls, 1);
    assert_eq!(snapshot.destroy_calls, 0);
}

#[tokio::test]
async fn recovery_propagates_budget_exhaustion_from_health_check() {
    // Pins: recovery does not classify a budget error raised before execution
    // as a sandbox failure and therefore never executes or reprovisions.
    let provider = Arc::new(MockHandProvider::new(
        "mock-health-budget",
        MockProviderState {
            health_checks: VecDeque::from([Err(MoaError::BudgetExhausted(
                "run deadline exhausted during health check".to_string(),
            ))]),
            classifications: VecDeque::from([ToolFailureClass::ReProvision {
                reason: "must not classify terminal budget exhaustion".to_string(),
            }]),
            ..MockProviderState::default()
        },
    ));
    let router =
        router_with_provider_and_idempotency(provider.clone(), IdempotencyClass::Idempotent).await;

    let error = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            worker_id: None,
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect_err("budget exhaustion must escape recovery unchanged");

    assert!(matches!(
        error,
        MoaError::BudgetExhausted(message)
            if message == "run deadline exhausted during health check"
    ));
    let snapshot = provider.snapshot();
    assert_eq!(snapshot.provision_calls, 1);
    assert_eq!(snapshot.execute_calls, 0);
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
    let router =
        router_with_provider_and_idempotency(provider.clone(), IdempotencyClass::Idempotent).await;

    let secured_4 = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            worker_id: None,
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("recovery path should return a tool output");

    let _hand_id = secured_4.hand_id.clone();

    let output = secured_4.safe_output;

    assert!(output.is_error);
    assert!(output.to_text().contains("tool sandbox became unavailable"));
    let snapshot = provider.snapshot();
    assert_eq!(snapshot.execute_calls, 3);
    assert_eq!(snapshot.provision_calls, 3);
    assert_eq!(snapshot.destroy_calls, 2);
}

#[tokio::test]
async fn recovery_does_not_retry_non_idempotent_execution_failure() {
    let provider = Arc::new(MockHandProvider::new(
        "mock-non-idempotent-retry",
        MockProviderState {
            execute_results: VecDeque::from([Err(MoaError::ProviderError(
                "temporary outage after command started".to_string(),
            ))]),
            classifications: VecDeque::from([ToolFailureClass::Retryable {
                reason: "temporary outage after command started".to_string(),
                backoff_hint: Duration::ZERO,
            }]),
            ..MockProviderState::default()
        },
    ));
    let router = router_with_provider(provider.clone()).await;

    let secured_5 = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            worker_id: None,
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("recovery path should return a tool output");

    let _hand_id = secured_5.hand_id.clone();

    let output = secured_5.safe_output;

    assert!(output.is_error);
    assert!(
        output
            .to_text()
            .contains("automatic retry is disabled for non_idempotent tools")
    );
    let snapshot = provider.snapshot();
    assert_eq!(snapshot.execute_calls, 1);
    assert_eq!(snapshot.provision_calls, 1);
    assert_eq!(snapshot.destroy_calls, 0);
}

#[tokio::test]
async fn recovery_does_not_reprovision_non_idempotent_execution_failure() {
    let provider = Arc::new(MockHandProvider::new(
        "mock-non-idempotent-reprovision",
        MockProviderState {
            execute_results: VecDeque::from([Err(MoaError::ProviderError(
                "sandbox died after command started".to_string(),
            ))]),
            classifications: VecDeque::from([ToolFailureClass::ReProvision {
                reason: "sandbox died after command started".to_string(),
            }]),
            ..MockProviderState::default()
        },
    ));
    let router = router_with_provider(provider.clone()).await;

    let secured_6 = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            worker_id: None,
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("recovery path should return a tool output");

    let _hand_id = secured_6.hand_id.clone();

    let output = secured_6.safe_output;

    assert!(output.is_error);
    assert!(
        output
            .to_text()
            .contains("automatic re-provision is disabled for non_idempotent tools")
    );
    let snapshot = provider.snapshot();
    assert_eq!(snapshot.execute_calls, 1);
    assert_eq!(snapshot.provision_calls, 1);
    assert_eq!(snapshot.destroy_calls, 0);
}

#[tokio::test]
async fn recovery_reprovisions_non_idempotent_before_execution() {
    let provider = Arc::new(MockHandProvider::new(
        "mock-non-idempotent-health",
        MockProviderState {
            health_checks: VecDeque::from([Ok(false), Ok(true)]),
            execute_results: VecDeque::from([Ok(ToolOutput::text(
                "ran once",
                Duration::from_millis(1),
            ))]),
            ..MockProviderState::default()
        },
    ));
    let router = router_with_provider(provider.clone()).await;

    let secured_7 = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            worker_id: None,
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("recovery path should return a tool output");

    let _hand_id = secured_7.hand_id.clone();

    let output = secured_7.safe_output;

    assert!(!output.is_error);
    assert_eq!(output.to_text(), "ran once");
    let snapshot = provider.snapshot();
    assert_eq!(snapshot.execute_calls, 1);
    assert_eq!(snapshot.provision_calls, 2);
    assert_eq!(snapshot.destroy_calls, 1);
}

#[tokio::test]
async fn recovery_falls_back_when_primary_provider_fails_before_execution() {
    let primary = Arc::new(MockHandProvider::new(
        "primary-cloud",
        MockProviderState {
            provision_results: VecDeque::from([Err(MoaError::ProviderError(
                "connection refused".to_string(),
            ))]),
            ..MockProviderState::default()
        },
    ));
    let fallback = Arc::new(MockHandProvider::new(
        "fallback-cloud",
        MockProviderState {
            execute_results: VecDeque::from([Ok(ToolOutput::text(
                "fallback ran",
                Duration::from_millis(1),
            ))]),
            ..MockProviderState::default()
        },
    ));
    let router = router_with_providers_and_routes(
        &[primary.clone(), fallback.clone()],
        vec![
            HandRoute {
                provider: primary.provider_name().to_string(),
                tier: SandboxTier::Container,
                policy: moa_core::types::hands::SandboxPolicySnapshot::builtin(
                    moa_core::types::hands::BuiltinPolicyRevision::RouteUnset,
                ),
            },
            HandRoute {
                provider: fallback.provider_name().to_string(),
                tier: SandboxTier::MicroVM,
                policy: moa_core::types::hands::SandboxPolicySnapshot::builtin(
                    moa_core::types::hands::BuiltinPolicyRevision::RouteUnset,
                ),
            },
        ],
        IdempotencyClass::NonIdempotent,
    )
    .await;

    let secured_8 = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            worker_id: None,
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("fallback route should return a tool output");

    let _hand_id = secured_8.hand_id.clone();

    let output = secured_8.safe_output;

    assert!(!output.is_error);
    assert_eq!(output.to_text(), "fallback ran");
    let primary = primary.snapshot();
    let fallback = fallback.snapshot();
    assert_eq!(primary.provision_calls, 1);
    assert_eq!(primary.execute_calls, 0);
    assert_eq!(fallback.provision_calls, 1);
    assert_eq!(fallback.execute_calls, 1);
}

#[tokio::test]
async fn recovery_falls_back_after_execution_only_for_idempotent_tools() {
    let primary = Arc::new(MockHandProvider::new(
        "primary-idempotent",
        MockProviderState {
            execute_results: VecDeque::from([Err(MoaError::ProviderError(
                "gateway temporarily unavailable".to_string(),
            ))]),
            classifications: VecDeque::from([ToolFailureClass::Retryable {
                reason: "gateway temporarily unavailable".to_string(),
                backoff_hint: Duration::ZERO,
            }]),
            ..MockProviderState::default()
        },
    ));
    let fallback = Arc::new(MockHandProvider::new(
        "fallback-idempotent",
        MockProviderState {
            execute_results: VecDeque::from([Ok(ToolOutput::text(
                "idempotent fallback",
                Duration::from_millis(1),
            ))]),
            ..MockProviderState::default()
        },
    ));
    let router = router_with_providers_and_routes(
        &[primary.clone(), fallback.clone()],
        vec![
            HandRoute {
                provider: primary.provider_name().to_string(),
                tier: SandboxTier::Container,
                policy: moa_core::types::hands::SandboxPolicySnapshot::builtin(
                    moa_core::types::hands::BuiltinPolicyRevision::RouteUnset,
                ),
            },
            HandRoute {
                provider: fallback.provider_name().to_string(),
                tier: SandboxTier::MicroVM,
                policy: moa_core::types::hands::SandboxPolicySnapshot::builtin(
                    moa_core::types::hands::BuiltinPolicyRevision::RouteUnset,
                ),
            },
        ],
        IdempotencyClass::Idempotent,
    )
    .await;

    let secured_9 = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            worker_id: None,
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("idempotent fallback route should return a tool output");

    let _hand_id = secured_9.hand_id.clone();

    let output = secured_9.safe_output;

    assert!(!output.is_error);
    assert_eq!(output.to_text(), "idempotent fallback");
    assert_eq!(primary.snapshot().execute_calls, 1);
    assert_eq!(fallback.snapshot().execute_calls, 1);
}

#[tokio::test]
async fn recovery_does_not_fallback_after_non_idempotent_execution_failure() {
    let primary = Arc::new(MockHandProvider::new(
        "primary-non-idempotent",
        MockProviderState {
            execute_results: VecDeque::from([Err(MoaError::ProviderError(
                "sandbox died after command started".to_string(),
            ))]),
            classifications: VecDeque::from([ToolFailureClass::ReProvision {
                reason: "sandbox died after command started".to_string(),
            }]),
            ..MockProviderState::default()
        },
    ));
    let fallback = Arc::new(MockHandProvider::new(
        "fallback-non-idempotent",
        MockProviderState {
            execute_results: VecDeque::from([Ok(ToolOutput::text(
                "should not run",
                Duration::from_millis(1),
            ))]),
            ..MockProviderState::default()
        },
    ));
    let router = router_with_providers_and_routes(
        &[primary.clone(), fallback.clone()],
        vec![
            HandRoute {
                provider: primary.provider_name().to_string(),
                tier: SandboxTier::Container,
                policy: moa_core::types::hands::SandboxPolicySnapshot::builtin(
                    moa_core::types::hands::BuiltinPolicyRevision::RouteUnset,
                ),
            },
            HandRoute {
                provider: fallback.provider_name().to_string(),
                tier: SandboxTier::MicroVM,
                policy: moa_core::types::hands::SandboxPolicySnapshot::builtin(
                    moa_core::types::hands::BuiltinPolicyRevision::RouteUnset,
                ),
            },
        ],
        IdempotencyClass::NonIdempotent,
    )
    .await;

    let secured_10 = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session(),
            caller_identity: &identity(),
            worker_id: None,
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("non-idempotent failure should return a tool output");

    let _hand_id = secured_10.hand_id.clone();

    let output = secured_10.safe_output;

    assert!(output.is_error);
    assert!(
        output
            .to_text()
            .contains("automatic re-provision is disabled for non_idempotent tools")
    );
    assert_eq!(primary.snapshot().execute_calls, 1);
    let fallback = fallback.snapshot();
    assert_eq!(fallback.provision_calls, 0);
    assert_eq!(fallback.execute_calls, 0);
}

#[tokio::test]
async fn recovery_prefers_successful_fallback_for_same_scope() {
    let primary = Arc::new(MockHandProvider::new(
        "primary-once",
        MockProviderState {
            provision_results: VecDeque::from([Err(MoaError::ProviderError(
                "connection refused".to_string(),
            ))]),
            ..MockProviderState::default()
        },
    ));
    let fallback = Arc::new(MockHandProvider::new(
        "fallback-sticky",
        MockProviderState {
            execute_results: VecDeque::from([Ok(ToolOutput::text(
                "first fallback",
                Duration::from_millis(1),
            ))]),
            ..MockProviderState::default()
        },
    ));
    let router = router_with_providers_and_routes(
        &[primary.clone(), fallback.clone()],
        vec![
            HandRoute {
                provider: primary.provider_name().to_string(),
                tier: SandboxTier::Container,
                policy: moa_core::types::hands::SandboxPolicySnapshot::builtin(
                    moa_core::types::hands::BuiltinPolicyRevision::RouteUnset,
                ),
            },
            HandRoute {
                provider: fallback.provider_name().to_string(),
                tier: SandboxTier::MicroVM,
                policy: moa_core::types::hands::SandboxPolicySnapshot::builtin(
                    moa_core::types::hands::BuiltinPolicyRevision::RouteUnset,
                ),
            },
        ],
        IdempotencyClass::NonIdempotent,
    )
    .await;
    let session = session();

    let secured_11 = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session,
            caller_identity: &identity(),
            worker_id: None,
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("first call should use fallback");

    let _first_hand_id = secured_11.hand_id.clone();

    let first_output = secured_11.safe_output;
    let secured_12 = router
        .execute_authorized_with_recovery(crate::core::AuthorizedToolCall {
            session: &session,
            caller_identity: &identity(),
            worker_id: None,
            invocation: &bash_invocation(),
            tool_call_id: ToolCallId::new(),
            active_canary: None,
            catalog: None,
            scope: crate::core::ToolCallScope::unbounded(),
        })
        .await
        .expect("second call should prefer the proven fallback");
    let _second_hand_id = secured_12.hand_id.clone();
    let second_output = secured_12.safe_output;

    assert_eq!(first_output.to_text(), "first fallback");
    assert_eq!(second_output.to_text(), "ok");
    assert_eq!(primary.snapshot().provision_calls, 1);
    let fallback = fallback.snapshot();
    assert_eq!(fallback.provision_calls, 1);
    assert_eq!(fallback.execute_calls, 2);
}
