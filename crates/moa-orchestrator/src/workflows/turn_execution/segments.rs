//! Segment lifecycle transitions and deterministic outcome assessment.

use moa_brain::pipeline::segments::{SegmentCompleted, SegmentTracker};
use moa_brain::segment_assessment::AssessmentOverride;
use moa_brain::turn_segments::{
    assess_segment_events, latest_user_message, segment_boundary_sequences,
    segment_events_for_assessment, task_segment_from_active, task_segment_from_completed,
};
use moa_core::wire::session_store::{
    AppendEventRequest, CompleteSegmentRequest, CreateSegmentRequest,
    UpdateSegmentAssessmentRequest,
};
use moa_core::{
    events::Event, types::completion::CompletionRequest, types::events_stream::EventRecord,
    types::identifiers::SegmentId, types::identifiers::SessionId,
    types::query_rewrite::QueryRewriteResult, types::segment_assessment::AssessmentPhase,
    types::segments::ActiveSegment, types::segments::TaskSegment, types::session::SessionMeta,
};
use restate_sdk::prelude::*;

use super::TurnExecutionImpl;
use super::event_queries::{
    latest_event_cutoff_before_seq, load_next_user_message_cutoff, load_segment_assessment_events,
    load_segment_baseline, load_segment_boundary_events, load_session_events_fallback,
    load_session_meta,
};
use super::experience::{emit_experience_for_assessment, record_segment_assessment_learning};
use crate::services::session_store::RestateSessionStoreClient;
use crate::turn_driver::segments as driver_segments;
use crate::workflows::durable_utc_now;
use crate::workflows::turn_events::append_session_event;

#[derive(Clone, Debug)]
pub(super) struct PostOutcomeAssessment {
    meta: SessionMeta,
    segment: ActiveSegment,
    phase: AssessmentPhase,
    overrides: Vec<AssessmentOverride>,
    cutoff_before_seq: Option<u64>,
    duration_ms: u64,
    assessed_at: chrono::DateTime<chrono::Utc>,
    resolution_config: moa_core::config::ResolutionConfig,
}

enum SegmentAssessmentTarget<'a> {
    Completed(&'a SegmentCompleted),
    Active(&'a ActiveSegment),
}

struct SegmentAssessmentInput<'a> {
    target: SegmentAssessmentTarget<'a>,
    events: &'a [EventRecord],
    next_user_message: Option<&'a str>,
    rewrite: Option<&'a QueryRewriteResult>,
    phase: AssessmentPhase,
    overrides: &'a [AssessmentOverride],
    duration_ms: u64,
    assessed_at: chrono::DateTime<chrono::Utc>,
    resolution_config: &'a moa_core::config::ResolutionConfig,
}

impl SegmentAssessmentTarget<'_> {
    fn segment_id(&self) -> SegmentId {
        match self {
            Self::Completed(segment) => segment.segment_id,
            Self::Active(segment) => segment.id,
        }
    }

    fn turn_count(&self) -> u32 {
        match self {
            Self::Completed(segment) => segment.turn_count,
            Self::Active(segment) => segment.turn_count,
        }
    }

    fn token_cost(&self) -> u64 {
        match self {
            Self::Completed(segment) => segment.token_cost,
            Self::Active(segment) => segment.token_cost,
        }
    }

    fn task_segment(
        &self,
        meta: &SessionMeta,
        assessment: &moa_core::types::segment_assessment::SegmentAssessment,
        events: &[EventRecord],
    ) -> TaskSegment {
        match self {
            Self::Completed(segment) => {
                task_segment_from_completed(meta, segment, events, assessment)
            }
            Self::Active(segment) => task_segment_from_active(meta, segment, assessment, None),
        }
    }
}

fn tenant_key(meta: &SessionMeta) -> String {
    meta.tenant_id.to_string()
}

pub(super) async fn ensure_current_segment(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    request: &mut CompletionRequest,
) -> Result<Option<ActiveSegment>, HandlerError> {
    let active_segment = ctx
        .service_client::<RestateSessionStoreClient>()
        .get_active_segment(Json(session_id))
        .call()
        .await?
        .into_inner()
        .map(|segment| segment.active_view());

    let now = durable_utc_now(ctx, "workflow_utc_now").await?;
    let mut active_segment = active_segment;
    if let Some(transition) = SegmentTracker::transition_from_metadata(
        &request.metadata,
        session_id,
        &tenant_key(meta),
        &active_segment,
        now,
    ) {
        if let Some(completed) = transition.completed.clone() {
            ctx.service_client::<RestateSessionStoreClient>()
                .complete_segment(Json(CompleteSegmentRequest {
                    segment_id: completed.segment_id,
                    update: completed.update.clone(),
                }))
                .send();
            moa_core::coordination_counters::record_durable_append();
            ctx.service_client::<RestateSessionStoreClient>()
                .append_event(Json(AppendEventRequest {
                    session_id,
                    event: completed.clone().into_event(),
                    dedupe_key: None,
                }))
                .send();
            assess_completed_segment_at_transition(
                workflow,
                ctx,
                session_id,
                meta,
                &completed,
                &request.metadata,
            )
            .await?;
        }

        ctx.service_client::<RestateSessionStoreClient>()
            .create_segment(Json(CreateSegmentRequest {
                segment: transition.task_segment.clone(),
            }))
            .call()
            .await?;
        moa_core::coordination_counters::record_durable_append();
        ctx.service_client::<RestateSessionStoreClient>()
            .append_event(Json(AppendEventRequest {
                session_id,
                event: transition.started.clone().into_event(),
                dedupe_key: None,
            }))
            .send();

        active_segment = Some(transition.active_segment);
    }

    Ok(active_segment)
}

async fn assess_completed_segment_at_transition(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    completed: &SegmentCompleted,
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<(), HandlerError> {
    if !workflow.config.resolution.enabled {
        return Ok(());
    }

    let boundaries =
        load_segment_boundary_events(ctx, workflow.session_store.clone(), session_id).await?;
    let (segment_events, next_user_message) = if let Some(boundary) =
        segment_boundary_sequences(&boundaries, completed.segment_id)
    {
        let next_user = load_next_user_message_cutoff(
            ctx,
            workflow.session_store.clone(),
            session_id,
            boundary.start_seq,
        )
        .await?
        .map(|(text, sequence_num)| (Some(text), Some(sequence_num)))
        .unwrap_or((None, None));
        let events = load_segment_assessment_events(
            ctx,
            workflow.session_store.clone(),
            session_id,
            completed.segment_id,
            boundary,
            next_user.1,
            true,
        )
        .await?;
        (
            segment_events_for_assessment(&events, completed.segment_id, next_user.1),
            next_user.0,
        )
    } else {
        tracing::warn!(
            session_id = %session_id,
            segment_id = %completed.segment_id,
            "segment start event missing; falling back to full event log for completed segment assessment"
        );
        let events =
            load_session_events_fallback(ctx, workflow.session_store.clone(), session_id, None)
                .await?;
        let (next_user_message, next_user_seq) = latest_user_message(&events)
            .map(|(text, sequence_num)| (Some(text.to_string()), Some(sequence_num)))
            .unwrap_or((None, None));
        let segment_events =
            segment_events_for_assessment(&events, completed.segment_id, next_user_seq);
        (segment_events, next_user_message)
    };
    let rewrite = driver_segments::query_rewrite_from_metadata(metadata);
    let phase = if next_user_message.is_some() {
        AssessmentPhase::Deferred
    } else {
        AssessmentPhase::Immediate
    };
    let resolution_config = workflow.config.resolution.clone();
    assess_and_persist_segment(
        workflow,
        ctx,
        meta,
        SegmentAssessmentInput {
            target: SegmentAssessmentTarget::Completed(completed),
            events: &segment_events,
            next_user_message: next_user_message.as_deref(),
            rewrite: rewrite.as_ref(),
            phase,
            overrides: &[],
            duration_ms: completed.duration_ms,
            assessed_at: completed.update.ended_at,
            resolution_config: &resolution_config,
        },
    )
    .await?;
    Ok(())
}

pub(super) async fn capture_current_active_segment_assessment(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    phase: AssessmentPhase,
    overrides: &[AssessmentOverride],
    cutoff_before_seq: Option<u64>,
) -> Result<Option<PostOutcomeAssessment>, HandlerError> {
    if !workflow.config.resolution.enabled {
        return Ok(None);
    }

    let meta = load_session_meta(ctx, workflow.session_store.clone(), session_id).await?;
    let Some(segment) = ctx
        .service_client::<RestateSessionStoreClient>()
        .get_active_segment(Json(session_id))
        .call()
        .await?
        .into_inner()
        .map(|segment| segment.active_view())
    else {
        return Ok(None);
    };
    let assessed_at = durable_utc_now(ctx, "workflow_utc_now").await?;
    let duration_ms = assessed_at
        .signed_duration_since(segment.started_at)
        .num_milliseconds()
        .max(0) as u64;
    let cutoff_before_seq = match cutoff_before_seq {
        Some(sequence_num) => Some(sequence_num),
        None => {
            latest_event_cutoff_before_seq(ctx, workflow.session_store.clone(), session_id).await?
        }
    };
    Ok(Some(PostOutcomeAssessment {
        meta,
        segment,
        phase,
        overrides: overrides.to_vec(),
        cutoff_before_seq,
        duration_ms,
        assessed_at,
        resolution_config: workflow.config.resolution.clone(),
    }))
}

pub(super) async fn run_post_outcome_assessment(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    assessment: PostOutcomeAssessment,
) {
    let session_id = assessment.meta.id;
    if let Err(error) = persist_post_outcome_assessment(workflow, ctx, assessment).await {
        let error_text = format!("{error:?}");
        tracing::warn!(
            session_id = %session_id,
            error = %error_text,
            "post-outcome segment assessment failed"
        );
        if let Err(warning_error) = append_session_event(
            ctx,
            session_id,
            Event::Warning {
                message: format!("post-outcome segment assessment failed: {error_text}"),
            },
        )
        .await
        {
            tracing::warn!(
                session_id = %session_id,
                error = ?warning_error,
                "failed to append post-outcome assessment warning"
            );
        }
    }
}

async fn persist_post_outcome_assessment(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    assessment: PostOutcomeAssessment,
) -> Result<(), HandlerError> {
    let session_id = assessment.meta.id;
    let segment_id = assessment.segment.id;
    let boundaries =
        load_segment_boundary_events(ctx, workflow.session_store.clone(), session_id).await?;
    let segment_events = if let Some(boundary) = segment_boundary_sequences(&boundaries, segment_id)
    {
        let events = load_segment_assessment_events(
            ctx,
            workflow.session_store.clone(),
            session_id,
            segment_id,
            boundary,
            assessment.cutoff_before_seq,
            true,
        )
        .await?;
        segment_events_for_assessment(&events, segment_id, assessment.cutoff_before_seq)
    } else {
        tracing::warn!(
            session_id = %session_id,
            segment_id = %segment_id,
            "segment start event missing; falling back to bounded event log for post-outcome active segment assessment"
        );
        let events = load_session_events_fallback(
            ctx,
            workflow.session_store.clone(),
            session_id,
            assessment.cutoff_before_seq,
        )
        .await?;
        segment_events_for_assessment(&events, segment_id, assessment.cutoff_before_seq)
    };
    assess_and_persist_segment(
        workflow,
        ctx,
        &assessment.meta,
        SegmentAssessmentInput {
            target: SegmentAssessmentTarget::Active(&assessment.segment),
            events: &segment_events,
            next_user_message: None,
            rewrite: None,
            phase: assessment.phase,
            overrides: &assessment.overrides,
            duration_ms: assessment.duration_ms,
            assessed_at: assessment.assessed_at,
            resolution_config: &assessment.resolution_config,
        },
    )
    .await
}

async fn assess_and_persist_segment(
    workflow: &TurnExecutionImpl,
    ctx: &WorkflowContext<'_>,
    meta: &SessionMeta,
    input: SegmentAssessmentInput<'_>,
) -> Result<(), HandlerError> {
    let baseline = load_segment_baseline(ctx, meta.tenant_id).await?;
    let assessment = assess_segment_events(
        input.events,
        input.target.turn_count(),
        input.target.token_cost(),
        input.duration_ms,
        baseline.as_ref(),
        input.next_user_message,
        input.rewrite.is_some_and(|rewrite| rewrite.is_new_task),
        input.assessed_at,
        input.phase,
        input.overrides,
        input.resolution_config,
    );
    let segment_id = input.target.segment_id();
    record_segment_assessment_learning(workflow, ctx, meta.tenant_id, segment_id, &assessment)
        .await?;
    ctx.service_client::<RestateSessionStoreClient>()
        .update_segment_assessment(Json(UpdateSegmentAssessmentRequest {
            segment_id,
            assessment: assessment.clone(),
        }))
        .call()
        .await?;
    let task_segment = input.target.task_segment(meta, &assessment, input.events);
    emit_experience_for_assessment(
        workflow,
        ctx,
        meta,
        &task_segment,
        &assessment,
        input.events,
        input.rewrite,
        Some(input.duration_ms),
    )
    .await
}
