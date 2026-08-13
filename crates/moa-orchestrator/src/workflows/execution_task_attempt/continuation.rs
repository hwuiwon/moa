//! Persisted continuation schema for bounded execution-task attempts.

use moa_core::types::{
    completion::ToolInvocation,
    context::ContextMessage,
    identifiers::ToolCallId,
    security::{SecurityCircuitState, ToolCapabilityId},
    tools::{AsyncToolJobTerminalOutcome, IdempotencyClass},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Current durable schema for a bounded task-agent continuation.
pub(super) const TASK_ATTEMPT_CONTINUATION_SCHEMA_VERSION: u32 = 1;

/// Maximum canonical continuation payload accepted by persistence.
pub(super) const MAX_TASK_ATTEMPT_CONTINUATION_BYTES: usize = 1024 * 1024;

/// Canonical state needed to resume an agent without replaying an external effect.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TaskAttemptContinuation {
    /// Durable schema version.
    pub schema_version: u32,
    /// Exact bounded execution state.
    pub state: TaskAttemptContinuationState,
    /// Exact storage-only action-review resolution consumed by the next attempt.
    pub review_resolution: Option<moa_execution::wire::ExecutionActionReviewResolution>,
    /// Exact terminal provider outcome consumed by a resumed agent external effect.
    pub external_job_resolution: Option<AsyncToolJobTerminalOutcome>,
    /// Release receipt that proves sandbox compute is asleep before this wait was published.
    pub workspace_release_receipt_id: Option<Uuid>,
}

/// Supported bounded continuation points.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum TaskAttemptContinuationState {
    /// Task-local agent state after a complete model/tool boundary.
    Agent {
        /// Complete bounded conversation required by the next model turn.
        messages: Vec<ContextMessage>,
        /// Zero-based model turn to execute next.
        next_turn: u32,
        /// Cumulative durable task usage.
        usage: moa_artifacts::execution_plan::ExecutionUsage,
        /// Prompt-injection circuit state owned by this exact task generation.
        security_circuit: SecurityCircuitState,
        /// Capabilities fenced by the persisted circuit.
        disabled_capabilities: std::collections::BTreeMap<String, ToolCapabilityId>,
        /// Exact effect waiting on a storage-only action review, when present.
        pending_review: Option<Box<PendingReviewedToolInvocation>>,
        /// Model-emitted tool effects not yet dispatched by a bounded slice.
        pending_tool_calls: Vec<ToolInvocation>,
        /// Exact agent tool invocation currently owned by an asynchronous provider job.
        pending_external: Option<PendingExternalToolInvocation>,
    },
    /// Direct capability effect waiting on a storage-only action review.
    CapabilityReview {
        /// Exact reviewed effect; resumption consumes its persisted resolution.
        pending_review: PendingReviewedToolInvocation,
        /// Cumulative durable task usage.
        usage: moa_artifacts::execution_plan::ExecutionUsage,
    },
    /// Direct async-capable effect reserved before its provider start.
    CapabilityExternalStart {
        /// Stable tool-call identity reused if recovery proves the provider did not start.
        tool_id: ToolCallId,
        /// Cumulative durable task usage before provider dispatch.
        usage: moa_artifacts::execution_plan::ExecutionUsage,
    },
}

/// Reviewed provider effect that must never be reconstructed from a fresh model turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingReviewedToolInvocation {
    /// Stable action-review identity.
    pub review_uid: Uuid,
    /// Exact durable review expiry returned by action-review admission.
    pub expires_at: chrono::DateTime<chrono::Utc>,
    /// Exact provider invocation accepted by policy.
    pub invocation: ToolInvocation,
    /// Compiler/catalog-pinned replay semantics for watchdog classification.
    pub effect_idempotency: IdempotencyClass,
}

/// Agent effect that was durably handed to an asynchronous provider.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingExternalToolInvocation {
    /// Stable MOA external-job identity bound before sandbox release.
    pub external_job_uid: Option<Uuid>,
    /// Exact model-emitted invocation awaiting the terminal provider result.
    pub invocation: ToolInvocation,
    /// Compiler/catalog-pinned replay semantics.
    pub effect_idempotency: IdempotencyClass,
}

impl TaskAttemptContinuation {
    /// Returns the exact action-review identity carried by a parked continuation.
    pub(super) const fn pending_review_uid(&self) -> Option<Uuid> {
        match &self.state {
            TaskAttemptContinuationState::Agent { pending_review, .. } => match pending_review {
                Some(pending) => Some(pending.review_uid),
                None => None,
            },
            TaskAttemptContinuationState::CapabilityReview { pending_review, .. } => {
                Some(pending_review.review_uid)
            }
            TaskAttemptContinuationState::CapabilityExternalStart { .. } => None,
        }
    }

    /// Binds the deterministic MOA external-job identity before checkpoint persistence.
    pub(super) fn bind_external_job(&mut self, external_job_uid: Uuid) -> Result<(), String> {
        let TaskAttemptContinuationState::Agent {
            pending_external: Some(pending),
            ..
        } = &mut self.state
        else {
            return Err("agent external continuation is missing its pending effect".to_string());
        };
        if pending
            .external_job_uid
            .is_some_and(|current| current != external_job_uid)
        {
            return Err("agent external continuation is bound to another job".to_string());
        }
        pending.external_job_uid = Some(external_job_uid);
        Ok(())
    }

    /// Serializes and enforces the hard continuation-size bound before any DB write.
    pub(super) fn to_bounded_json(&self) -> Result<serde_json::Value, String> {
        if self.schema_version != TASK_ATTEMPT_CONTINUATION_SCHEMA_VERSION {
            return Err(format!(
                "unsupported task continuation schema version {}",
                self.schema_version
            ));
        }
        let bytes = serde_json::to_vec(self)
            .map_err(|error| format!("serialize task continuation: {error}"))?;
        if bytes.len() > MAX_TASK_ATTEMPT_CONTINUATION_BYTES {
            return Err(format!(
                "task continuation is {} bytes; maximum is {} and the task must be decomposed or replanned",
                bytes.len(),
                MAX_TASK_ATTEMPT_CONTINUATION_BYTES
            ));
        }
        serde_json::from_slice(&bytes)
            .map_err(|error| format!("decode canonical task continuation: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use moa_artifacts::execution_plan::ExecutionUsage;

    use super::*;

    // Pins: a continuation that cannot fit in the bounded durable payload is rejected
    // before persistence so callers must decompose or request a replan.
    #[test]
    fn oversized_agent_continuation_requires_decomposition_offline() {
        let continuation = TaskAttemptContinuation {
            schema_version: TASK_ATTEMPT_CONTINUATION_SCHEMA_VERSION,
            state: TaskAttemptContinuationState::Agent {
                messages: vec![ContextMessage::user(
                    "x".repeat(MAX_TASK_ATTEMPT_CONTINUATION_BYTES),
                )],
                next_turn: 1,
                usage: ExecutionUsage {
                    cost_microusd: 0,
                    tokens: 0,
                    tool_calls: 0,
                    retrieved_bytes: 0,
                },
                security_circuit: SecurityCircuitState::default(),
                disabled_capabilities: std::collections::BTreeMap::new(),
                pending_review: None,
                pending_tool_calls: Vec::new(),
                pending_external: None,
            },
            review_resolution: None,
            external_job_resolution: None,
            workspace_release_receipt_id: None,
        };

        let error = continuation
            .to_bounded_json()
            .expect_err("oversized continuation must fail closed");
        assert!(error.contains("must be decomposed or replanned"));
    }

    // Pins: a direct async capability resumes with the same stable tool-call ID
    // after a NotStarted recovery instead of creating a second provider identity.
    #[test]
    fn direct_external_start_checkpoint_round_trips_stable_tool_id_offline() {
        let tool_id = ToolCallId(Uuid::from_u128(77));
        let continuation = TaskAttemptContinuation {
            schema_version: TASK_ATTEMPT_CONTINUATION_SCHEMA_VERSION,
            state: TaskAttemptContinuationState::CapabilityExternalStart {
                tool_id,
                usage: zero_usage(),
            },
            review_resolution: None,
            external_job_resolution: None,
            workspace_release_receipt_id: None,
        };

        let decoded: TaskAttemptContinuation = serde_json::from_value(
            continuation
                .to_bounded_json()
                .expect("direct provisional continuation must fit"),
        )
        .expect("direct provisional continuation must decode");
        assert!(matches!(
            decoded.state,
            TaskAttemptContinuationState::CapabilityExternalStart {
                tool_id: decoded_tool_id,
                ..
            } if decoded_tool_id == tool_id
        ));
    }

    const fn zero_usage() -> ExecutionUsage {
        ExecutionUsage {
            cost_microusd: 0,
            tokens: 0,
            tool_calls: 0,
            retrieved_bytes: 0,
        }
    }
}
