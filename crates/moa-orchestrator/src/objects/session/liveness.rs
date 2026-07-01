//! Per-child heartbeat-liveness watchdog scheduling for the Session VO.
//!
//! When a child becomes active the Session VO arms exactly one single-outstanding,
//! generation-guarded delayed self-call (`Session::check_child_liveness`) per child,
//! reusing the shared delayed-self-call helper that also drives the narration tick. The
//! check is strictly per active child of *this* session — it never polls globally across
//! tenants or sessions.
//!
//! A fired check reads the child's compact progress summary and, when the child is still
//! active and its heartbeat has not advanced past the configured staleness threshold,
//! reschedules itself (same generation). A child that is terminal — or legitimately
//! blocked on a `request_input` round-trip (`awaiting_input`) and therefore emitting no
//! heartbeats — is never flagged: terminal stops the watchdog, awaiting-input keeps
//! watching without raising. A genuinely stale child appends an idempotent
//! `WorkerHeartbeatStale` event and raises a resume-eligible `HeartbeatStale` control
//! signal, then stops. The staleness decision is a pure function so it is unit-testable
//! without a Restate runtime.

use std::time::Duration;

use moa_core::{AgentSignalId, ChildSignalKind, ParentResumePolicy, SignalSeverity};

use super::*;
use crate::vo::schedule_generation_guarded_self_call;

/// Registered Restate object name for the Session VO, used for the untyped self-call.
const SESSION_OBJECT_NAME: &str = "Session";
/// Handler name of the per-child liveness watchdog tick on the Session VO.
const CHECK_CHILD_LIVENESS_HANDLER: &str = "check_child_liveness";

/// Arms one single-outstanding liveness check for a child that just became active.
///
/// Called from the `register_child` active edge. The single-outstanding guard (one
/// `child_liveness` entry per child) prevents overlapping schedules; the monotonic
/// generation lets a superseded check no-op when it fires. A zero stale threshold
/// disables the watchdog. Mutates `state`; the caller persists.
pub(super) async fn ensure_child_liveness_scheduled(
    ctx: &ObjectContext<'_>,
    state: &mut SessionVoState,
    worker_id: &str,
) -> Result<(), HandlerError> {
    let stale_ms = OrchestratorCtx::current_config()
        .session_limits
        .worker_heartbeat_stale_ms;
    if stale_ms == 0 {
        return Ok(());
    }
    let Some(generation) = state.arm_child_liveness(worker_id) else {
        return Ok(());
    };
    let now = durable_utc_now(ctx).await?;
    send_child_liveness_check(ctx, worker_id, generation, now, stale_ms);
    Ok(())
}

/// Runs one generation-guarded per-child liveness check.
///
/// No-op when a newer arming/clear superseded this check's generation, when the child is
/// no longer owned by this session, when the child is terminal, or when the child is
/// `awaiting_input`. Otherwise it reschedules while the heartbeat is fresh, or raises a
/// non-fatal stale signal (and stops) once the heartbeat ages past the threshold.
pub(super) async fn run_child_liveness_check(
    ctx: &ObjectContext<'_>,
    req: CheckChildLivenessRequest,
) -> Result<(), HandlerError> {
    let mut state = SessionVoState::load_from(ctx).await?;

    // Per-child generation guard: a superseded (or cleared) check no longer owns
    // scheduling for this child, so drop it without rescheduling.
    if !state.liveness_generation_matches(&req.worker_id, req.expected_generation) {
        return Ok(());
    }

    // Stop if the child is no longer owned by this session (e.g. removed on self-clean).
    let cached_terminal = match state
        .children
        .iter()
        .find(|child| child.id == req.worker_id)
        .map(|child| child.terminal.is_some())
    {
        Some(terminal) => terminal,
        None => {
            state.clear_child_liveness(&req.worker_id);
            state.persist_into(ctx);
            return Ok(());
        }
    };

    let stale_ms = OrchestratorCtx::current_config()
        .session_limits
        .worker_heartbeat_stale_ms;
    let summary = fetch_child_summary(ctx, &req.worker_id).await;
    let now = durable_utc_now(ctx).await?;

    // A failed read leaves the heartbeat unknown: treat as not-terminal/not-awaiting and
    // reschedule rather than fail the watchdog.
    let is_terminal = cached_terminal
        || summary
            .as_ref()
            .is_some_and(|summary| crate::delegation::is_terminal_worker_state(summary.state));
    let awaiting_input = summary
        .as_ref()
        .is_some_and(|summary| summary.awaiting_input);
    let last_heartbeat_at = summary
        .as_ref()
        .and_then(|summary| summary.last_heartbeat_at);

    match decide_child_liveness(
        is_terminal,
        awaiting_input,
        last_heartbeat_at,
        now,
        stale_ms,
    ) {
        ChildLivenessDecision::Stop => {
            state.clear_child_liveness(&req.worker_id);
            state.persist_into(ctx);
        }
        ChildLivenessDecision::Reschedule { delay_ms } => {
            // Same generation: the single outstanding check continues watching this child.
            send_child_liveness_check(ctx, &req.worker_id, req.expected_generation, now, delay_ms);
        }
        ChildLivenessDecision::Stale { last_heartbeat_at } => {
            let session_id = parse_session_key(ctx.key())?;
            raise_child_stale(ctx, session_id, &req.worker_id, last_heartbeat_at, stale_ms).await?;
            // The watchdog has raised the alarm; stop so a stuck child cannot fan out a
            // stream of resume signals while it stays stale.
            state.clear_child_liveness(&req.worker_id);
            state.persist_into(ctx);
        }
    }
    Ok(())
}

/// Reads the child's compact progress summary, omitting it on a failed read.
async fn fetch_child_summary(
    ctx: &ObjectContext<'_>,
    worker_id: &str,
) -> Option<WorkerProgressSummary> {
    match ctx
        .object_client::<WorkerClient>(worker_id.to_string())
        .progress_summary()
        .call()
        .await
    {
        Ok(summary) => Some(summary.into_inner()),
        Err(error) => {
            tracing::warn!(
                worker_id = %worker_id,
                error = %error,
                "child liveness: progress summary unavailable; rescheduling"
            );
            None
        }
    }
}

/// Issues one generation-guarded delayed self-call to `check_child_liveness`.
fn send_child_liveness_check(
    ctx: &ObjectContext<'_>,
    worker_id: &str,
    generation: u64,
    now: DateTime<Utc>,
    delay_ms: u64,
) {
    let delay = Duration::from_millis(delay_ms);
    let scheduled_at = now + chrono::Duration::milliseconds(delay_ms as i64);
    let scheduled_for_millis = scheduled_at.timestamp_millis();
    schedule_generation_guarded_self_call(
        ctx,
        SESSION_OBJECT_NAME,
        CHECK_CHILD_LIVENESS_HANDLER,
        generation,
        format!("{worker_id}:{scheduled_for_millis}"),
        Json::from(CheckChildLivenessRequest {
            worker_id: worker_id.to_string(),
            expected_generation: generation,
            scheduled_at,
        }),
        delay,
    );
}

/// Decision of whether a fired liveness check should stop, reschedule, or raise stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildLivenessDecision {
    /// Terminal child: stop the watchdog.
    Stop,
    /// Active and not stale (fresh, missing, or legitimately awaiting input): reschedule.
    Reschedule {
        /// Delay until the next check should fire.
        delay_ms: u64,
    },
    /// Heartbeat aged past the threshold while active: raise a non-fatal stale signal.
    Stale {
        /// Last heartbeat observed before staleness, carried into the event/dedupe key.
        last_heartbeat_at: DateTime<Utc>,
    },
}

/// Pure liveness decision: terminal → stop, awaiting-input → never stale, else compare
/// the heartbeat age to the threshold.
///
/// Kept free of `ctx` so the staleness ordering is unit-testable with deterministic
/// timestamps. A child with no heartbeat yet is never stale (it has not been given a
/// chance to beat), and an `awaiting_input` child is never stale even with an aged
/// heartbeat because it is blocked on a `request_input` round-trip, not stuck.
fn decide_child_liveness(
    is_terminal: bool,
    awaiting_input: bool,
    last_heartbeat_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    stale_threshold_ms: u64,
) -> ChildLivenessDecision {
    if is_terminal {
        return ChildLivenessDecision::Stop;
    }
    if awaiting_input {
        return ChildLivenessDecision::Reschedule {
            delay_ms: stale_threshold_ms,
        };
    }
    match last_heartbeat_at {
        Some(last) if (now - last).num_milliseconds() >= stale_threshold_ms as i64 => {
            ChildLivenessDecision::Stale {
                last_heartbeat_at: last,
            }
        }
        Some(last) => {
            let age_ms = (now - last).num_milliseconds().max(0) as u64;
            ChildLivenessDecision::Reschedule {
                delay_ms: stale_threshold_ms.saturating_sub(age_ms).max(1),
            }
        }
        None => ChildLivenessDecision::Reschedule {
            delay_ms: stale_threshold_ms,
        },
    }
}

/// Builds the idempotency dedupe key for one stale-heartbeat detection.
///
/// Keyed on `(worker_id, last_heartbeat_at)` so the same stale condition appends at
/// most one `WorkerHeartbeatStale` event no matter how often the watchdog re-detects it.
fn stale_dedupe_key(worker_id: &str, last_heartbeat_at: DateTime<Utc>) -> String {
    format!(
        "worker_stale:{worker_id}:{}",
        last_heartbeat_at.timestamp_millis()
    )
}

/// Appends the idempotent stale event and raises a resume-eligible stale control signal.
///
/// The event dedupes on `(worker_id, last_heartbeat_at)`. The `HeartbeatStale` signal
/// is delivered to this same Session VO via a detached self-send of `record_child_signal`
/// (never inline, to avoid re-entering the single-writer queue), which applies the idle
/// resume gate (`resume_policy = IfIdle`) so only an idle coordinator is woken. Non-fatal.
async fn raise_child_stale(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    worker_id: &str,
    last_heartbeat_at: DateTime<Utc>,
    threshold_ms: u64,
) -> Result<(), HandlerError> {
    append_session_event_deduped(
        ctx,
        session_id,
        Event::WorkerHeartbeatStale {
            worker_id: worker_id.to_string(),
            last_heartbeat_at,
            threshold_ms,
        },
        stale_dedupe_key(worker_id, last_heartbeat_at),
    )
    .await?;

    let signal_id = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(AgentSignalId::new())) })
        .name("worker_stale_signal_id")
        .await?
        .into_inner();
    let created_at = ctx
        .run(|| async { Ok::<_, HandlerError>(Json::from(Utc::now())) })
        .name("worker_stale_signal_at")
        .await?
        .into_inner();

    let summary =
        format!("worker {worker_id} heartbeat went stale (no progress for over {threshold_ms}ms)");
    ctx.object_client::<SessionClient>(session_id.to_string())
        .record_child_signal(Json::from(WorkerSignal {
            signal_id,
            worker_id: worker_id.to_string(),
            parent_session: session_id,
            kind: ChildSignalKind::HeartbeatStale,
            severity: SignalSeverity::Warning,
            summary,
            payload: serde_json::Value::Null,
            created_at,
            resume_policy: ParentResumePolicy::IfIdle,
            input_request_id: None,
            input_audience: None,
        }))
        .send();
    tracing::info!(
        key = %ctx.key(),
        worker_id = %worker_id,
        threshold_ms,
        "raised worker heartbeat-stale signal"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration, Utc};

    use super::{ChildLivenessDecision, decide_child_liveness, stale_dedupe_key};

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).expect("valid timestamp")
    }

    #[test]
    fn aged_heartbeat_without_awaiting_input_is_stale() {
        // Pins: an active child whose heartbeat is older than the threshold and is not
        // awaiting input is flagged stale, carrying the observed heartbeat for the event.
        let now = ts(1_000_000);
        let heartbeat = now - Duration::milliseconds(120_000);
        let decision = decide_child_liveness(false, false, Some(heartbeat), now, 60_000);
        assert_eq!(
            decision,
            ChildLivenessDecision::Stale {
                last_heartbeat_at: heartbeat
            }
        );
        // The dedupe key is keyed on the child id and the exact stale heartbeat instant.
        assert_eq!(
            stale_dedupe_key("child-1", heartbeat),
            format!("worker_stale:child-1:{}", heartbeat.timestamp_millis())
        );
    }

    #[test]
    fn fresh_heartbeat_reschedules() {
        // Pins: an active child whose heartbeat advanced within the threshold keeps the
        // watchdog rescheduling rather than raising a false stale alarm.
        let now = ts(1_000_000);
        let heartbeat = now - Duration::milliseconds(5_000);
        assert_eq!(
            decide_child_liveness(false, false, Some(heartbeat), now, 60_000),
            ChildLivenessDecision::Reschedule { delay_ms: 55_000 }
        );
    }

    #[test]
    fn nearly_stale_heartbeat_reschedules_at_least_one_millisecond() {
        // Pins: a fresh-but-near-threshold heartbeat does not schedule a zero-delay loop.
        let now = ts(1_000_000);
        let heartbeat = now - Duration::milliseconds(59_999);
        assert_eq!(
            decide_child_liveness(false, false, Some(heartbeat), now, 60_000),
            ChildLivenessDecision::Reschedule { delay_ms: 1 }
        );
    }

    #[test]
    fn awaiting_input_is_never_stale_even_with_aged_heartbeat() {
        // Pins: a child blocked on a request_input round-trip emits no heartbeats but is
        // legitimately waiting, so an aged heartbeat must NOT raise a stale signal.
        let now = ts(1_000_000);
        let heartbeat = now - Duration::milliseconds(600_000);
        assert_eq!(
            decide_child_liveness(false, true, Some(heartbeat), now, 60_000),
            ChildLivenessDecision::Reschedule { delay_ms: 60_000 }
        );
    }

    #[test]
    fn missing_heartbeat_reschedules() {
        // Pins: a child that has not beat yet is never stale (it has not been given a
        // chance), so the watchdog keeps watching instead of flagging it.
        let now = ts(1_000_000);
        assert_eq!(
            decide_child_liveness(false, false, None, now, 60_000),
            ChildLivenessDecision::Reschedule { delay_ms: 60_000 }
        );
    }

    #[test]
    fn terminal_child_stops_the_watchdog() {
        // Pins: a terminal child stops the watchdog regardless of heartbeat age, so a
        // finished child is never flagged stale.
        let now = ts(1_000_000);
        let heartbeat = now - Duration::milliseconds(600_000);
        assert_eq!(
            decide_child_liveness(true, false, Some(heartbeat), now, 60_000),
            ChildLivenessDecision::Stop
        );
    }
}
