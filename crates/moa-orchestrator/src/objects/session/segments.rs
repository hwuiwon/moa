//! Task segment transition and classification helpers for session turns.

use super::*;

pub(super) async fn ensure_current_segment(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    request: &mut CompletionRequest,
) -> Result<(), HandlerError> {
    let mut state = SessionVoState::load_from(ctx).await?;
    let meta = state
        .meta
        .clone()
        .ok_or_else(|| TerminalError::new("session meta missing"))?;

    if state.current_segment.is_none()
        && let Some(segment) = ctx
            .service_client::<RestateSessionStoreClient>()
            .get_active_segment(Json(session_id))
            .call()
            .await?
            .into_inner()
    {
        state.current_segment = Some(segment.active_view());
    }

    if let Some(mut transition) = SegmentTracker::transition_from_metadata(
        &request.metadata,
        session_id,
        meta.workspace_id.as_str(),
        &state.current_segment,
        Utc::now(),
    ) {
        if let Some(completed) = transition.completed.clone() {
            ctx.service_client::<RestateSessionStoreClient>()
                .complete_segment(Json(CompleteSegmentRequest {
                    segment_id: completed.segment_id,
                    update: completed.update.clone(),
                }))
                .send();
            ctx.service_client::<RestateSessionStoreClient>()
                .append_event(Json(AppendEventRequest {
                    session_id,
                    event: completed.clone().into_event(),
                }))
                .send();
            score_completed_segment_at_transition(
                ctx,
                session_id,
                meta.workspace_id.as_str(),
                &completed,
                &request.metadata,
            )
            .await?;
        }

        classify_started_segment(ctx, meta.workspace_id.as_str(), request, &mut transition).await?;

        ctx.service_client::<RestateSessionStoreClient>()
            .create_segment(Json(CreateSegmentRequest {
                segment: transition.task_segment.clone(),
            }))
            .send();
        ctx.service_client::<RestateSessionStoreClient>()
            .append_event(Json(AppendEventRequest {
                session_id,
                event: transition.started.clone().into_event(),
            }))
            .send();

        state.set_current_segment(transition.active_segment);
        state.persist_into(ctx);
    }

    if let Some(segment) = state.current_segment.as_ref() {
        request.metadata.insert(
            "_moa.segment_id".to_string(),
            serde_json::json!(segment.id.to_string()),
        );
        request.metadata.insert(
            "_moa.segment_index".to_string(),
            serde_json::json!(segment.segment_index),
        );
    }

    Ok(())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct IntentClassification {
    label: String,
    confidence: f64,
}

async fn classify_started_segment(
    ctx: &ObjectContext<'_>,
    tenant_id: &str,
    request: &CompletionRequest,
    transition: &mut moa_brain::pipeline::segments::SegmentTransition,
) -> Result<(), HandlerError> {
    let runtime = OrchestratorCtx::current();
    if !runtime.config.intents.enabled {
        return Ok(());
    }
    let Some(embedding_provider) = runtime.embedding_provider.clone() else {
        return Ok(());
    };

    let session_store = runtime.session_store.clone();
    let threshold = runtime.config.intents.classification_threshold;
    let tenant_id = tenant_id.to_string();
    let task_summary = transition
        .task_segment
        .task_summary
        .clone()
        .unwrap_or_default();
    let first_user_message = user_message_for_intent(request).unwrap_or_default();
    let segment_id = transition.task_segment.id.0;

    let classification = ctx
        .run(|| async move {
            let classifier = IntentClassifier::with_threshold(
                session_store.clone(),
                embedding_provider,
                threshold,
            );
            let Some((intent, confidence)) = classifier
                .classify(&tenant_id, &task_summary, &first_user_message)
                .await
                .map_err(HandlerError::from)?
            else {
                return Ok(Json::from(None::<IntentClassification>));
            };

            session_store
                .append_learning(&LearningEntry {
                    id: uuid::Uuid::now_v7(),
                    tenant_id: tenant_id.clone(),
                    learning_type: "intent_classified".to_string(),
                    target_id: segment_id.to_string(),
                    target_label: Some(intent.label.clone()),
                    payload: serde_json::json!({
                        "intent_id": intent.id,
                        "task_summary": task_summary,
                        "first_user_message": first_user_message,
                    }),
                    confidence: Some(confidence),
                    source_refs: vec![segment_id],
                    actor: "system".to_string(),
                    valid_from: Utc::now(),
                    valid_to: None,
                    batch_id: None,
                    version: 1,
                })
                .await
                .map_err(HandlerError::from)?;

            Ok(Json::from(Some(IntentClassification {
                label: intent.label,
                confidence,
            })))
        })
        .name("classify_started_segment")
        .await?
        .into_inner();

    if let Some(classification) = classification {
        transition.task_segment.intent_label = Some(classification.label.clone());
        transition.task_segment.intent_confidence = Some(classification.confidence);
        transition.started.intent_label = Some(classification.label.clone());
        transition.started.intent_confidence = Some(classification.confidence);
        transition.active_segment.intent_label = Some(classification.label);
    }

    Ok(())
}

fn user_message_for_intent(request: &CompletionRequest) -> Option<String> {
    request
        .messages
        .iter()
        .find(|message| message.role == MessageRole::User)
        .map(|message| message.content.trim().to_string())
        .filter(|message| !message.is_empty())
}
