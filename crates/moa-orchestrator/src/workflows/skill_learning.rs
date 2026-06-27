//! Detached workflow for post-turn skill draft proposal generation.

use std::sync::Arc;

use moa_core::wire::session_store::AppendEventRequest;
use moa_core::{
    Event, EventRange, EventRecord, EventType, MoaConfig, MoaError, Result as MoaResult, SegmentId,
    SessionId, SessionStore as _,
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_providers::ModelRouter;
use moa_session::PostgresSessionStore;
use moa_skills::distiller::{
    ExperienceDistillationInput, SkillProposalGeneration,
    distill_skill_from_experience_with_learning, proposal_generation_from_distillation,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::services::session_store::RestateSessionStoreClient;

const FALLBACK_EVENT_TAIL_LIMIT: usize = 200;

/// Workflow request for one experience-backed skill-learning pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSkillLearningRequest {
    /// Session that produced the source experience.
    pub session_id: SessionId,
    /// Experience record to distill into a reviewable skill draft.
    pub experience_id: Uuid,
}

/// Serializable report returned by a skill-learning workflow run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillLearningReport {
    /// Session that produced the source experience.
    pub session_id: SessionId,
    /// Experience considered by this run.
    pub experience_id: Uuid,
    /// Stable outcome label for observability and tests.
    pub outcome: String,
    /// Human-readable skip or failure reason when available.
    pub message: Option<String>,
    /// Proposed learning-candidate ID when a draft was created.
    pub candidate_id: Option<Uuid>,
    /// Draft skill artifact revision created for review when available.
    pub draft_artifact_revision_uid: Option<Uuid>,
}

/// Restate workflow surface for one detached skill-learning pass.
#[restate_sdk::workflow]
pub trait SkillLearning {
    /// Runs one skill-learning workflow body.
    async fn run(
        request: Json<RunSkillLearningRequest>,
    ) -> Result<Json<SkillLearningReport>, HandlerError>;
}

/// Concrete `SkillLearning` workflow implementation.
pub struct SkillLearningImpl;

impl SkillLearning for SkillLearningImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<RunSkillLearningRequest>,
    ) -> Result<Json<SkillLearningReport>, HandlerError> {
        annotate_restate_handler_span("SkillLearning", "run");
        let request = request.into_inner();
        let runtime = OrchestratorCtx::current();
        let store = runtime.session_store_backend();
        let config = runtime.config().as_ref().clone();
        let router = match runtime
            .provider_registry()
            .model_router_for_config(&config)
            .map(Arc::new)
        {
            Ok(router) => router,
            Err(error) => {
                return Ok(failed_workflow_report(
                    &ctx,
                    &request,
                    format!("build skill learning model router: {error}"),
                )
                .await);
            }
        };

        let request_for_run = request.clone();
        let generation = ctx
            .run(move || async move {
                let report =
                    run_skill_learning_for_experience(&config, store, router, request_for_run)
                        .await
                        .map_err(HandlerError::from)?;
                Ok::<_, HandlerError>(Json::from(report))
            })
            .name("skill_learning_generate_proposal")
            .await;

        match generation {
            Ok(report) => Ok(report),
            Err(error) => Ok(failed_workflow_report(&ctx, &request, error.to_string()).await),
        }
    }
}

async fn failed_workflow_report(
    ctx: &WorkflowContext<'_>,
    request: &RunSkillLearningRequest,
    message: String,
) -> Json<SkillLearningReport> {
    tracing::warn!(
        session_id = %request.session_id,
        experience_id = %request.experience_id,
        error = %message,
        "skill learning proposal generation failed"
    );
    record_skill_learning_failure_from_workflow(
        ctx,
        request.session_id,
        request.experience_id,
        &message,
    )
    .await;
    Json::from(SkillLearningReport {
        session_id: request.session_id,
        experience_id: request.experience_id,
        outcome: "failed".to_string(),
        message: Some(message),
        candidate_id: None,
        draft_artifact_revision_uid: None,
    })
}

/// Runs skill-learning for one persisted experience using supplied runtime dependencies.
pub async fn run_skill_learning_for_experience(
    config: &MoaConfig,
    store: Arc<PostgresSessionStore>,
    model_router: Arc<ModelRouter>,
    request: RunSkillLearningRequest,
) -> MoaResult<SkillLearningReport> {
    let session = store.get_session(request.session_id).await?;
    let experience =
        load_experience_record(store.as_ref(), request.session_id, request.experience_id).await?;
    let attributions = store
        .list_experience_attributions(request.experience_id)
        .await?;
    let events =
        bounded_segment_events(store.as_ref(), request.session_id, experience.segment_id).await?;
    let tool_calls = events
        .iter()
        .filter(|record| matches!(record.event, Event::ToolCall { .. }))
        .count();
    if tool_calls < config.learning.skills.min_tool_calls {
        return Ok(skipped_report(
            request.session_id,
            request.experience_id,
            format!(
                "tool call count {tool_calls} below configured threshold {}",
                config.learning.skills.min_tool_calls
            ),
        ));
    }

    let outcome = distill_skill_from_experience_with_learning(
        config,
        &session,
        ExperienceDistillationInput {
            experience,
            attributions,
            events,
        },
        model_router,
        Some(store),
    )
    .await?;

    Ok(report_from_proposal_generation(
        request.session_id,
        request.experience_id,
        proposal_generation_from_distillation(outcome),
    ))
}

/// Appends the warning event used when detached skill-learning generation fails.
pub async fn record_skill_learning_failure(
    store: &PostgresSessionStore,
    session_id: SessionId,
    experience_id: Uuid,
    error: &str,
) -> MoaResult<EventRecord> {
    store
        .emit_event_record(
            session_id,
            Event::Warning {
                message: skill_learning_failure_message(experience_id, error),
            },
        )
        .await
}

async fn record_skill_learning_failure_from_workflow(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    experience_id: Uuid,
    error: &str,
) {
    let append = ctx
        .service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest {
            session_id,
            event: Event::Warning {
                message: skill_learning_failure_message(experience_id, error),
            },
        }))
        .call()
        .await;
    if let Err(warning_error) = append {
        tracing::warn!(
            session_id = %session_id,
            experience_id = %experience_id,
            error = ?warning_error,
            "failed to record skill learning warning event"
        );
    }
}

fn skill_learning_failure_message(experience_id: Uuid, error: &str) -> String {
    format!("skill learning proposal generation failed for experience {experience_id}: {error}")
}

async fn load_experience_record(
    store: &PostgresSessionStore,
    session_id: SessionId,
    experience_id: Uuid,
) -> MoaResult<moa_core::ExperienceRecord> {
    store
        .get_experience_record(session_id, experience_id)
        .await?
        .ok_or_else(|| {
            MoaError::StorageError(format!(
                "experience record {experience_id} not found for session {session_id}"
            ))
        })
}

async fn bounded_segment_events(
    store: &PostgresSessionStore,
    session_id: SessionId,
    segment_id: SegmentId,
) -> MoaResult<Vec<EventRecord>> {
    let boundaries = store
        .get_events(
            session_id,
            EventRange {
                event_types: Some(vec![EventType::SegmentStarted, EventType::SegmentCompleted]),
                ..EventRange::default()
            },
        )
        .await?;
    let start_seq = boundaries.iter().find_map(|record| match &record.event {
        Event::SegmentStarted {
            segment_id: started,
            ..
        } if *started == segment_id => Some(record.sequence_num),
        _ => None,
    });
    let completed_seq = boundaries.iter().find_map(|record| match &record.event {
        Event::SegmentCompleted {
            segment_id: completed,
            ..
        } if *completed == segment_id => Some(record.sequence_num),
        _ => None,
    });

    let range = match start_seq {
        Some(from_seq) => EventRange {
            from_seq: Some(from_seq),
            to_seq: completed_seq,
            ..EventRange::default()
        },
        None => EventRange::recent(FALLBACK_EVENT_TAIL_LIMIT),
    };
    store.get_events(session_id, range).await
}

fn report_from_proposal_generation(
    session_id: SessionId,
    experience_id: Uuid,
    outcome: SkillProposalGeneration,
) -> SkillLearningReport {
    match outcome {
        SkillProposalGeneration::Proposed {
            candidate_id,
            draft_artifact_revision_uid,
        } => SkillLearningReport {
            session_id,
            experience_id,
            outcome: "proposed".to_string(),
            message: None,
            candidate_id: Some(candidate_id),
            draft_artifact_revision_uid: Some(draft_artifact_revision_uid),
        },
        SkillProposalGeneration::Unchanged => skipped_report(
            session_id,
            experience_id,
            "existing skill did not need a draft",
        ),
        SkillProposalGeneration::Skipped { reason } => {
            skipped_report(session_id, experience_id, format!("{reason:?}"))
        }
    }
}

fn skipped_report(
    session_id: SessionId,
    experience_id: Uuid,
    message: impl Into<String>,
) -> SkillLearningReport {
    SkillLearningReport {
        session_id,
        experience_id,
        outcome: "skipped".to_string(),
        message: Some(message.into()),
        candidate_id: None,
        draft_artifact_revision_uid: None,
    }
}
