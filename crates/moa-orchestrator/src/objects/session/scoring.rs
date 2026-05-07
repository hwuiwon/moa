//! Resolution scoring helpers for session task segments.

use super::*;

pub(super) async fn score_completed_segment_at_transition(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    tenant_id: &str,
    completed: &moa_brain::pipeline::segments::SegmentCompleted,
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<(), HandlerError> {
    if !OrchestratorCtx::current().config.resolution.enabled {
        return Ok(());
    }

    let events = load_session_events(ctx, session_id).await?;
    let (next_user_message, next_user_seq) = latest_user_message(&events)
        .map(|(text, sequence_num)| (Some(text.to_string()), Some(sequence_num)))
        .unwrap_or((None, None));
    let segment_events = segment_events_for_scoring(&events, completed.segment_id, next_user_seq);
    let rewrite = query_rewrite_from_metadata(metadata);
    let baseline = load_segment_baseline(ctx, tenant_id, completed.intent_label.as_deref()).await?;
    let phase = if next_user_message.is_some() {
        ScoringPhase::Deferred
    } else {
        ScoringPhase::Immediate
    };
    let score = score_segment_events(
        &segment_events,
        completed.turn_count,
        completed.token_cost,
        completed.duration_ms,
        baseline.as_ref(),
        next_user_message.as_deref(),
        rewrite.as_ref().is_some_and(|rewrite| rewrite.is_new_task),
        phase,
        &[],
    );

    record_resolution_learning(ctx, tenant_id, completed.segment_id, &score).await?;
    ctx.service_client::<RestateSessionStoreClient>()
        .update_segment_resolution_score(Json(UpdateSegmentResolutionScoreRequest {
            segment_id: completed.segment_id,
            score,
        }))
        .send();
    Ok(())
}

pub(super) async fn score_active_segment(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    state: &SessionVoState,
    segment: &ActiveSegment,
    phase: ScoringPhase,
    overrides: &[ResolutionOverride],
) -> Result<(), HandlerError> {
    let runtime = OrchestratorCtx::current();
    if !runtime.config.resolution.enabled {
        return Ok(());
    }
    let tenant_id = state
        .meta
        .as_ref()
        .map(|meta| meta.workspace_id.as_str())
        .ok_or_else(|| TerminalError::new("session meta missing"))?;
    let events = load_session_events(ctx, session_id).await?;
    let segment_events = segment_events_for_scoring(&events, segment.id, None);
    let baseline = load_segment_baseline(ctx, tenant_id, segment.intent_label.as_deref()).await?;
    let duration_ms = Utc::now()
        .signed_duration_since(segment.started_at)
        .num_milliseconds()
        .max(0) as u64;
    let score = score_segment_events(
        &segment_events,
        segment.turn_count,
        segment.token_cost,
        duration_ms,
        baseline.as_ref(),
        None,
        false,
        phase,
        overrides,
    );

    record_resolution_learning(ctx, tenant_id, segment.id, &score).await?;
    ctx.service_client::<RestateSessionStoreClient>()
        .update_segment_resolution_score(Json(UpdateSegmentResolutionScoreRequest {
            segment_id: segment.id,
            score,
        }))
        .send();
    Ok(())
}

async fn record_resolution_learning(
    ctx: &ObjectContext<'_>,
    tenant_id: &str,
    segment_id: SegmentId,
    score: &moa_core::ResolutionScore,
) -> Result<(), HandlerError> {
    let session_store = OrchestratorCtx::current().session_store.clone();
    let tenant_id = tenant_id.to_string();
    let score = score.clone();
    ctx.run(|| async move {
        session_store
            .append_learning(&LearningEntry {
                id: uuid::Uuid::now_v7(),
                tenant_id,
                learning_type: "resolution_scored".to_string(),
                target_id: segment_id.to_string(),
                target_label: Some(score.label.as_str().to_string()),
                payload: serde_json::to_value(&score).map_err(|error| {
                    HandlerError::from(MoaError::StorageError(format!(
                        "serialize resolution score learning payload: {error}"
                    )))
                })?,
                confidence: Some(score.confidence),
                source_refs: vec![segment_id.0],
                actor: "system".to_string(),
                valid_from: Utc::now(),
                valid_to: None,
                batch_id: None,
                version: 1,
            })
            .await
            .map_err(HandlerError::from)
    })
    .name("record_resolution_learning")
    .await?;
    Ok(())
}

async fn load_session_events(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
) -> Result<Vec<EventRecord>, HandlerError> {
    Ok(ctx
        .service_client::<RestateSessionStoreClient>()
        .get_events(Json(GetEventsRequest {
            session_id,
            range: EventRange::all(),
        }))
        .call()
        .await?
        .into_inner())
}

async fn load_segment_baseline(
    ctx: &ObjectContext<'_>,
    tenant_id: &str,
    intent_label: Option<&str>,
) -> Result<Option<moa_core::SegmentBaseline>, HandlerError> {
    Ok(ctx
        .service_client::<RestateSessionStoreClient>()
        .get_segment_baseline(Json(GetSegmentBaselineRequest {
            tenant_id: tenant_id.to_string(),
            intent_label: intent_label.map(ToOwned::to_owned),
        }))
        .call()
        .await?
        .into_inner())
}

#[allow(clippy::too_many_arguments)]
fn score_segment_events(
    segment_events: &[EventRecord],
    turn_count: u32,
    token_cost: u64,
    duration_ms: u64,
    baseline: Option<&moa_core::SegmentBaseline>,
    next_user_message: Option<&str>,
    is_new_task: bool,
    phase: ScoringPhase,
    extra_overrides: &[ResolutionOverride],
) -> moa_core::ResolutionScore {
    let config = OrchestratorCtx::current().config.resolution.clone();
    let tool = tool_signal::score(segment_events);
    let verification = verification_signal::score(segment_events);
    let continuation = continuation_signal::score(
        continuation_signal::ContinuationInput {
            next_user_message,
            initial_query: first_user_message(segment_events),
            is_new_task,
        },
        config.rephrase_similarity_threshold,
    );
    let self_assessment = self_assessment_signal::score(last_brain_response(segment_events));
    let structural = structural_signal::score(
        structural_signal::SegmentMetrics {
            turn_count,
            token_cost,
            duration_secs: duration_ms as f64 / 1_000.0,
        },
        baseline,
        config.structural_min_samples,
    );
    let mut overrides = extra_overrides.to_vec();
    if let Some(override_value) = verification_signal::override_for_events(segment_events) {
        overrides.push(override_value);
    }
    if tool_signal::all_tools_failed(segment_events) {
        overrides.push(ResolutionOverride::AllToolsFailed);
    }

    ResolutionScorer::new(config.weights).score(
        tool,
        verification,
        continuation,
        self_assessment,
        structural,
        phase,
        &overrides,
    )
}

fn segment_events_for_scoring(
    events: &[EventRecord],
    segment_id: SegmentId,
    cutoff_before_seq: Option<u64>,
) -> Vec<EventRecord> {
    let start_seq = events.iter().find_map(|record| match &record.event {
        Event::SegmentStarted {
            segment_id: started_id,
            ..
        } if *started_id == segment_id => Some(record.sequence_num),
        _ => None,
    });
    let completed_seq = events.iter().find_map(|record| match &record.event {
        Event::SegmentCompleted {
            segment_id: completed_id,
            ..
        } if *completed_id == segment_id => Some(record.sequence_num),
        _ => None,
    });
    let end_exclusive = cutoff_before_seq
        .or_else(|| completed_seq.map(|sequence_num| sequence_num.saturating_add(1)));

    events
        .iter()
        .filter(|record| start_seq.is_none_or(|start_seq| record.sequence_num >= start_seq))
        .filter(|record| end_exclusive.is_none_or(|end_seq| record.sequence_num < end_seq))
        .cloned()
        .collect()
}

fn latest_user_message(events: &[EventRecord]) -> Option<(&str, u64)> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::UserMessage { text, .. } => Some((text.as_str(), record.sequence_num)),
        _ => None,
    })
}

fn first_user_message(events: &[EventRecord]) -> Option<&str> {
    events.iter().find_map(|record| match &record.event {
        Event::UserMessage { text, .. } => Some(text.as_str()),
        _ => None,
    })
}

fn last_brain_response(events: &[EventRecord]) -> Option<&str> {
    events.iter().rev().find_map(|record| match &record.event {
        Event::BrainResponse { text, .. } => Some(text.as_str()),
        _ => None,
    })
}

fn query_rewrite_from_metadata(
    metadata: &std::collections::HashMap<String, serde_json::Value>,
) -> Option<QueryRewriteResult> {
    metadata
        .get("query_rewrite")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}
