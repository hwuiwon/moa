use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    error::MoaError, error::Result, error::ToolFailureClass, traits::HandProvider,
    types::action_policy::ActionClass, types::action_policy::ActionPolicyEffect,
    types::action_policy::RiskLevel, types::completion::ToolInvocation, types::hands::HandHandle,
    types::hands::HandSpec, types::hands::HandStatus, types::hands::SandboxTier,
    types::identifiers::SessionId, types::identifiers::TenantId, types::session::SessionMeta,
    types::tools::IdempotencyClass, types::tools::ToolDiffStrategy, types::tools::ToolInputShape,
    types::tools::ToolOutput, types::tools::ToolPolicySpec,
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

#[async_trait]
impl HandProvider for MockHandProvider {
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
    }]);
    registry.retain_only(["bash"]);
    let mut providers = HashMap::new();
    providers.insert(provider.provider_name().to_string(), provider);
    ToolRouter::new(registry, providers)
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
    ToolRouter::new(registry, provider_map)
}

fn session() -> SessionMeta {
    SessionMeta {
        id: SessionId::new(),
        tenant_id: TenantId::new(),
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
    let router =
        router_with_provider_and_idempotency(provider.clone(), IdempotencyClass::Idempotent).await;

    let (_hand_id, output) = router
        .execute_authorized_with_recovery(&session(), None, &bash_invocation())
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
    let router =
        router_with_provider_and_idempotency(provider.clone(), IdempotencyClass::Idempotent).await;

    let (_hand_id, output) = router
        .execute_authorized_with_recovery(&session(), None, &bash_invocation())
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
    let router =
        router_with_provider_and_idempotency(provider.clone(), IdempotencyClass::Idempotent).await;

    let (_hand_id, output) = router
        .execute_authorized_with_recovery(&session(), None, &bash_invocation())
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
    let router =
        router_with_provider_and_idempotency(provider.clone(), IdempotencyClass::Idempotent).await;

    let (_hand_id, output) = router
        .execute_authorized_with_recovery(&session(), None, &bash_invocation())
        .await
        .expect("recovery path should return a tool output");

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

    let (_hand_id, output) = router
        .execute_authorized_with_recovery(&session(), None, &bash_invocation())
        .await
        .expect("recovery path should return a tool output");

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

    let (_hand_id, output) = router
        .execute_authorized_with_recovery(&session(), None, &bash_invocation())
        .await
        .expect("recovery path should return a tool output");

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

    let (_hand_id, output) = router
        .execute_authorized_with_recovery(&session(), None, &bash_invocation())
        .await
        .expect("recovery path should return a tool output");

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
            },
            HandRoute {
                provider: fallback.provider_name().to_string(),
                tier: SandboxTier::MicroVM,
            },
        ],
        IdempotencyClass::NonIdempotent,
    )
    .await;

    let (_hand_id, output) = router
        .execute_authorized_with_recovery(&session(), None, &bash_invocation())
        .await
        .expect("fallback route should return a tool output");

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
            },
            HandRoute {
                provider: fallback.provider_name().to_string(),
                tier: SandboxTier::MicroVM,
            },
        ],
        IdempotencyClass::Idempotent,
    )
    .await;

    let (_hand_id, output) = router
        .execute_authorized_with_recovery(&session(), None, &bash_invocation())
        .await
        .expect("idempotent fallback route should return a tool output");

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
            },
            HandRoute {
                provider: fallback.provider_name().to_string(),
                tier: SandboxTier::MicroVM,
            },
        ],
        IdempotencyClass::NonIdempotent,
    )
    .await;

    let (_hand_id, output) = router
        .execute_authorized_with_recovery(&session(), None, &bash_invocation())
        .await
        .expect("non-idempotent failure should return a tool output");

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
            },
            HandRoute {
                provider: fallback.provider_name().to_string(),
                tier: SandboxTier::MicroVM,
            },
        ],
        IdempotencyClass::NonIdempotent,
    )
    .await;
    let session = session();

    let (_first_hand_id, first_output) = router
        .execute_authorized_with_recovery(&session, None, &bash_invocation())
        .await
        .expect("first call should use fallback");
    let (_second_hand_id, second_output) = router
        .execute_authorized_with_recovery(&session, None, &bash_invocation())
        .await
        .expect("second call should prefer the proven fallback");

    assert_eq!(first_output.to_text(), "first fallback");
    assert_eq!(second_output.to_text(), "ok");
    assert_eq!(primary.snapshot().provision_calls, 1);
    let fallback = fallback.snapshot();
    assert_eq!(fallback.provision_calls, 1);
    assert_eq!(fallback.execute_calls, 2);
}
