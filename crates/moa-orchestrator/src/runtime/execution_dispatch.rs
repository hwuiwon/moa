//! Strict conversion from durable execution outbox rows to Restate targets.

use moa_core::types::identifiers::TenantId;
use moa_execution::{
    repository::outbox::{ExecutionDispatchKind, ExecutionDispatchRecord},
    wire::{
        ExecutionCompensationAttemptCancelRequest, ExecutionCompensationAttemptRequest,
        ExecutionExternalJobCancelRequest, ExecutionTaskAttemptCancelRequest,
        ExecutionTaskAttemptRequest,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::objects::execution_run_controller::ExecutionRunAdvanceRequest;

/// Journal-safe copy of one claimed outbox row.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JournaledExecutionDispatch {
    /// Immutable outbox identity and downstream idempotency key.
    pub dispatch_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Owning run, when applicable.
    pub run_uid: Option<Uuid>,
    /// Owning task, when applicable.
    pub task_id: Option<Uuid>,
    /// Owning compensation, when applicable.
    pub compensation_id: Option<Uuid>,
    /// Immutable trigger target, when applicable.
    pub trigger_uid: Option<Uuid>,
    /// Exact asynchronous job target, when applicable.
    pub external_job_uid: Option<Uuid>,
    /// Closed delivery target.
    pub kind: ExecutionDispatchKind,
    /// Controller generation fence.
    pub controller_generation: Option<u64>,
    /// Exact scheduling wake.
    pub wake_epoch: Option<u64>,
    /// Task-attempt generation fence.
    pub attempt_generation: Option<u64>,
    /// Compensation logical generation fence.
    pub compensation_generation: Option<u64>,
    /// Compensation-attempt generation fence.
    pub compensation_attempt_generation: Option<u64>,
    /// Immutable target payload.
    pub payload: Value,
    /// Claim attempt count after the current claim.
    pub delivery_attempts: u32,
}

/// Fully validated downstream request selected from one immutable dispatch.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
pub enum ExecutionDispatchTarget {
    /// Run-controller activation.
    RunActivation(ExecutionRunAdvanceRequest),
    /// Task-attempt workflow.
    TaskAttempt(ExecutionTaskAttemptRequest),
    /// Cancellation signal for one exact active task-attempt workflow.
    TaskAttemptCancel(ExecutionTaskAttemptCancelRequest),
    /// Compensation-attempt workflow.
    CompensationAttempt(ExecutionCompensationAttemptRequest),
    /// Cancellation signal for one exact active compensation-attempt workflow.
    CompensationAttemptCancel(ExecutionCompensationAttemptCancelRequest),
    /// Temporal-trigger service.
    TriggerDelivery(ExecutionTriggerDeliveryRequest),
    /// Tool-executor external cancellation.
    ExternalCancel {
        /// Immutable outbox identity and downstream idempotency key.
        dispatch_uid: Uuid,
        /// Exact provider cancellation request.
        request: ExecutionExternalJobCancelRequest,
    },
}

/// Immutable request delivered to `ExecutionTrigger/fire`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTriggerDeliveryRequest {
    /// Outbox identity used as the Restate idempotency key.
    pub dispatch_uid: Uuid,
    /// Owning tenant used to install repository RLS scope.
    pub tenant_id: TenantId,
    /// Immutable trigger identity reloaded by the handler.
    pub trigger_uid: Uuid,
}

impl From<ExecutionDispatchRecord> for JournaledExecutionDispatch {
    fn from(record: ExecutionDispatchRecord) -> Self {
        Self {
            dispatch_uid: record.dispatch_uid,
            tenant_id: record.tenant_id,
            run_uid: record.run_uid,
            task_id: record.task_id,
            compensation_id: record.compensation_id,
            trigger_uid: record.trigger_uid,
            external_job_uid: record.external_job_uid,
            kind: record.kind,
            controller_generation: record.controller_generation,
            wake_epoch: record.wake_epoch,
            attempt_generation: record.attempt_generation,
            compensation_generation: record.compensation_generation,
            compensation_attempt_generation: record.compensation_attempt_generation,
            payload: record.payload,
            delivery_attempts: record.delivery_attempts,
        }
    }
}

impl JournaledExecutionDispatch {
    /// Strictly decodes and cross-checks the downstream request.
    pub fn target(&self) -> Result<ExecutionDispatchTarget, moa_execution::Error> {
        match self.kind {
            ExecutionDispatchKind::RunActivation => {
                let request = ExecutionRunAdvanceRequest {
                    dispatch_uid: self.dispatch_uid,
                    tenant_id: self.tenant_id,
                    run_uid: required(self.run_uid, "run activation run_uid")?,
                    controller_generation: required(
                        self.controller_generation,
                        "run activation controller_generation",
                    )?,
                    wake_epoch: required(self.wake_epoch, "run activation wake_epoch")?,
                };
                Ok(ExecutionDispatchTarget::RunActivation(request))
            }
            ExecutionDispatchKind::TaskAttempt => {
                let request: ExecutionTaskAttemptRequest = decode(&self.payload, "task attempt")?;
                require(
                    request.dispatch_uid == self.dispatch_uid,
                    "task dispatch UID",
                )?;
                require(request.tenant_id == self.tenant_id, "task tenant")?;
                require(
                    request.run_uid == required(self.run_uid, "task run_uid")?,
                    "task run",
                )?;
                require(
                    request.task_id.as_uuid() == required(self.task_id, "task task_id")?,
                    "task identity",
                )?;
                require(
                    request.controller_generation
                        == required(self.controller_generation, "task controller_generation")?,
                    "task controller generation",
                )?;
                require(
                    request.attempt_generation
                        == required(self.attempt_generation, "task attempt_generation")?,
                    "task attempt generation",
                )?;
                Ok(ExecutionDispatchTarget::TaskAttempt(request))
            }
            ExecutionDispatchKind::TaskAttemptCancel => {
                let request: ExecutionTaskAttemptCancelRequest =
                    decode(&self.payload, "task attempt cancellation")?;
                require(
                    request.cancellation_dispatch_uid == self.dispatch_uid,
                    "task cancellation dispatch UID",
                )?;
                require(
                    request.tenant_id == self.tenant_id,
                    "task cancellation tenant",
                )?;
                require(
                    request.run_uid == required(self.run_uid, "task cancellation run_uid")?,
                    "task cancellation run",
                )?;
                require(
                    request.task_id.as_uuid()
                        == required(self.task_id, "task cancellation task_id")?,
                    "task cancellation identity",
                )?;
                require(
                    request.controller_generation
                        == required(
                            self.controller_generation,
                            "task cancellation controller_generation",
                        )?,
                    "task cancellation controller generation",
                )?;
                require(
                    request.attempt_controller_generation > 0,
                    "task cancellation attempt controller generation",
                )?;
                require(
                    request.attempt_generation
                        == required(
                            self.attempt_generation,
                            "task cancellation attempt_generation",
                        )?,
                    "task cancellation attempt generation",
                )?;
                Ok(ExecutionDispatchTarget::TaskAttemptCancel(request))
            }
            ExecutionDispatchKind::CompensationAttempt => {
                let request: ExecutionCompensationAttemptRequest =
                    decode(&self.payload, "compensation attempt")?;
                require(
                    request.dispatch_uid == self.dispatch_uid,
                    "compensation dispatch UID",
                )?;
                require(request.tenant_id == self.tenant_id, "compensation tenant")?;
                require(
                    request.run_uid == required(self.run_uid, "compensation run_uid")?,
                    "compensation run",
                )?;
                require(
                    request.compensation_id.as_uuid()
                        == required(self.compensation_id, "compensation compensation_id")?,
                    "compensation identity",
                )?;
                require(
                    request.controller_generation
                        == required(
                            self.controller_generation,
                            "compensation controller_generation",
                        )?,
                    "compensation controller generation",
                )?;
                require(
                    request.compensation_generation
                        == required(self.compensation_generation, "compensation generation")?,
                    "compensation generation",
                )?;
                require(
                    request.compensation_attempt_generation
                        == required(
                            self.compensation_attempt_generation,
                            "compensation attempt_generation",
                        )?,
                    "compensation attempt generation",
                )?;
                Ok(ExecutionDispatchTarget::CompensationAttempt(request))
            }
            ExecutionDispatchKind::CompensationAttemptCancel => {
                let request: ExecutionCompensationAttemptCancelRequest =
                    decode(&self.payload, "compensation attempt cancellation")?;
                require(
                    request.cancellation_dispatch_uid == self.dispatch_uid,
                    "compensation cancellation dispatch UID",
                )?;
                require(
                    request.tenant_id == self.tenant_id,
                    "compensation cancellation tenant",
                )?;
                require(
                    request.run_uid == required(self.run_uid, "compensation cancellation run_uid")?,
                    "compensation cancellation run",
                )?;
                require(
                    request.compensation_id.as_uuid()
                        == required(
                            self.compensation_id,
                            "compensation cancellation compensation_id",
                        )?,
                    "compensation cancellation identity",
                )?;
                require(
                    request.controller_generation
                        == required(
                            self.controller_generation,
                            "compensation cancellation controller_generation",
                        )?,
                    "compensation cancellation controller generation",
                )?;
                require(
                    request.attempt_controller_generation > 0,
                    "compensation cancellation attempt controller generation",
                )?;
                require(
                    request.compensation_generation
                        == required(
                            self.compensation_generation,
                            "compensation cancellation generation",
                        )?,
                    "compensation cancellation generation",
                )?;
                require(
                    request.compensation_attempt_generation
                        == required(
                            self.compensation_attempt_generation,
                            "compensation cancellation attempt_generation",
                        )?,
                    "compensation cancellation attempt generation",
                )?;
                Ok(ExecutionDispatchTarget::CompensationAttemptCancel(request))
            }
            ExecutionDispatchKind::TriggerDelivery => Ok(ExecutionDispatchTarget::TriggerDelivery(
                ExecutionTriggerDeliveryRequest {
                    dispatch_uid: self.dispatch_uid,
                    tenant_id: self.tenant_id,
                    trigger_uid: required(self.trigger_uid, "trigger trigger_uid")?,
                },
            )),
            ExecutionDispatchKind::ExternalCancel => {
                let request: ExecutionExternalJobCancelRequest =
                    decode(&self.payload, "external cancellation")?;
                require(
                    request.tenant_id == self.tenant_id,
                    "external cancellation tenant",
                )?;
                require(
                    request.external_job_uid
                        == required(self.external_job_uid, "external external_job_uid")?,
                    "external cancellation job",
                )?;
                Ok(ExecutionDispatchTarget::ExternalCancel {
                    dispatch_uid: self.dispatch_uid,
                    request,
                })
            }
        }
    }
}

fn required<T>(value: Option<T>, field: &str) -> Result<T, moa_execution::Error> {
    value.ok_or_else(|| invalid(format!("execution dispatch is missing {field}")))
}

fn require(condition: bool, field: &str) -> Result<(), moa_execution::Error> {
    if condition {
        Ok(())
    } else {
        Err(invalid(format!(
            "execution dispatch payload disagrees with persisted {field}"
        )))
    }
}

fn decode<T: serde::de::DeserializeOwned>(
    payload: &Value,
    target: &str,
) -> Result<T, moa_execution::Error> {
    serde_json::from_value(payload.clone()).map_err(|error| {
        invalid(format!(
            "execution {target} dispatch payload is invalid: {error}"
        ))
    })
}

fn invalid(message: String) -> moa_execution::Error {
    moa_execution::Error::InvalidRepositoryData { message }
}

#[cfg(test)]
mod tests {
    use moa_execution::{
        state::ExecutionTaskId,
        wire::{ExecutionAttemptCancelReason, ExecutionTaskAttemptCancelRequest},
    };

    use super::*;

    #[test]
    fn run_target_uses_only_persisted_fences() {
        // Pins: an untrusted payload cannot redirect a run activation.
        let dispatch_uid = Uuid::from_u128(1);
        let run_uid = Uuid::from_u128(2);
        let dispatch = JournaledExecutionDispatch {
            dispatch_uid,
            tenant_id: TenantId::from(Uuid::from_u128(3)),
            run_uid: Some(run_uid),
            task_id: None,
            compensation_id: None,
            trigger_uid: None,
            external_job_uid: None,
            kind: ExecutionDispatchKind::RunActivation,
            controller_generation: Some(4),
            wake_epoch: Some(5),
            attempt_generation: None,
            compensation_generation: None,
            compensation_attempt_generation: None,
            payload: serde_json::json!({ "run_uid": Uuid::from_u128(99) }),
            delivery_attempts: 1,
        };

        assert_eq!(
            dispatch.target().expect("run target must decode"),
            ExecutionDispatchTarget::RunActivation(ExecutionRunAdvanceRequest {
                dispatch_uid,
                tenant_id: dispatch.tenant_id,
                run_uid,
                controller_generation: 4,
                wake_epoch: 5,
            })
        );
    }

    #[test]
    fn task_payload_must_match_outbox_coordinates() {
        // Pins: payload corruption cannot move a capacity-owning attempt to another dispatch.
        let dispatch = JournaledExecutionDispatch {
            dispatch_uid: Uuid::from_u128(1),
            tenant_id: TenantId::from(Uuid::from_u128(2)),
            run_uid: Some(Uuid::from_u128(3)),
            task_id: Some(Uuid::from_u128(4)),
            compensation_id: None,
            trigger_uid: None,
            external_job_uid: None,
            kind: ExecutionDispatchKind::TaskAttempt,
            controller_generation: Some(1),
            wake_epoch: None,
            attempt_generation: Some(1),
            compensation_generation: None,
            compensation_attempt_generation: None,
            payload: serde_json::json!({}),
            delivery_attempts: 1,
        };

        let error = dispatch
            .target()
            .expect_err("empty task payload must fail closed");
        assert!(matches!(
            error,
            moa_execution::Error::InvalidRepositoryData { .. }
        ));
    }

    #[test]
    fn task_cancel_payload_cannot_redirect_persisted_attempt_fences() {
        // Pins: dispatcher routing validates the persisted cancellation target
        // before the keyed workflow performs its canonical row-lock checks.
        let dispatch_uid = Uuid::from_u128(1);
        let tenant_id = TenantId::from(Uuid::from_u128(2));
        let run_uid = Uuid::from_u128(3);
        let task_id = ExecutionTaskId::from_uuid(Uuid::from_u128(4));
        let payload = ExecutionTaskAttemptCancelRequest {
            cancellation_dispatch_uid: dispatch_uid,
            tenant_id,
            run_uid,
            task_id,
            controller_generation: 5,
            attempt_controller_generation: 5,
            task_generation: 6,
            attempt_generation: 7,
            active_dispatch_uid: Uuid::from_u128(8),
            capacity_reservation_uid: Uuid::from_u128(9),
            watchdog_trigger_uid: Uuid::from_u128(10),
            reason: ExecutionAttemptCancelReason::RunTerminal,
        };
        let dispatch = JournaledExecutionDispatch {
            dispatch_uid,
            tenant_id,
            run_uid: Some(run_uid),
            task_id: Some(task_id.as_uuid()),
            compensation_id: None,
            trigger_uid: None,
            external_job_uid: None,
            kind: ExecutionDispatchKind::TaskAttemptCancel,
            controller_generation: Some(5),
            wake_epoch: None,
            attempt_generation: Some(99),
            compensation_generation: None,
            compensation_attempt_generation: None,
            payload: serde_json::to_value(payload).expect("serialize task cancellation"),
            delivery_attempts: 1,
        };

        assert!(matches!(
            dispatch.target(),
            Err(moa_execution::Error::InvalidRepositoryData { .. })
        ));
    }
}
