//! Replay-determinism coverage for Restate workflow durable steps.

// NONDETERMINISM AUDIT
//
// Consolidate:
// - `Instant::now` and `Utc::now` were previously consulted directly in `Consolidate::run`.
//   `Utc::now` now lives inside the journaled `now` durable step, so Restate captures the
//   first-run timestamp and replays it instead of consulting live time.
// - `Uuid::now_v7` and `Utc::now` in `record_memory_learning` are inside the journaled
//   `record_memory_learning` durable step. The current graph no-op report skips that step, but
//   the nondeterministic sources are still behind `ctx.run(...)` when it becomes active.
//

#[path = "../support/mod.rs"]
mod support;

use async_trait::async_trait;
use chrono::{Duration, TimeZone, Utc};
use moa_core::types::identifiers::TenantId;
use moa_memory_lifecycle::{
    BackfillStats, ConsolidationOutcome, DecayStats, DigestStats, ExpiryStats, MergeStats,
    SweepStats,
};
use moa_orchestrator::workflows::consolidate::{
    ConsolidateReport, ConsolidateRequest, ConsolidateSteps, run_consolidate_workflow,
};
use restate_sdk::prelude::HandlerError;
use serde_json::json;
use support::durable_step_recorder::{Recorder, assert_traces_identical};
use support::fake_clock::FakeClock;

struct RecordedConsolidateSteps<'a> {
    recorder: &'a mut Recorder,
    clock: FakeClock,
    duration_ms: u64,
    captured_changelog_version: i64,
}

#[async_trait]
impl ConsolidateSteps for RecordedConsolidateSteps<'_> {
    async fn mark_consolidation_started(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<(), HandlerError> {
        self.recorder.invoke(
            "Tenant",
            "mark_consolidation_started",
            &json!({
                "key": request.tenant_id.to_string(),
                "request": request.target_date,
            }),
            || (),
        );
        Ok(())
    }

    async fn capture_now(&mut self) -> Result<chrono::DateTime<Utc>, HandlerError> {
        Ok(self.recorder.run("now", &json!({}), || self.clock.now()))
    }

    async fn capture_current_changelog_version(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<i64, HandlerError> {
        Ok(self.recorder.run("capture_changelog_version", request, || {
            self.captured_changelog_version
        }))
    }

    async fn merge_duplicates(
        &mut self,
        request: &ConsolidateRequest,
        _now: chrono::DateTime<Utc>,
    ) -> Result<MergeStats, HandlerError> {
        Ok(self.recorder.run("merge", request, MergeStats::default))
    }

    async fn decay_confidence(
        &mut self,
        request: &ConsolidateRequest,
        _now: chrono::DateTime<Utc>,
    ) -> Result<DecayStats, HandlerError> {
        Ok(self.recorder.run("decay", request, DecayStats::default))
    }

    async fn sweep_contradictions(
        &mut self,
        request: &ConsolidateRequest,
        _now: chrono::DateTime<Utc>,
    ) -> Result<SweepStats, HandlerError> {
        Ok(self
            .recorder
            .run("contradict", request, SweepStats::default))
    }

    async fn expire_idle_facts(
        &mut self,
        request: &ConsolidateRequest,
        _now: chrono::DateTime<Utc>,
    ) -> Result<ExpiryStats, HandlerError> {
        Ok(self.recorder.run("expire", request, ExpiryStats::default))
    }

    async fn backfill_entities(
        &mut self,
        request: &ConsolidateRequest,
    ) -> Result<BackfillStats, HandlerError> {
        Ok(self
            .recorder
            .run("backfill", request, BackfillStats::default))
    }

    async fn rebuild_digests(
        &mut self,
        request: &ConsolidateRequest,
        _now: chrono::DateTime<Utc>,
    ) -> Result<DigestStats, HandlerError> {
        Ok(self.recorder.run("digest", request, DigestStats::default))
    }

    async fn build_consolidate_report(
        &mut self,
        request: &ConsolidateRequest,
        ran_at: chrono::DateTime<Utc>,
        outcome: ConsolidationOutcome,
    ) -> Result<ConsolidateReport, HandlerError> {
        Ok(self.recorder.run("report", request, || {
            ConsolidateReport::from_outcome(
                request.tenant_id,
                request.target_date,
                ran_at,
                self.duration_ms,
                outcome,
            )
        }))
    }

    async fn record_memory_learning(
        &mut self,
        report: &ConsolidateReport,
    ) -> Result<(), HandlerError> {
        if report.records_updated == 0
            && report.records_deleted == 0
            && report.relative_dates_normalized == 0
            && report.contradictions_resolved == 0
            && report.confidence_decayed == 0
            && report.duplicates_merged == 0
            && report.entity_embeddings_backfilled == 0
            && report.aliases_promoted == 0
            && report.digests_rebuilt == 0
            && report.errors.is_empty()
        {
            return Ok(());
        }
        self.recorder.run(
            "record_memory_learning",
            report,
            || json!({"recorded": true}),
        );
        Ok(())
    }

    async fn consolidation_completed(
        &mut self,
        report: &ConsolidateReport,
    ) -> Result<(), HandlerError> {
        self.recorder.invoke(
            "Tenant",
            "consolidation_completed",
            &json!({
                "key": report.tenant_id.to_string(),
                "request": report,
            }),
            || (),
        );
        Ok(())
    }

    async fn advance_consolidation_watermark(
        &mut self,
        request: &ConsolidateRequest,
        changelog_version: i64,
    ) -> Result<(), HandlerError> {
        self.recorder.run(
            "advance_consolidation_watermark",
            &json!({
                "tenant_id": request.tenant_id,
                "changelog_version": changelog_version,
            }),
            || json!({"advanced": true}),
        );
        Ok(())
    }
}

#[tokio::test]
async fn consolidate_workflow_first_run_and_replay_emit_identical_durable_steps_for_minimal_input()
{
    let request = ConsolidateRequest {
        tenant_id: tenant(1),
        target_date: chrono::NaiveDate::from_ymd_opt(2026, 5, 7).expect("valid target date"),
        observed_changelog_version: Some(42),
    };
    let clock = fixed_clock();

    let trace1 = run_consolidate_trace(Recorder::recording(), request.clone(), clock.clone()).await;
    clock.advance(Duration::hours(6));
    let trace2 = run_consolidate_trace(Recorder::replaying(trace1.clone()), request, clock).await;

    assert_traces_identical(&trace1, &trace2);
}

async fn run_consolidate_trace(
    recorder: Recorder,
    request: ConsolidateRequest,
    clock: FakeClock,
) -> Vec<support::durable_step_recorder::DurableStep> {
    let mut recorder = recorder;
    let mut steps = RecordedConsolidateSteps {
        recorder: &mut recorder,
        clock,
        duration_ms: 250,
        captured_changelog_version: 42,
    };
    run_consolidate_workflow(&mut steps, request)
        .await
        .expect("consolidate workflow should succeed");
    recorder.finish()
}

fn fixed_clock() -> FakeClock {
    FakeClock::new(
        Utc.with_ymd_and_hms(2026, 5, 7, 12, 0, 0)
            .single()
            .expect("valid fixed time"),
    )
}

fn tenant(value: u128) -> TenantId {
    TenantId::from(uuid::Uuid::from_u128(value))
}
