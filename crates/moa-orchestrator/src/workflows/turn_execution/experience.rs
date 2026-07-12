//! Best-effort experience persistence, weakness mining, and skill-learning dispatch.

use chrono::Utc;
use moa_brain::turn_learning::build_segment_learning_bundle;
use moa_core::{
    error::MoaError, events::Event, types::events_stream::EventRecord,
    types::identifiers::SegmentId, types::identifiers::SessionId,
    types::query_rewrite::QueryRewriteResult, types::segments::TaskSegment,
    types::session::SessionMeta,
};
use restate_sdk::prelude::*;

use super::TurnExecutionImpl;
use crate::turn_driver::learning as driver_learning;
use crate::workflows::durable_utc_now;
use crate::workflows::skill_learning::{RunSkillLearningRequest, SkillLearningClient};
use crate::workflows::turn_events::append_session_event;

#[allow(
    clippy::too_many_arguments,
    reason = "assessment evidence and the workflow implementation remain explicit at this persistence boundary"
)]
pub(super) async fn emit_experience_for_assessment(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    meta: &SessionMeta,
    segment: &TaskSegment,
    assessment: &moa_core::types::segment_assessment::SegmentAssessment,
    segment_events: &[EventRecord],
    rewrite: Option<&QueryRewriteResult>,
    duration_ms: Option<u64>,
) -> Result<(), HandlerError> {
    let now = durable_utc_now(ctx, "workflow_utc_now").await?;
    let learning = build_segment_learning_bundle(
        meta,
        segment,
        assessment,
        segment_events,
        rewrite,
        duration_ms,
        now,
    );
    let store = workflow.session_store.clone();
    let experience_id = learning.experience.id;
    let skill_learning_eligible = driver_learning::skill_learning_dispatch_is_eligible(
        segment_events,
        workflow.config.learning.skills.min_tool_calls,
        &learning.experience,
        &learning.attributions,
    );
    let learning_error = ctx
        .run(move || {
            let store = store.clone();
            let learning = learning.clone();
            async move {
                let result = async {
                    store.append_experience_record(&learning.experience).await?;
                    store
                        .append_experience_attributions(&learning.attributions)
                        .await?;
                    for candidate in &learning.candidates {
                        store.append_learning_candidate(candidate).await?;
                    }
                    Ok::<(), MoaError>(())
                }
                .await;
                Ok::<_, HandlerError>(Json::from(result.err().map(|error| error.to_string())))
            }
        })
        .name("emit_experience_learning")
        .await?
        .into_inner();

    if let Some(error) = learning_error {
        tracing::warn!(
            session_id = %meta.id,
            segment_id = %segment.id,
            error,
            "experience learning emission failed"
        );
        append_session_event(
            ctx,
            meta.id,
            Event::Warning {
                message: format!(
                    "experience learning emission failed for segment {}: {error}",
                    segment.id
                ),
            },
        )
        .await?;
        return Ok(());
    }
    if skill_learning_eligible {
        dispatch_skill_learning_after_experience(ctx, meta.id, experience_id).await?;
    }
    mine_segment_weakness_patterns(workflow, ctx, meta, segment_events).await?;
    Ok(())
}

/// Mines the segment's failure signals into reviewable weakness candidates.
///
/// Failure-driven learning counterpart to experience emission: deterministic,
/// model-free, and idempotent (candidate IDs derive from the tenant and
/// pattern key), so a mining failure only warns and never fails the turn.
async fn mine_segment_weakness_patterns(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    meta: &SessionMeta,
    segment_events: &[EventRecord],
) -> Result<(), HandlerError> {
    if moa_skills::mining::session_failures_from_events(segment_events).is_empty() {
        return Ok(());
    }
    let tenant_id = meta.tenant_id;
    let session_id = meta.id;
    let store = workflow.session_store.clone();
    let events = segment_events.to_vec();
    let mining_error = ctx
        .run(move || {
            let store = store.clone();
            let events = events.clone();
            async move {
                let result = moa_skills::mining::mine_and_file_session_failures(
                    store.as_ref(),
                    tenant_id,
                    &events,
                    Utc::now(),
                )
                .await;
                // Recorded inside the durable step so a workflow replay reuses the
                // journaled result instead of re-incrementing the counter.
                if let Ok(filed) = &result {
                    moa_observability::runtime_metrics::record_skill_learning_candidates_filed(
                        "mined",
                        "weakness",
                        *filed as u64,
                    );
                }
                Ok::<_, HandlerError>(Json::from(result.err().map(|error| error.to_string())))
            }
        })
        .name("mine_weakness_patterns")
        .await?
        .into_inner();
    if let Some(error) = mining_error {
        tracing::warn!(
            session_id = %session_id,
            error,
            "weakness mining failed; failure signals were not filed"
        );
    }
    Ok(())
}

async fn dispatch_skill_learning_after_experience(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    experience_id: uuid::Uuid,
) -> Result<(), HandlerError> {
    ctx.workflow_client::<SkillLearningClient>(experience_id.to_string())
        .run(Json(RunSkillLearningRequest {
            session_id,
            experience_id,
            // Single-session dispatch: the per-session gate is the sole evidence.
            recurrence: None,
        }))
        .send();
    tracing::debug!(
        session_id = %session_id,
        experience_id = %experience_id,
        "dispatched detached skill learning workflow"
    );
    Ok(())
}

pub(super) async fn record_segment_assessment_learning(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    tenant_id: moa_core::types::identifiers::TenantId,
    segment_id: SegmentId,
    assessment: &moa_core::types::segment_assessment::SegmentAssessment,
) -> Result<(), HandlerError> {
    let session_store = workflow.session_store.clone();
    let assessment = assessment.clone();
    ctx.run(|| async move {
        let entry = driver_learning::segment_assessment_learning_entry(
            driver_learning::SegmentAssessmentLearningRequest {
                id: uuid::Uuid::now_v7(),
                tenant_id,
                segment_id,
                assessment: &assessment,
                valid_from: Utc::now(),
            },
        )
        .map_err(HandlerError::from)?;
        session_store
            .append_learning(&entry)
            .await
            .map_err(HandlerError::from)
    })
    .name("record_segment_assessment_learning")
    .await?;
    Ok(())
}
