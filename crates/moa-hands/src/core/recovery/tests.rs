use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    ActionClass, ActionPolicyEffect, HandHandle, HandProvider, HandSpec, HandStatus,
    IdempotencyClass, MoaError, Result, RiskLevel, SandboxTier, SessionId, SessionMeta, TenantId,
    ToolDiffStrategy, ToolFailureClass, ToolInputShape, ToolInvocation, ToolOutput, ToolPolicySpec,
};
use serde_json::json;

use crate::core::{ToolRegistry, ToolRouter};

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
    registry.retarget_hand_tools(provider.provider_name(), SandboxTier::Container);
    registry.retain_only(["bash"]);
    let mut providers = HashMap::new();
    providers.insert(provider.provider_name().to_string(), provider);
    ToolRouter::new(registry, providers)
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
    let router =
        router_with_provider_and_idempotency(provider.clone(), IdempotencyClass::Idempotent).await;

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
    let router =
        router_with_provider_and_idempotency(provider.clone(), IdempotencyClass::Idempotent).await;

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
    let router =
        router_with_provider_and_idempotency(provider.clone(), IdempotencyClass::Idempotent).await;

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
        .execute_authorized_with_recovery(&session(), &bash_invocation())
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
        .execute_authorized_with_recovery(&session(), &bash_invocation())
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
        .execute_authorized_with_recovery(&session(), &bash_invocation())
        .await
        .expect("recovery path should return a tool output");

    assert!(!output.is_error);
    assert_eq!(output.to_text(), "ran once");
    let snapshot = provider.snapshot();
    assert_eq!(snapshot.execute_calls, 1);
    assert_eq!(snapshot.provision_calls, 2);
    assert_eq!(snapshot.destroy_calls, 1);
}
