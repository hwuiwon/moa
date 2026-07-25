//! Segment lifecycle transitions and deterministic outcome assessment.

use moa_brain::pipeline::segments::{BoundaryFallbackInput, SegmentCompleted, SegmentTracker};
use moa_brain::query_rewrite::QueryRewriteResult;
use moa_brain::segment_assessment::AssessmentOverride;
use moa_brain::turn_segments::{
    assess_segment_events, latest_user_message, segment_boundary_sequences,
    segment_events_for_assessment, task_segment_from_active, task_segment_from_completed,
};
use moa_core::{
    events::Event, types::completion::CompletionRequest, types::events_stream::EventRecord,
    types::identifiers::SegmentId, types::identifiers::SessionId,
    types::segment_assessment::AssessmentPhase, types::segments::ActiveSegment,
    types::segments::TaskSegment, types::session::SessionMeta,
};
use moa_wire::session_store::{
    AppendEventRequest, CompleteSegmentRequest, CreateSegmentRequest,
    UpdateSegmentAssessmentRequest,
};
use restate_sdk::prelude::*;

use super::TurnExecutionImpl;
use super::event_queries::{
    latest_event_cutoff_before_seq, load_next_user_message_cutoff, load_recent_target_events,
    load_segment_assessment_events, load_segment_baseline, load_segment_boundary_events,
    load_session_events_fallback, load_session_meta,
};
use super::experience::{emit_experience_for_assessment, record_segment_assessment_learning};
use crate::services::session_store::RestateSessionStoreClient;
use crate::turn_driver::progress as driver_progress;
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
    resolution_config: moa_config::ResolutionConfig,
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
    resolution_config: &'a moa_config::ResolutionConfig,
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
    let active_segment = crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .get_active_segment(Json(session_id)),
    )
    .call()
    .await?
    .into_inner()
    .map(|segment| segment.active_view());

    let now = durable_utc_now(ctx, "workflow_utc_now").await?;
    let mut active_segment = active_segment;
    let boundary_config = workflow.config.learning.segments.clone();
    let fallback_owned = build_boundary_fallback(
        ctx,
        workflow,
        session_id,
        &active_segment,
        &request.metadata,
    )
    .await?;
    let fallback = fallback_owned
        .as_ref()
        .map(|owned| owned.as_input(&boundary_config));
    if let Some(transition) = SegmentTracker::transition_from_metadata(
        &request.metadata,
        session_id,
        &tenant_key(meta),
        &active_segment,
        now,
        fallback,
    ) {
        if let Some(completed) = transition.completed.clone() {
            crate::restate_identity::replay_safe_request(
                ctx.service_client::<RestateSessionStoreClient>()
                    .complete_segment(Json(CompleteSegmentRequest {
                        segment_id: completed.segment_id,
                        update: completed.update.clone(),
                    })),
            )
            .send();
            moa_core::coordination_counters::record_durable_append();
            crate::restate_identity::replay_safe_request(
                ctx.service_client::<RestateSessionStoreClient>()
                    .append_event(Json(AppendEventRequest {
                        session_id,
                        event: completed.clone().into_event(),
                        dedupe_key: None,
                    })),
            )
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

        crate::restate_identity::replay_safe_request(
            ctx.service_client::<RestateSessionStoreClient>()
                .create_segment(Json(CreateSegmentRequest {
                    segment: transition.task_segment.clone(),
                })),
        )
        .call()
        .await?;
        moa_core::coordination_counters::record_durable_append();
        crate::restate_identity::replay_safe_request(
            ctx.service_client::<RestateSessionStoreClient>()
                .append_event(Json(AppendEventRequest {
                    session_id,
                    event: transition.started.clone().into_event(),
                    dedupe_key: None,
                })),
        )
        .send();

        active_segment = Some(transition.active_segment);
    }

    Ok(active_segment)
}

/// Owned storage backing a [`BoundaryFallbackInput`]; the borrowed input is
/// rebuilt against a borrowed config just before the tracker call.
struct OwnedBoundaryFallback {
    user_message: String,
    previous_event_at: Option<chrono::DateTime<chrono::Utc>>,
    user_message_at: chrono::DateTime<chrono::Utc>,
}

impl OwnedBoundaryFallback {
    fn as_input<'a>(
        &'a self,
        config: &'a moa_config::SegmentBoundaryConfig,
    ) -> BoundaryFallbackInput<'a> {
        BoundaryFallbackInput {
            user_message: &self.user_message,
            previous_event_at: self.previous_event_at,
            user_message_at: self.user_message_at,
            config,
        }
    }
}

/// Gathers deterministic segment-boundary inputs for the fallback path.
///
/// Returns `None` (skipping the extra event load) when there is no active
/// segment, when the rewrite LLM already produced a boundary signal, or when the
/// active segment already began at/after the current user message — the last
/// guard prevents the marker/idle heuristics from re-firing across model-loop
/// iterations of the same user turn.
async fn build_boundary_fallback(
    ctx: &WorkflowContext<'_>,
    workflow: &TurnExecutionImpl,
    session_id: SessionId,
    active_segment: &Option<ActiveSegment>,
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<Option<OwnedBoundaryFallback>, HandlerError> {
    let Some(active) = active_segment.as_ref() else {
        return Ok(None);
    };
    if driver_segments::query_rewrite_from_metadata(metadata)
        .is_some_and(|rewrite| rewrite.has_boundary_signal)
    {
        return Ok(None);
    }
    let Some(user_sequence_num) = ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::USER_MESSAGE_SEQUENCE)
        .await?
        .map(Json::into_inner)
    else {
        return Ok(None);
    };

    let events = load_recent_target_events(ctx, workflow.session_store.clone(), session_id).await?;
    let Some((user_message, user_message_at)) = user_message_event(&events, user_sequence_num)
    else {
        return Ok(None);
    };
    if active.started_at >= user_message_at {
        return Ok(None);
    }

    Ok(Some(OwnedBoundaryFallback {
        user_message,
        previous_event_at: previous_event_timestamp(&events, user_sequence_num),
        user_message_at,
    }))
}

/// Extracts the raw text and timestamp of the user message at `user_sequence_num`.
fn user_message_event(
    events: &[EventRecord],
    user_sequence_num: u64,
) -> Option<(String, chrono::DateTime<chrono::Utc>)> {
    events
        .iter()
        .find(|record| record.sequence_num == user_sequence_num)
        .and_then(|record| match &record.event {
            Event::UserMessage { text, .. } => Some((text.clone(), record.timestamp)),
            _ => None,
        })
}

/// Returns the timestamp of the newest session event strictly before the current
/// user message, used to measure the idle gap.
fn previous_event_timestamp(
    events: &[EventRecord],
    user_sequence_num: u64,
) -> Option<chrono::DateTime<chrono::Utc>> {
    events
        .iter()
        .filter(|record| record.sequence_num < user_sequence_num)
        .map(|record| record.timestamp)
        .max()
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
    let Some(segment) = crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .get_active_segment(Json(session_id)),
    )
    .call()
    .await?
    .into_inner()
    .map(|segment| segment.active_view()) else {
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
            workflow.event_appender(),
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
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .update_segment_assessment(Json(UpdateSegmentAssessmentRequest {
                segment_id,
                assessment: assessment.clone(),
            })),
    )
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
