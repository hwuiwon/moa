//! Open-loop dispatcher, session pool, and result collection.
//!
//! The dispatcher walks a pre-computed arrival schedule and starts one turn
//! per arrival on an idle session from the pool. It never skips or delays
//! schedule entries because the target is slow: when every session is busy,
//! the arrival waits for the first idle session and the wait is charged to
//! coordinated-omission-corrected latency. Sessions that finish their plan
//! are finalized and replaced so the pool keeps its size while the schedule
//! is running.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use futures_util::StreamExt as _;
use rand::SeedableRng;
use rand::rngs::StdRng;

use crate::*;

/// Length of one report window.
const WINDOW_LEN: Duration = Duration::from_secs(10);
/// Concurrency for initial session-pool setup.
const SETUP_CONCURRENCY: usize = 16;

#[derive(Debug)]
pub(crate) struct TurnObservation {
    pub(crate) ttft: Option<Duration>,
    pub(crate) auto_denied_approvals: usize,
}

/// One session occupying a pool slot.
struct SessionSlot {
    target_index: usize,
    session_id: SessionId,
    plan: SessionPlan,
    next_turn: usize,
    last_seq: u64,
}

/// Messages funneled into the single-owner collector task.
enum CollectorMessage {
    SessionStarted,
    SessionSetupFailed,
    TurnCompleted {
        intended: Duration,
        dispatched: Duration,
        completed: Duration,
        ttft: Option<Duration>,
        tool_calls: u64,
        event_error_events: u64,
        tool_error_events: u64,
        auto_denied_approvals: usize,
        event_load_failed: bool,
    },
    TurnFailed {
        completed: Duration,
        kind: TurnFailureKind,
    },
    SessionFinished(SessionReport),
}

/// Aggregates owned by the collector task.
struct CollectorState {
    recorder: LatencyRecorder,
    errors: ErrorTaxonomy,
    turns_completed: u64,
    post_warmup_completions: u64,
    total_tool_calls: u64,
    auto_denied_approvals: usize,
    sessions_started: usize,
    sessions: Vec<SessionReport>,
}

/// Shared context captured by dispatcher-spawned turn tasks.
struct DispatchCtx {
    targets: Vec<Arc<dyn SessionTarget>>,
    pool: TenancyPool,
    collector_tx: mpsc::UnboundedSender<CollectorMessage>,
    idle_tx: mpsc::UnboundedSender<SessionSlot>,
    generating: AtomicBool,
    think_time: Duration,
    turn_timeout: Duration,
    run_start: tokio::time::Instant,
    session_ordinal: AtomicUsize,
    inspection_files: InspectionFiles,
    profile: SessionProfileKind,
    seed: u64,
}

impl DispatchCtx {
    fn elapsed(&self) -> Duration {
        self.run_start.elapsed()
    }

    /// Creates one new session on a Zipf-picked identity. Returns `None` and
    /// reports the setup failure when creation fails.
    async fn create_session(self: &Arc<Self>) -> Option<SessionSlot> {
        let ordinal = self.session_ordinal.fetch_add(1, Ordering::Relaxed);
        let mut rng = StdRng::seed_from_u64(self.seed ^ (ordinal as u64).wrapping_mul(0x9E37_79B9));
        let target_index = self.pool.pick_index(&mut rng);
        let plan = sampled_session_plan(ordinal, self.profile, &self.inspection_files, &mut rng);
        match self.targets[target_index].start_session(&plan).await {
            Ok(session_id) => {
                let _ = self.collector_tx.send(CollectorMessage::SessionStarted);
                Some(SessionSlot {
                    target_index,
                    session_id,
                    plan,
                    next_turn: 0,
                    last_seq: 0,
                })
            }
            Err(error) => {
                tracing::warn!(%error, "loadtest session setup failed");
                let _ = self.collector_tx.send(CollectorMessage::SessionSetupFailed);
                None
            }
        }
    }

    /// Finalizes a session into a `SessionReport`. `end_of_run` marks pool
    /// drain at schedule end, where an incomplete plan is expected and not a
    /// failure.
    async fn finalize_session(&self, slot: SessionSlot, failure: Option<String>, end_of_run: bool) {
        let target = &self.targets[slot.target_index];
        let completed_turns = slot.next_turn;
        let report = match target.session_meta(slot.session_id).await {
            Ok(meta) => {
                let status_failure = if end_of_run {
                    end_of_run_status_failure(&meta.status)
                } else {
                    session_status_failure_reason(
                        &meta.status,
                        completed_turns,
                        slot.plan.turns.len(),
                    )
                };
                let note = if failure.is_some() || status_failure.is_some() {
                    target
                        .recent_events(slot.session_id)
                        .await
                        .ok()
                        .and_then(|events| latest_session_note(&events))
                } else {
                    None
                };
                SessionReport {
                    session_id: slot.session_id,
                    profile: slot.plan.profile,
                    status: meta.status.clone(),
                    planned_turns: slot.plan.turns.len(),
                    completed_turns,
                    cache_hit_rate: meta.cache_hit_rate(),
                    total_cost_cents: meta.total_cost_cents as u64,
                    failure_reason: merge_failure_reason(failure, status_failure, note),
                }
            }
            Err(error) => SessionReport {
                session_id: slot.session_id,
                profile: slot.plan.profile,
                status: SessionStatus::Failed,
                planned_turns: slot.plan.turns.len(),
                completed_turns,
                cache_hit_rate: 0.0,
                total_cost_cents: 0,
                failure_reason: merge_failure_reason(
                    failure,
                    Some(format!("failed to load session metadata: {error}")),
                    None,
                ),
            },
        };
        let _ = self
            .collector_tx
            .send(CollectorMessage::SessionFinished(report));
    }

    /// Creates a replacement session and parks it in the idle queue while the
    /// schedule is still generating arrivals.
    async fn replace_session(self: &Arc<Self>) {
        if !self.generating.load(Ordering::Relaxed) {
            return;
        }
        if let Some(slot) = self.create_session().await {
            let _ = self.idle_tx.send(slot);
        }
    }
}

/// Runs the schedule against the target pool and returns the final report.
pub(crate) async fn run_sessions(
    targets: Vec<Arc<dyn SessionTarget>>,
    pool: TenancyPool,
    options: &LoadTestOptions,
    started: Instant,
) -> Result<LoadTestReport> {
    let schedule = build_arrival_offsets(
        options.rate_plan(),
        options.duration,
        options.arrival,
        options.seed,
    )?;
    let warmup = options.resolved_warmup();
    let recorder = LatencyRecorder::new(WINDOW_LEN, warmup)?;
    let inspection_files = inspectable_files(None).await?;
    let mut tenant_ids: Vec<Uuid> = pool
        .entries()
        .iter()
        .map(|entry| entry.tenant_id.0)
        .collect();
    tenant_ids.dedup();

    let (collector_tx, collector_rx) = mpsc::unbounded_channel();
    let (idle_tx, mut idle_rx) = mpsc::unbounded_channel();
    let collector = tokio::spawn(run_collector(collector_rx, recorder));

    let ctx = Arc::new(DispatchCtx {
        targets,
        pool,
        collector_tx,
        idle_tx,
        generating: AtomicBool::new(true),
        think_time: options.think_time,
        turn_timeout: options.turn_timeout,
        run_start: tokio::time::Instant::now(),
        session_ordinal: AtomicUsize::new(0),
        inspection_files,
        profile: options.profile,
        seed: options.seed,
    });

    // Fill the initial pool concurrently so setup cost does not eat into the
    // schedule.
    let setup = futures_util::stream::iter((0..options.sessions).map(|_| {
        let ctx = ctx.clone();
        async move {
            if let Some(slot) = ctx.create_session().await {
                let _ = ctx.idle_tx.send(slot);
                true
            } else {
                false
            }
        }
    }))
    .buffer_unordered(SETUP_CONCURRENCY)
    .collect::<Vec<bool>>()
    .await;
    let ready_sessions = setup.iter().filter(|ok| **ok).count();
    if ready_sessions == 0 {
        return Err(MoaError::ProviderError(
            "loadtest could not create any sessions; aborting".to_string(),
        ));
    }
    if ready_sessions < options.sessions {
        tracing::warn!(
            requested = options.sessions,
            ready = ready_sessions,
            "loadtest pool started degraded"
        );
    }

    // The schedule clock starts after setup: reset run_start-relative offsets
    // by re-anchoring the deadline base here.
    let schedule_base = tokio::time::Instant::now();
    let mut turn_tasks = tokio::task::JoinSet::new();
    for offset in &schedule {
        tokio::time::sleep_until(schedule_base + *offset).await;
        let Some(slot) = idle_rx.recv().await else {
            break;
        };
        let intended = *offset + (schedule_base - ctx.run_start);
        let ctx = ctx.clone();
        turn_tasks.spawn(run_one_turn(ctx, slot, intended));
    }
    ctx.generating.store(false, Ordering::Relaxed);
    while turn_tasks.join_next().await.is_some() {}

    // Drain sessions still parked in the pool; their plans were cut short by
    // the end of the schedule, which is expected.
    while let Ok(slot) = idle_rx.try_recv() {
        ctx.finalize_session(slot, None, true).await;
    }
    drop(ctx);

    let state = collector
        .await
        .map_err(|error| MoaError::ProviderError(format!("collector task panicked: {error}")))?;

    Ok(build_report(
        options, started, &schedule, warmup, tenant_ids, state,
    ))
}

/// Executes one scheduled turn on one session slot.
async fn run_one_turn(ctx: Arc<DispatchCtx>, mut slot: SessionSlot, intended: Duration) {
    let dispatched = ctx.elapsed();
    let target = ctx.targets[slot.target_index].clone();
    let prompt = slot.plan.turns[slot.next_turn].prompt.clone();
    match target
        .run_turn(slot.session_id, &prompt, ctx.turn_timeout)
        .await
    {
        Ok(observation) => {
            // The turn is complete the moment the target reports it; the
            // event fetch below is harness bookkeeping and must not count
            // toward measured latency.
            let completed = ctx.elapsed();
            let mut tool_calls = 0u64;
            let mut event_error_events = 0u64;
            let mut tool_error_events = 0u64;
            let mut event_load_failed = false;
            match target
                .session_events_since(slot.session_id, slot.last_seq)
                .await
            {
                Ok(events) => {
                    for record in events {
                        slot.last_seq = slot.last_seq.max(record.sequence_num);
                        match &record.event {
                            Event::ToolCall { .. } => tool_calls += 1,
                            Event::ToolError { error, .. }
                                if !is_expected_harness_denial(error) =>
                            {
                                tool_error_events += 1;
                            }
                            Event::Error { .. } => event_error_events += 1,
                            _ => {}
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, session = %slot.session_id, "event load failed");
                    event_load_failed = true;
                }
            }
            let _ = ctx.collector_tx.send(CollectorMessage::TurnCompleted {
                intended,
                dispatched,
                completed,
                ttft: observation.ttft,
                tool_calls,
                event_error_events,
                tool_error_events,
                auto_denied_approvals: observation.auto_denied_approvals,
                event_load_failed,
            });
            slot.next_turn += 1;
            if slot.next_turn < slot.plan.turns.len() {
                let think =
                    sampled_think_time(ctx.think_time, ctx.seed, slot.session_id, slot.next_turn);
                if !think.is_zero() {
                    tokio::time::sleep(think).await;
                }
                let _ = ctx.idle_tx.send(slot);
            } else {
                ctx.finalize_session(slot, None, false).await;
                ctx.replace_session().await;
            }
        }
        Err(failure) => {
            let completed = ctx.elapsed();
            let _ = ctx.collector_tx.send(CollectorMessage::TurnFailed {
                completed,
                kind: failure.kind,
            });
            let turn_number = slot.next_turn + 1;
            ctx.finalize_session(
                slot,
                Some(format!("turn {turn_number} failed: {failure}")),
                false,
            )
            .await;
            ctx.replace_session().await;
        }
    }
}

/// Single-owner aggregation loop.
async fn run_collector(
    mut collector_rx: mpsc::UnboundedReceiver<CollectorMessage>,
    recorder: LatencyRecorder,
) -> CollectorState {
    let mut state = CollectorState {
        recorder,
        errors: ErrorTaxonomy::default(),
        turns_completed: 0,
        post_warmup_completions: 0,
        total_tool_calls: 0,
        auto_denied_approvals: 0,
        sessions_started: 0,
        sessions: Vec::new(),
    };
    let warmup = state.recorder_warmup();
    while let Some(message) = collector_rx.recv().await {
        match message {
            CollectorMessage::SessionStarted => state.sessions_started += 1,
            CollectorMessage::SessionSetupFailed => state.errors.session_setup_failures += 1,
            CollectorMessage::TurnCompleted {
                intended,
                dispatched,
                completed,
                ttft,
                tool_calls,
                event_error_events,
                tool_error_events,
                auto_denied_approvals,
                event_load_failed,
            } => {
                state.turns_completed += 1;
                if completed >= warmup {
                    state.post_warmup_completions += 1;
                }
                state.total_tool_calls += tool_calls;
                state.errors.event_error_events += event_error_events;
                state.errors.tool_error_events += tool_error_events;
                state.auto_denied_approvals += auto_denied_approvals;
                if event_load_failed {
                    state.errors.event_load_failures += 1;
                }
                if let Err(error) = state
                    .recorder
                    .record_turn(intended, dispatched, completed, ttft)
                {
                    tracing::warn!(%error, "latency recording failed");
                }
            }
            CollectorMessage::TurnFailed { completed, kind } => {
                match kind {
                    TurnFailureKind::StartFailed => state.errors.turn_start_failures += 1,
                    TurnFailureKind::Timeout => state.errors.turn_timeouts += 1,
                    TurnFailureKind::Failed | TurnFailureKind::Transport => {
                        state.errors.turn_failures += 1;
                    }
                    TurnFailureKind::Cancelled => state.errors.turn_cancellations += 1,
                }
                if let Err(error) = state.recorder.record_turn_error(completed) {
                    tracing::warn!(%error, "error recording failed");
                }
            }
            CollectorMessage::SessionFinished(report) => state.sessions.push(report),
        }
    }
    state
}

impl CollectorState {
    fn recorder_warmup(&self) -> Duration {
        self.recorder.warmup()
    }
}

/// Assembles the final report from collector state.
fn build_report(
    options: &LoadTestOptions,
    started: Instant,
    schedule: &[Duration],
    warmup: Duration,
    tenant_ids: Vec<Uuid>,
    mut state: CollectorState,
) -> LoadTestReport {
    state
        .sessions
        .sort_by_key(|session| session.session_id.to_string());
    let sessions_failed = state
        .sessions
        .iter()
        .filter(|session| session.failure_reason.is_some())
        .count();
    let sessions_completed = state.sessions.len().saturating_sub(sessions_failed);
    let cache_samples = state
        .sessions
        .iter()
        .map(|session| session.cache_hit_rate)
        .collect::<Vec<_>>();
    let total_cost_cents = state
        .sessions
        .iter()
        .map(|session| session.total_cost_cents)
        .sum();
    let measure_window = options
        .duration
        .saturating_sub(warmup)
        .as_secs_f64()
        .max(f64::MIN_POSITIVE);

    LoadTestReport {
        mode: options.mode,
        endpoint: options.endpoint.clone(),
        profile: options.profile,
        requested_rate_qps: options.rate,
        achieved_rate_qps: state.post_warmup_completions as f64 / measure_window,
        sessions_started: state.sessions_started,
        sessions_completed,
        sessions_failed,
        turns_scheduled: schedule.len() as u64,
        turns_completed: state.turns_completed,
        errors: state.errors,
        total_tool_calls: state.total_tool_calls as usize,
        auto_denied_approvals: state.auto_denied_approvals,
        duration_ms: started.elapsed().as_secs_f64() * 1_000.0,
        warmup_ms: warmup.as_secs_f64() * 1_000.0,
        turn_latency_corrected_ms: state.recorder.corrected_summary(),
        turn_latency_ms: state.recorder.uncorrected_summary(),
        dispatch_delay_ms: state.recorder.dispatch_delay_summary(),
        ttft_ms: state.recorder.ttft_summary(),
        step_latency_ms: Vec::new(),
        cache_hit_rate: summarize_percentiles(&cache_samples),
        total_cost_cents,
        windows: state.recorder.window_reports(),
        tenant_ids,
        hdr: state
            .recorder
            .serialized()
            .map_err(|error| tracing::warn!(%error, "histogram serialization failed"))
            .ok(),
        sessions: state.sessions,
    }
}

/// Samples an exponentially distributed think time with the configured mean,
/// capped at 5x to bound stragglers. Deterministic per (seed, session, turn).
fn sampled_think_time(mean: Duration, seed: u64, session_id: SessionId, turn: usize) -> Duration {
    use rand::Rng as _;

    if mean.is_zero() {
        return mean;
    }
    let session_bits = session_id.0.as_u128() as u64;
    let mut rng = StdRng::seed_from_u64(seed ^ session_bits ^ ((turn as u64) << 48));
    let uniform: f64 = rng.gen_range(f64::MIN_POSITIVE..1.0);
    mean.mul_f64(-uniform.ln()).min(mean * 5)
}

/// Failure classification for sessions cut short by the end of the schedule.
fn end_of_run_status_failure(status: &SessionStatus) -> Option<String> {
    match status {
        SessionStatus::Failed | SessionStatus::Cancelled => {
            Some(format!("session ended in status {status:?}"))
        }
        _ => None,
    }
}

fn session_status_failure_reason(
    status: &SessionStatus,
    completed_turns: usize,
    planned_turns: usize,
) -> Option<String> {
    match status {
        SessionStatus::Failed | SessionStatus::Cancelled => {
            Some(format!("session ended in status {status:?}"))
        }
        SessionStatus::Paused if completed_turns < planned_turns => {
            Some(format!("session ended in status {status:?}"))
        }
        _ => None,
    }
}

pub(crate) fn latest_session_note(events: &[EventRecord]) -> Option<String> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::Warning { message } => Some(message.clone()),
        Event::Error { message, .. } => Some(message.clone()),
        _ => None,
    })
}

pub(crate) fn is_expected_harness_denial(message: &str) -> bool {
    message.contains("auto-denied by moa-loadtest")
}

pub(crate) fn merge_failure_reason(
    primary: Option<String>,
    secondary: Option<String>,
    note: Option<String>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(primary) = primary {
        parts.push(primary);
    }
    if let Some(secondary) = secondary
        && !parts.iter().any(|existing| existing == &secondary)
    {
        parts.push(secondary);
    }
    if let Some(note) = note
        && !parts.iter().any(|existing| existing == &note)
    {
        parts.push(format!("session note: {note}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" | "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paused_after_all_planned_turns_is_success_status() {
        // Pins: a Restate Session parked in Paused after all planned turns is a successful idle session.
        assert_eq!(
            session_status_failure_reason(&SessionStatus::Paused, 5, 5),
            None
        );
    }

    #[test]
    fn paused_before_all_planned_turns_is_failure_status() {
        // Pins: Paused is still a failure when the remote turn loop stopped before the plan completed.
        assert_eq!(
            session_status_failure_reason(&SessionStatus::Paused, 4, 5),
            Some("session ended in status Paused".to_string())
        );
    }

    #[test]
    fn failed_status_is_always_failure_status() {
        // Pins: failed remote sessions are never reclassified as success by completed-turn accounting.
        assert_eq!(
            session_status_failure_reason(&SessionStatus::Failed, 5, 5),
            Some("session ended in status Failed".to_string())
        );
    }

    #[test]
    fn cancelled_status_is_failure_status_even_after_all_planned_turns() {
        // Pins: a remote session cancelled mid-load is a failure regardless of turn progress.
        assert_eq!(
            session_status_failure_reason(&SessionStatus::Cancelled, 5, 5),
            Some("session ended in status Cancelled".to_string())
        );
    }

    #[test]
    fn end_of_run_drain_treats_incomplete_paused_sessions_as_healthy() {
        // Pins: sessions cut short because the schedule ended are not failures;
        // only Failed/Cancelled statuses count during pool drain.
        assert_eq!(end_of_run_status_failure(&SessionStatus::Paused), None);
        assert_eq!(end_of_run_status_failure(&SessionStatus::Running), None);
        assert!(end_of_run_status_failure(&SessionStatus::Failed).is_some());
        assert!(end_of_run_status_failure(&SessionStatus::Cancelled).is_some());
    }

    #[test]
    fn merge_failure_reason_drops_segments_duplicating_an_existing_part() {
        // Pins: identical primary/secondary/note strings collapse to one segment instead of
        // repeating the same reason in the merged failure string.
        let merged = merge_failure_reason(
            Some("session ended in status Failed".to_string()),
            Some("session ended in status Failed".to_string()),
            Some("session ended in status Failed".to_string()),
        );

        assert_eq!(merged, Some("session ended in status Failed".to_string()));
    }

    #[test]
    fn merge_failure_reason_joins_distinct_parts_with_note_prefix() {
        // Pins: distinct reasons are joined in order and the session note keeps its prefix.
        let merged = merge_failure_reason(
            Some("primary failure".to_string()),
            Some("secondary failure".to_string()),
            Some("worker drained".to_string()),
        );

        assert_eq!(
            merged,
            Some("primary failure | secondary failure | session note: worker drained".to_string())
        );
    }

    #[test]
    fn merge_failure_reason_is_none_when_no_parts_present() {
        // Pins: a fully-successful session produces no synthesized failure reason.
        assert_eq!(merge_failure_reason(None, None, None), None);
    }
}
