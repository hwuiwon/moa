//! Durable resource supervision for one behavior-lab trial.
//!
//! Every paid or side-effecting dispatch a trial makes goes through here first.
//! The rule the module exists to enforce is narrow and absolute: capacity is
//! withheld from the run's durable ledger *before* the call, and actual usage is
//! reconciled against that reservation *after* it. A trial that cannot reserve
//! does not dispatch.
//!
//! Two properties make this survive Restate replay:
//!
//! * every reservation is keyed by a deterministic dispatch coordinate — trial,
//!   component, turn index — so a re-executed journal step finds its own
//!   reservation instead of charging the envelope twice; and
//! * the reservation and reconciliation each happen inside one journaled step,
//!   so the wall-clock instant they used is replayed rather than recomputed.
//!
//! Parallel trials of one run share the run ledger, so the sum of what they
//! withhold is bounded by the run envelope, not by the per-trial envelope times
//! the number of trials.

use super::*;

use crate::workflows::durable_utc_now;
use moa_core::types::resource::{ResourceAmounts, ResourceEnvelope};
use moa_experiments::model::{
    ExperimentResourceAdmission, ExperimentResourceComponent, ExperimentResourceDenial,
    ExperimentResourceReservationRequest, ExperimentResourceUsage, ExperimentTurnResourceShares,
};

/// Splits a trial envelope into the worst case one simulated turn may consume.
///
/// The envelope authored a turn ceiling, so the honest per-turn worst case is
/// the envelope divided by that ceiling. Reserving that share for every turn
/// makes the sum of a trial's reservations exactly its envelope, and a turn that
/// would push past it is refused before it dispatches rather than after it
/// bills.
pub(super) fn per_turn_worst_case(limits: ResourceAmounts) -> ResourceAmounts {
    ExperimentTurnResourceShares::from_trial_limits(limits).turn
}

/// The simulator's share of one turn.
///
/// The simulator issues exactly one model call and no tool calls, and it does
/// not consume a target turn. It takes half the turn's money and tokens; the
/// target keeps the rest.
pub(super) fn simulator_worst_case(turn: ResourceAmounts) -> ResourceAmounts {
    ExperimentTurnResourceShares::from_turn(turn).simulator
}

/// The target's share of one turn: everything the simulator did not take.
pub(super) fn target_worst_case(turn: ResourceAmounts) -> ResourceAmounts {
    ExperimentTurnResourceShares::from_turn(turn).target
}

/// Deterministic reservation key for one simulator model call.
pub(super) fn simulator_reservation_key(trial_uid: Uuid, turn_index: u32) -> String {
    format!("trial:{trial_uid}:simulator:{turn_index}")
}

/// Deterministic reservation key for one target turn.
pub(super) fn target_reservation_key(trial_uid: Uuid, turn_index: u32) -> String {
    format!("trial:{trial_uid}:target:{turn_index}")
}

/// Deterministic reservation key for one execution-template dispatch.
///
/// A durable execution run is one dispatch rather than a per-turn loop, so it has
/// exactly one reservation for the whole trial envelope.
pub(super) fn execution_reservation_key(trial_uid: Uuid) -> String {
    format!("trial:{trial_uid}:target:execution")
}

/// Withholds capacity on the run ledger for one upcoming dispatch.
///
/// The caller must dispatch only on [`ExperimentResourceAdmission::Granted`].
pub(super) async fn reserve(
    ctx: &WorkflowContext<'_>,
    trial: &ExperimentTrialRecord,
    component: ExperimentResourceComponent,
    reservation_key: String,
    worst_case: ResourceAmounts,
    pool: &sqlx::PgPool,
) -> Result<ExperimentResourceAdmission, HandlerError> {
    let pool = pool.clone();
    let request = ExperimentResourceReservationRequest {
        run_uid: trial.run_uid,
        trial_uid: trial.trial_uid,
        reservation_key,
        component,
        worst_case,
    };
    let scope = trial.scope;
    Ok(ctx
        .run(|| {
            let pool = pool.clone();
            let request = request.clone();
            async move {
                // `Utc::now()` inside the journaled step: the admission decision
                // is replayed, not re-decided against a clock that has moved
                // past the envelope's deadline.
                ExperimentStore::new(pool)
                    .try_reserve_resources(&scope, request, Utc::now())
                    .await
                    .map(Json::from)
                    .map_err(moa_error_to_handler_error)
            }
        })
        .name("experiment_trial_reserve_resources")
        .await?
        .into_inner())
}

/// Returns a denial or confirms that this invocation owns a fresh reservation.
///
/// `AlreadySettled` means the durable dispatch coordinate completed in an
/// earlier invocation. Treating it like a grant would issue the paid side
/// effect again without withholding any capacity.
pub(crate) fn reservation_denial(
    admission: &ExperimentResourceAdmission,
) -> Result<Option<&ExperimentResourceDenial>, HandlerError> {
    match admission {
        ExperimentResourceAdmission::Granted(_) => Ok(None),
        ExperimentResourceAdmission::Denied(denial) => Ok(Some(denial)),
        ExperimentResourceAdmission::AlreadySettled(record) => Err(TerminalError::new(format!(
            "experiment resource reservation {} is already settled; refusing duplicate dispatch",
            record.reservation_key
        ))
        .into()),
    }
}

/// Commits the real usage of a dispatch and frees the unused reservation.
pub(super) async fn reconcile(
    ctx: &WorkflowContext<'_>,
    trial: &ExperimentTrialRecord,
    reservation_key: String,
    actual: ExperimentResourceUsage,
    pool: &sqlx::PgPool,
) -> Result<(), HandlerError> {
    let pool = pool.clone();
    let scope = trial.scope;
    let run_uid = trial.run_uid;
    ctx.run(|| {
        let pool = pool.clone();
        let reservation_key = reservation_key.clone();
        async move {
            ExperimentStore::new(pool)
                .reconcile_resources(&scope, run_uid, &reservation_key, actual)
                .await
                .map(|outcome| Json::from(outcome.overrun().is_some()))
                .map_err(moa_error_to_handler_error)
        }
    })
    .name("experiment_trial_reconcile_resources")
    .await?;
    Ok(())
}

/// Returns a reservation whose dispatch never happened.
pub(super) async fn release(
    ctx: &WorkflowContext<'_>,
    trial: &ExperimentTrialRecord,
    reservation_key: String,
    pool: &sqlx::PgPool,
) -> Result<(), HandlerError> {
    let pool = pool.clone();
    let scope = trial.scope;
    let run_uid = trial.run_uid;
    ctx.run(|| {
        let pool = pool.clone();
        let reservation_key = reservation_key.clone();
        async move {
            ExperimentStore::new(pool)
                .release_resources(&scope, run_uid, &reservation_key)
                .await
                .map(Json::from)
                .map_err(moa_error_to_handler_error)
        }
    })
    .name("experiment_trial_release_resources")
    .await?;
    Ok(())
}

/// Returns whether the trial's absolute deadline has already passed.
///
/// Read through a journaled step so a replay takes the same branch it took the
/// first time instead of re-deciding against a clock that has since moved.
pub(super) async fn deadline_passed(
    ctx: &WorkflowContext<'_>,
    envelope: &ResourceEnvelope,
) -> Result<bool, HandlerError> {
    let now = durable_utc_now(ctx, "experiment_trial_deadline_check").await?;
    Ok(envelope.deadline_passed(now))
}

/// Time a trial may still wait on a child before its envelope expires.
///
/// Returns `None` when the deadline has already passed, which the caller must
/// treat as "stop waiting and stop the trial".
pub(super) fn time_remaining(
    envelope: &ResourceEnvelope,
    now: chrono::DateTime<Utc>,
) -> Option<Duration> {
    let remaining = envelope.time_remaining(now)?;
    (!remaining.is_zero()).then_some(remaining)
}

/// The durable terminal shape of a trial stopped by its own resource envelope.
///
/// `BudgetCap` is the typed reason for every envelope refusal, including the
/// absolute deadline: the deadline is one dimension of the same authored
/// envelope. The exact refusal — which dimension, which scope, or the deadline —
/// travels in the durable error string so an operator can tell them apart
/// without a second enum that means the same thing.
pub(super) struct TrialResourceStop {
    /// Status the trial reaches.
    pub(super) status: ExperimentTrialStatus,
    /// Stable, PII-free error string recorded with the status.
    pub(super) error: Option<String>,
}

impl TrialResourceStop {
    /// Builds the terminal shape for one refused reservation.
    pub(super) fn from_denial(denial: &ExperimentResourceDenial) -> Self {
        if denial.is_deadline() {
            // The trial never reached a stopping point of its own: it ran out of
            // wall-clock time, which is an operator-visible failure.
            return Self {
                status: ExperimentTrialStatus::Failed,
                error: Some("experiment_resource_deadline_exceeded".to_string()),
            };
        }
        // The trial consumed exactly the capacity it was authorized to consume.
        // That is a clean stop at an authored ceiling, like the turn cap, and the
        // scorecard — not the lifecycle status — decides whether the evidence it
        // produced is good enough.
        Self {
            status: ExperimentTrialStatus::Completed,
            error: None,
        }
    }

    /// Builds the terminal shape for a deadline observed outside a reservation.
    pub(super) fn deadline() -> Self {
        Self {
            status: ExperimentTrialStatus::Failed,
            error: Some("experiment_resource_deadline_exceeded".to_string()),
        }
    }
}

/// Bounded trial envelope for orchestrator trial-record fixtures.
///
/// One helper rather than a literal repeated per fixture, so a change to the
/// envelope shape is a single edit instead of three that can silently diverge.
#[cfg(test)]
pub(crate) fn fixture_trial_envelope() -> moa_core::types::resource::ResourceEnvelope {
    moa_core::types::resource::ResourceEnvelope {
        version: moa_core::types::resource::RESOURCE_CONTRACT_VERSION,
        limits: moa_core::types::resource::ResourceAmounts {
            cost_micro_usd: 1_000_000,
            tokens: 100_000,
            turns: 8,
            model_calls: 16,
            tool_calls: 32,
        },
        deadline: Some(Utc::now() + chrono::Duration::hours(1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(
        cost: u64,
        tokens: u64,
        turns: u64,
        model_calls: u64,
        tool_calls: u64,
    ) -> ResourceAmounts {
        ResourceAmounts {
            cost_micro_usd: cost,
            tokens,
            turns,
            model_calls,
            tool_calls,
        }
    }

    #[test]
    fn settled_reservation_cannot_authorize_another_dispatch() {
        // Pins: retrying a settled coordinate must fail closed rather than
        // silently issuing a second paid side effect.
        let record = moa_experiments::model::ExperimentResourceReservationRecord {
            reservation_uid: Uuid::from_u128(1),
            run_uid: Uuid::from_u128(2),
            trial_uid: Some(Uuid::from_u128(3)),
            reservation_key: "trial:3:simulator:0".to_string(),
            component: ExperimentResourceComponent::Simulator,
            state: moa_experiments::model::ExperimentResourceReservationState::Reconciled,
            reserved: limits(10, 20, 0, 1, 0),
            actual: Some(ExperimentResourceUsage::model_call(8, 4, 7)),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let error = reservation_denial(&ExperimentResourceAdmission::AlreadySettled(record))
            .expect_err("settled reservation must not grant dispatch");
        assert!(format!("{error:?}").contains("refusing duplicate dispatch"));
    }

    #[test]
    fn per_turn_shares_never_oversubscribe_the_trial_envelope_offline() {
        // Pins: reserving the simulator share plus the target share on every
        // authorized turn sums to at most the trial envelope, so a trial cannot
        // spend more than it was admitted for by taking every turn it may take.
        let envelope = limits(1_000_000, 40_000, 8, 16, 64);
        let turn = per_turn_worst_case(envelope);
        let simulator = simulator_worst_case(turn);
        let target = target_worst_case(turn);

        let per_turn_cost = simulator.cost_micro_usd + target.cost_micro_usd;
        let per_turn_tokens = simulator.tokens + target.tokens;
        assert!(per_turn_cost * envelope.turns <= envelope.cost_micro_usd);
        assert!(per_turn_tokens * envelope.turns <= envelope.tokens);
        assert_eq!(simulator.turns + target.turns, 1);
    }

    #[test]
    fn a_turn_share_always_buys_at_least_one_model_call_offline() {
        // Pins: integer division must not produce a zero-model-call share, which
        // the ledger would reject as an empty reservation and which would stall
        // every trial under a small envelope.
        let turn = per_turn_worst_case(limits(10, 10, 100, 1, 0));
        assert_eq!(turn.model_calls, 1);
        assert_eq!(simulator_worst_case(turn).model_calls, 1);
        assert_eq!(target_worst_case(turn).model_calls, 1);
        assert!(!simulator_worst_case(turn).is_zero());
        assert!(!target_worst_case(turn).is_zero());
    }

    #[test]
    fn reservation_keys_are_stable_across_replays_offline() {
        // Pins: the key is the trial's durable coordinate plus the turn index, so
        // a replayed dispatch reserves the same row rather than a second one.
        let trial_uid = Uuid::from_u128(7);
        assert_eq!(
            simulator_reservation_key(trial_uid, 3),
            "trial:00000000-0000-0000-0000-000000000007:simulator:3"
        );
        assert_eq!(
            target_reservation_key(trial_uid, 3),
            "trial:00000000-0000-0000-0000-000000000007:target:3"
        );
        assert_ne!(
            target_reservation_key(trial_uid, 3),
            execution_reservation_key(trial_uid)
        );
    }

    #[test]
    fn a_child_wait_never_outlives_the_trial_envelope_offline() {
        // Pins: the wait budget a trial grants a durable child is the smaller of the
        // platform ceiling and its own remaining envelope. An expired envelope yields
        // no budget at all, so the caller cannot keep polling a run that is already
        // billing past its authorized deadline.
        let now = chrono::DateTime::from_timestamp(1_000_000, 0)
            .expect("fixed instant")
            .to_utc();
        let envelope = |seconds: i64| ResourceEnvelope {
            version: moa_core::types::resource::RESOURCE_CONTRACT_VERSION,
            limits: limits(1, 1, 1, 1, 1),
            deadline: Some(now + chrono::Duration::seconds(seconds)),
        };

        assert_eq!(
            time_remaining(&envelope(30), now),
            Some(std::time::Duration::from_secs(30)),
            "an envelope with time left grants exactly that much"
        );
        assert_eq!(
            time_remaining(&envelope(0), now),
            None,
            "an envelope expiring exactly now grants no wait"
        );
        assert_eq!(
            time_remaining(&envelope(-5), now),
            None,
            "an already-expired envelope grants no wait rather than a negative one"
        );
        assert_eq!(
            time_remaining(
                &ResourceEnvelope {
                    version: moa_core::types::resource::RESOURCE_CONTRACT_VERSION,
                    limits: limits(1, 1, 1, 1, 1),
                    deadline: None,
                },
                now,
            ),
            None,
            "an explicitly unbounded envelope states no deadline to bound a wait by"
        );
    }

    #[test]
    fn an_exhausted_envelope_completes_and_a_missed_deadline_fails_offline() {
        // Pins: both stop the trial with the same typed reason, but a trial that
        // spent its authorized ceiling is not reported as broken while one that
        // ran out of wall-clock time is.
        let exhausted = ExperimentResourceDenial {
            reason: moa_experiments::model::ExperimentResourceDenialReason::TrialEnvelopeExhausted,
            kind: Some(moa_core::types::resource::ResourceKind::CostMicroUsd),
            requested: 10,
            remaining: 1,
            limit: 100,
            deadline_at: None,
            message: "exhausted".to_string(),
        };
        let stop = TrialResourceStop::from_denial(&exhausted);
        assert_eq!(stop.status, ExperimentTrialStatus::Completed);
        assert!(stop.error.is_none());

        let expired = ExperimentResourceDenial {
            reason: moa_experiments::model::ExperimentResourceDenialReason::DeadlineExceeded,
            kind: None,
            requested: 0,
            remaining: 0,
            limit: 0,
            deadline_at: Some(Utc::now()),
            message: "deadline".to_string(),
        };
        let stop = TrialResourceStop::from_denial(&expired);
        assert_eq!(stop.status, ExperimentTrialStatus::Failed);
        assert_eq!(
            stop.error.as_deref(),
            Some("experiment_resource_deadline_exceeded")
        );
    }
}
