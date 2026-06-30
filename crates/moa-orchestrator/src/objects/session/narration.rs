//! Per-session progress-narration scheduling and cost gating for the Session VO.
//!
//! The Session VO drives default-on, user-facing progress narration with a
//! generation-guarded delayed self-call (`Session::narration_tick`), modeled on the
//! `CronJob` virtual object. The tick never makes the LLM call inline: when its cost
//! gate opens it `.send()`s the detached `LLMGateway::narrate_session` job *off* the
//! single-writer VO and reschedules itself while any source is still active.
//!
//! Scheduling is single-outstanding (at most one tick in flight per generation) and
//! the change cursor, interval, and rolling-window cap that decide whether to narrate
//! are factored into pure functions so they can be unit-tested without a Restate
//! runtime.

use std::time::Duration;

use moa_core::SessionActorRef;
use moa_core::traits::{Identity, IdentityType};

use super::*;
use crate::services::llm_gateway::LLMGatewayClient;
use crate::services::narration::{
    NarrateSessionRequest, is_terminal_child_state, is_terminal_turn_phase,
};
use crate::vo::schedule_generation_guarded_self_call;

/// Registered Restate object name for the Session VO (the `#[restate_sdk::object]`
/// trait identifier), used for the untyped self-call target.
const SESSION_OBJECT_NAME: &str = "Session";
/// Handler name of the narration tick on the Session VO.
const NARRATION_TICK_HANDLER: &str = "narration_tick";
/// Stable source id for the active coordinator turn in the change cursor.
const COORDINATOR_SOURCE_ID: &str = "coordinator";

/// Schedules the first narration tick on an active edge, if none is outstanding.
///
/// Called when a coordinator turn starts or a child becomes active. The
/// single-outstanding flag guarantees overlapping active edges cannot fan out
/// multiple ticks; the generation is bumped so any lingering stale tick from a prior
/// active period is ignored when it fires. Mutates `state`; the caller persists.
pub(super) async fn ensure_narration_tick_scheduled(
    ctx: &ObjectContext<'_>,
    state: &mut SessionVoState,
) -> Result<(), HandlerError> {
    if state.narration_tick_outstanding {
        return Ok(());
    }
    let config = OrchestratorCtx::current_config();
    let limits = &config.session_limits;
    if !limits.progress_narration_enabled || limits.progress_narration_interval_ms == 0 {
        return Ok(());
    }
    let now = durable_utc_now(ctx).await?;
    state.narration_tick_generation = state.narration_tick_generation.wrapping_add(1);
    state.narration_tick_outstanding = true;
    send_narration_tick(
        ctx,
        state.narration_tick_generation,
        now,
        limits.progress_narration_interval_ms,
    );
    Ok(())
}

/// Runs one generation-guarded narration tick.
///
/// Stale ticks (superseded generation) no-op without rescheduling; a disabled
/// narrator or an idle session stops scheduling and clears the outstanding flag.
/// Otherwise the tick computes the change cursor from a bounded fan-in, applies the
/// cost gate, dispatches the detached narration job when the gate opens, and always
/// reschedules the next tick (same generation) while work remains active.
pub(super) async fn run_narration_tick(
    ctx: &ObjectContext<'_>,
    generation: u64,
) -> Result<(), HandlerError> {
    let mut state = SessionVoState::load_from(ctx).await?;

    // Stale: a newer generation now owns scheduling, so do not reschedule.
    if tick_is_stale(generation, state.narration_tick_generation) {
        return Ok(());
    }

    let config = OrchestratorCtx::current_config();
    let limits = &config.session_limits;
    if !limits.progress_narration_enabled {
        state.narration_tick_outstanding = false;
        state.persist_into(ctx);
        return Ok(());
    }

    let pending = load_pending_state(ctx).await?;
    let active_turn_id = pending.active_turn_id.clone();
    let has_active =
        active_turn_id.is_some() || state.children.iter().any(|child| child.terminal.is_none());
    if !has_active {
        // Nothing active: stop scheduling and clear the outstanding flag.
        state.narration_tick_outstanding = false;
        state.persist_into(ctx);
        return Ok(());
    }

    let sources =
        collect_active_marker_sources(ctx, &state.children, active_turn_id.as_deref()).await;
    let marker = compute_change_marker(&sources);
    let now = durable_utc_now(ctx).await?;

    let gate_limits = NarrationGateLimits {
        interval: chrono::Duration::milliseconds(limits.progress_narration_interval_ms as i64),
        window_len: narration_window_len(
            limits.progress_narration_interval_ms,
            limits.progress_narration_max_per_window,
        ),
        max_per_window: limits.progress_narration_max_per_window,
    };
    let outcome = evaluate_narration_gate(
        now,
        state.last_narration_at,
        gate_limits,
        &marker,
        state.last_narrated_marker.as_deref(),
        NarrationWindow {
            start: state.narration_window_start,
            count: state.narration_window_count,
        },
    );

    let mut window = outcome.window;
    if outcome.narrate {
        match narration_identity(&state) {
            Some(identity) => {
                state.narration_seq = state.narration_seq.wrapping_add(1);
                let session_id = parse_session_key(ctx.key())?;
                let request = NarrateSessionRequest {
                    session_id,
                    narration_seq: state.narration_seq,
                    identity,
                };
                // DETACHED: the LLM call must never block the single-writer VO, so the
                // narration job is `.send()` (never `.call()`).
                ctx.service_client::<LLMGatewayClient>()
                    .narrate_session(Json::from(request))
                    .send();
                window.count = window.count.saturating_add(1);
                state.last_narrated_marker = Some(marker);
                state.last_narration_at = Some(now);
                tracing::debug!(
                    key = %ctx.key(),
                    narration_seq = state.narration_seq,
                    "dispatched detached progress narration job"
                );
            }
            None => {
                tracing::warn!(
                    key = %ctx.key(),
                    "no owning identity available for narration; skipping dispatch this tick"
                );
            }
        }
    }
    state.narration_window_start = window.start;
    state.narration_window_count = window.count;

    // Always reschedule the next tick (same generation) while something is active.
    send_narration_tick(
        ctx,
        state.narration_tick_generation,
        now,
        limits.progress_narration_interval_ms,
    );
    state.persist_into(ctx);
    Ok(())
}

/// Issues one generation-guarded delayed self-call to `narration_tick`.
fn send_narration_tick(
    ctx: &ObjectContext<'_>,
    generation: u64,
    now: DateTime<Utc>,
    interval_ms: u64,
) {
    let delay = Duration::from_millis(interval_ms);
    let scheduled_for_millis =
        (now + chrono::Duration::milliseconds(interval_ms as i64)).timestamp_millis();
    schedule_generation_guarded_self_call(
        ctx,
        SESSION_OBJECT_NAME,
        NARRATION_TICK_HANDLER,
        generation,
        scheduled_for_millis,
        Json::from(NarrationTickRequest { generation }),
        delay,
    );
}

/// Whether a fired tick belongs to a superseded scheduling generation.
fn tick_is_stale(tick_generation: u64, current_generation: u64) -> bool {
    tick_generation != current_generation
}

/// Collects active, narratable `(source_id, summary)` pairs via the Increment 3a
/// bounded fan-in, used only to compute the change cursor.
///
/// Only non-terminal sources with a non-empty summary are returned. Terminal children
/// are skipped, active-child reads are capped by `MAX_SUB_AGENT_FAN_OUT`, and a failed
/// read is omitted rather than failing the tick.
async fn collect_active_marker_sources(
    ctx: &ObjectContext<'_>,
    children: &[SubAgentChildRef],
    active_turn_id: Option<&str>,
) -> Vec<(String, String)> {
    let mut sources = Vec::new();

    if let Some(turn_id) = active_turn_id {
        match ctx
            .workflow_client::<TurnExecutionClient>(turn_id.to_string())
            .progress()
            .call()
            .await
        {
            Ok(progress) => {
                let progress = progress.into_inner();
                if !is_terminal_turn_phase(&progress.phase)
                    && let Some(summary) =
                        usable_marker_summary(progress.last_progress_summary.as_deref())
                {
                    sources.push((COORDINATOR_SOURCE_ID.to_string(), summary));
                }
            }
            Err(error) => tracing::warn!(
                turn_id = %turn_id,
                error = %error,
                "narration marker: active turn progress unavailable; omitting"
            ),
        }
    }

    for item in plan_child_progress_fan_in(children, MAX_SUB_AGENT_FAN_OUT) {
        let ChildProgressFetch::Fetch(child_id) = item else {
            continue; // terminal children are not active sources
        };
        match ctx
            .object_client::<SubAgentClient>(child_id.clone())
            .progress_summary()
            .call()
            .await
        {
            Ok(summary) => {
                let summary = summary.into_inner();
                if !is_terminal_child_state(summary.state)
                    && let Some(text) = usable_marker_summary(summary.last_summary.as_deref())
                {
                    sources.push((child_id.to_string(), text));
                }
            }
            Err(error) => tracing::warn!(
                child_id = %child_id,
                error = %error,
                "narration marker: child progress summary unavailable; omitting"
            ),
        }
    }

    sources
}

/// Normalizes a summary into a trimmed, non-empty marker line.
fn usable_marker_summary(summary: Option<&str>) -> Option<String> {
    let trimmed = summary?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Computes a stable, order-independent change cursor over the active sources.
///
/// The marker hashes each active source's `(id, last_summary)` — the SEMANTIC content,
/// not heartbeats — so a source stuck on one long-running step with an unchanged
/// summary reads as "unchanged" and is not re-narrated, while any summary change moves
/// the cursor. Returns the empty string when nothing is narratable.
fn compute_change_marker(sources: &[(String, String)]) -> String {
    if sources.is_empty() {
        return String::new();
    }
    let mut canonical: Vec<(&str, &str)> = sources
        .iter()
        .map(|(id, summary)| (id.as_str(), summary.as_str()))
        .collect();
    canonical.sort_unstable();
    let mut buffer = String::new();
    for (id, summary) in canonical {
        buffer.push_str(id);
        buffer.push('\u{1f}');
        buffer.push_str(summary);
        buffer.push('\u{1e}');
    }
    format!("{:016x}", fnv1a64(buffer.as_bytes()))
}

/// Deterministic 64-bit FNV-1a hash for the persisted change cursor, so the marker is
/// stable across replays, worker restarts, and non-sticky routing (unlike a seeded
/// hasher).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Rolling per-window narration accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NarrationWindow {
    start: Option<DateTime<Utc>>,
    count: u32,
}

/// Pure outcome of the narration cost gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NarrationGateOutcome {
    /// Whether a narration should be dispatched this tick.
    narrate: bool,
    /// Window accounting after any rolling-window reset. The count is NOT yet
    /// incremented for this tick; the caller increments it only on an actual dispatch.
    window: NarrationWindow,
}

/// Cadence and cost limits applied by the narration gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NarrationGateLimits {
    /// Minimum spacing between narrations.
    interval: chrono::Duration,
    /// Rolling-window length for the per-window cap.
    window_len: chrono::Duration,
    /// Maximum narrations dispatched within one rolling window.
    max_per_window: u32,
}

/// Rolling-window length for the per-window narration cap.
///
/// There is no dedicated window-length config, so the window is sized to the steady
/// one-narration-per-interval cadence: `interval * max_per_window`. At steady churn
/// the cap is the natural ceiling; if ticks ever fire faster than the interval the cap
/// binds sooner, bounding narration cost.
fn narration_window_len(interval_ms: u64, max_per_window: u32) -> chrono::Duration {
    let window_ms = interval_ms
        .saturating_mul(u64::from(max_per_window))
        .min(i64::MAX as u64);
    chrono::Duration::milliseconds(window_ms as i64)
}

/// Pure narration cost gate.
///
/// Narrates only when (a) the interval has elapsed since the last narration, (b) the
/// change cursor moved to a non-empty marker, and (c) the rolling window is under its
/// cap (resetting the window when its length has elapsed).
fn evaluate_narration_gate(
    now: DateTime<Utc>,
    last_narration_at: Option<DateTime<Utc>>,
    limits: NarrationGateLimits,
    marker: &str,
    last_narrated_marker: Option<&str>,
    window: NarrationWindow,
) -> NarrationGateOutcome {
    // Reset the rolling window when it has never started or its length has elapsed.
    let window = match window.start {
        Some(start) if now.signed_duration_since(start) < limits.window_len => window,
        _ => NarrationWindow {
            start: Some(now),
            count: 0,
        },
    };

    let interval_elapsed = match last_narration_at {
        None => true,
        Some(last) => now.signed_duration_since(last) >= limits.interval,
    };
    let marker_changed = !marker.is_empty() && last_narrated_marker != Some(marker);
    let under_cap = window.count < limits.max_per_window;

    NarrationGateOutcome {
        narrate: interval_elapsed && marker_changed && under_cap,
        window,
    }
}

/// Resolves the identity used to authorize the self-originated narration read.
///
/// Prefers the owning participant identity captured from the first verified turn,
/// falling back to one derived from persisted session metadata.
fn narration_identity(state: &SessionVoState) -> Option<Identity> {
    if let Some(identity) = state.owning_identity.as_ref() {
        return Some(identity.clone());
    }
    owning_identity_from_meta(state.meta.as_ref()?)
}

/// Derives a session-participant identity from persisted session metadata.
///
/// Prefers the session's bound contact (which holds a direct `Participant` tuple on
/// the session), then the recorded creating identity. Returns `None` for anonymous or
/// unknown owners, in which case the self-originated narration read cannot be
/// authorized and narration is skipped — never a broad authz bypass.
fn owning_identity_from_meta(meta: &SessionMeta) -> Option<Identity> {
    if let Some(contact) = meta.contact.as_ref() {
        return Some(Identity {
            identity_type: IdentityType::Contact,
            id: contact.contact_id.0,
            tenant_id: meta.tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        });
    }
    match meta.created_by.as_ref()? {
        SessionActorRef::Identity { id } => Some(Identity {
            identity_type: IdentityType::User,
            id: *id,
            tenant_id: meta.tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        }),
        SessionActorRef::Contact { id } => Some(Identity {
            identity_type: IdentityType::Contact,
            id: id.0,
            tenant_id: meta.tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        }),
        SessionActorRef::Anonymous => None,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use moa_core::traits::IdentityType;
    use moa_core::{
        ContactId, ContactRef, ContactVerificationState, SessionActorRef, SessionMeta, TenantId,
    };
    use uuid::Uuid;

    use super::{
        NarrationGateLimits, NarrationWindow, compute_change_marker, evaluate_narration_gate,
        narration_window_len, owning_identity_from_meta, tick_is_stale,
    };

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).expect("valid timestamp")
    }

    fn gate_limits() -> NarrationGateLimits {
        NarrationGateLimits {
            interval: chrono::Duration::milliseconds(20_000),
            window_len: narration_window_len(20_000, 30),
            max_per_window: 30,
        }
    }

    #[test]
    fn narration_tick_stale_generation_is_ignored() {
        // Pins: a tick whose generation no longer matches the VO's current generation
        // is stale; an equal generation is live.
        assert!(tick_is_stale(5, 6));
        assert!(tick_is_stale(7, 6));
        assert!(!tick_is_stale(6, 6));
    }

    #[test]
    fn narration_gate_narrates_when_changed_and_interval_ok_and_under_cap() {
        // Pins: first narration with a fresh non-empty marker, interval satisfied, and
        // window under cap narrates (the caller then increments narration_seq).
        let base = ts(1_000_000);
        let outcome = evaluate_narration_gate(
            base,
            None,
            gate_limits(),
            "marker-a",
            None,
            NarrationWindow {
                start: None,
                count: 0,
            },
        );

        assert!(outcome.narrate);
        // Window opened at `now` with no count yet (the caller adds 1 on dispatch).
        assert_eq!(outcome.window.start, Some(base));
        assert_eq!(outcome.window.count, 0);
    }

    #[test]
    fn narration_gate_skips_when_marker_unchanged() {
        // Pins: an unchanged change cursor skips the LLM call even after the interval.
        let base = ts(1_000_000);
        let outcome = evaluate_narration_gate(
            base + chrono::Duration::seconds(60),
            Some(base),
            gate_limits(),
            "marker-a",
            Some("marker-a"),
            NarrationWindow {
                start: Some(base),
                count: 1,
            },
        );

        assert!(!outcome.narrate);
    }

    #[test]
    fn narration_gate_skips_when_interval_not_elapsed() {
        // Pins: a changed marker still skips when the narration interval has not elapsed.
        let base = ts(1_000_000);
        let outcome = evaluate_narration_gate(
            base + chrono::Duration::seconds(5),
            Some(base),
            gate_limits(),
            "marker-b",
            Some("marker-a"),
            NarrationWindow {
                start: Some(base),
                count: 1,
            },
        );

        assert!(!outcome.narrate);
    }

    #[test]
    fn narration_gate_skips_at_window_cap() {
        // Pins: reaching the per-window cap backs the narrator off within the window.
        let base = ts(1_000_000);
        let outcome = evaluate_narration_gate(
            base + chrono::Duration::seconds(60),
            Some(base),
            gate_limits(),
            "marker-b",
            Some("marker-a"),
            NarrationWindow {
                start: Some(base),
                count: 30,
            },
        );

        assert!(!outcome.narrate);
    }

    #[test]
    fn narration_gate_window_reset_reopens_cap() {
        // Pins: once the rolling window elapses the cap resets and narration resumes.
        let base = ts(1_000_000);
        let outcome = evaluate_narration_gate(
            base + chrono::Duration::seconds(601),
            Some(base),
            gate_limits(),
            "marker-b",
            Some("marker-a"),
            NarrationWindow {
                start: Some(base),
                count: 30,
            },
        );

        assert!(outcome.narrate);
        assert_eq!(outcome.window.count, 0);
        assert_eq!(
            outcome.window.start,
            Some(base + chrono::Duration::seconds(601))
        );
    }

    #[test]
    fn change_marker_is_order_independent_and_summary_sensitive() {
        // Pins: the change cursor depends on semantic (id, summary) content, not order,
        // so reordered active sources match but a summary change moves the cursor.
        let a = vec![
            ("c1".to_string(), "reading docs".to_string()),
            ("coordinator".to_string(), "drafting reply".to_string()),
        ];
        let reordered = vec![
            ("coordinator".to_string(), "drafting reply".to_string()),
            ("c1".to_string(), "reading docs".to_string()),
        ];
        let changed = vec![
            ("c1".to_string(), "reading docs".to_string()),
            ("coordinator".to_string(), "writing reply".to_string()),
        ];

        assert_eq!(compute_change_marker(&a), compute_change_marker(&reordered));
        assert_ne!(compute_change_marker(&a), compute_change_marker(&changed));
        assert!(compute_change_marker(&[]).is_empty());
    }

    #[test]
    fn owning_identity_prefers_contact_then_created_by_then_none() {
        // Pins: narration identity sourcing favors the bound contact, then the creating
        // identity, and yields nothing for anonymous-owned sessions.
        let tenant = TenantId::new();
        let contact_id = ContactId::new();
        let creator = Uuid::now_v7();

        let mut with_contact = SessionMeta {
            tenant_id: tenant,
            created_by: Some(SessionActorRef::Identity { id: creator }),
            ..SessionMeta::default()
        };
        with_contact.contact = Some(contact_ref(contact_id, tenant));
        let identity = owning_identity_from_meta(&with_contact).expect("contact identity");
        assert_eq!(identity.identity_type, IdentityType::Contact);
        assert_eq!(identity.id, contact_id.0);
        assert_eq!(identity.tenant_id, tenant);

        let created_by_only = SessionMeta {
            tenant_id: tenant,
            created_by: Some(SessionActorRef::Identity { id: creator }),
            ..SessionMeta::default()
        };
        let identity = owning_identity_from_meta(&created_by_only).expect("creator identity");
        assert_eq!(identity.identity_type, IdentityType::User);
        assert_eq!(identity.id, creator);

        let anonymous = SessionMeta {
            tenant_id: tenant,
            created_by: Some(SessionActorRef::Anonymous),
            ..SessionMeta::default()
        };
        assert!(owning_identity_from_meta(&anonymous).is_none());
    }

    fn contact_ref(contact_id: ContactId, tenant_id: TenantId) -> ContactRef {
        ContactRef {
            contact_id,
            tenant_id,
            state: ContactVerificationState::Unverified,
            canonical_contact_id: None,
            linked_contact_ids: Vec::new(),
            scopes: Vec::new(),
            permissions: serde_json::Value::Null,
            agent_ids: Vec::new(),
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        }
    }
}
