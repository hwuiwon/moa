//! Detached workflow for post-turn skill draft proposal generation.

use std::sync::Arc;

use moa_config::MoaConfig;
use moa_core::{
    error::MoaError, error::Result as MoaResult, events::Event, events::EventType,
    traits::EmbeddingProvider, traits::SessionStore as _, types::events_stream::EventRange,
    types::events_stream::EventRecord, types::identifiers::SegmentId,
    types::identifiers::SessionId,
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_providers::{ModelRouter, ProviderRegistry};
use moa_session::PostgresSessionStore;
use moa_skills::distiller::{
    DispatchEvidence, DistillationOutcome, ExperienceDistillationInput, RecurrenceEvidence,
    SkillProposalGeneration, distill_skill_from_experience_with_learning,
    proposal_generation_from_distillation,
};
use moa_skills::evidence::{
    EvidenceScope, SanitizedLearningEvidence, SegmentNarrative, sanitize_segment_evidence,
};
use moa_skills::proposals::{
    RecurrenceSiblingSuite, SiblingResynthesis, SkillDraftProposal, accumulate_recurrence_siblings,
};
use moa_wire::session_store::AppendEventRequest;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::services::session_store::RestateSessionStoreClient;

const FALLBACK_EVENT_TAIL_LIMIT: usize = 200;

/// Workflow request for one experience-backed skill-learning pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSkillLearningRequest {
    /// Session that produced the source (exemplar) experience.
    pub session_id: SessionId,
    /// Experience record to distill into a reviewable skill draft.
    pub experience_id: Uuid,
    /// Recurrence context when this pass was dispatched by the recurrence cron
    /// rather than a single session clearing the dispatch gate. `None` keeps the
    /// single-session behavior: the configured tool-call floor and no siblings.
    #[serde(default)]
    pub recurrence: Option<RecurrenceDispatch>,
}

/// Recurrence context threaded from the recurrence cron into a learning pass.
///
/// The exemplar (the workflow's own `experience_id`) is distilled with the
/// relaxed floor and recurrence evidence; `siblings` are the other cluster
/// members fed into sibling accumulation so the draft generalizes immediately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecurrenceDispatch {
    /// Total resolved/partial occurrences of the fingerprint in the window.
    pub occurrences: usize,
    /// Every exact task fingerprint merged into this recurrence cluster. More
    /// than one entry means semantic clustering pooled differently-worded groups.
    #[serde(default)]
    pub merged_fingerprints: Vec<String>,
    /// Earliest observed occurrence in the cluster.
    pub first_seen: chrono::DateTime<chrono::Utc>,
    /// Latest observed occurrence in the cluster.
    pub last_seen: chrono::DateTime<chrono::Utc>,
    /// Other cluster members to accumulate as siblings after the exemplar files.
    pub siblings: Vec<RecurrenceSiblingRef>,
}

/// One recurrence cluster sibling: the experience and the session that owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecurrenceSiblingRef {
    /// Session that produced the sibling experience.
    pub session_id: SessionId,
    /// Sibling experience record identifier.
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
#[derive(Clone)]
pub struct SkillLearningImpl {
    session_store: Arc<PostgresSessionStore>,
    config: Arc<MoaConfig>,
    providers: Arc<ProviderRegistry>,
    /// Classifier used to sanitize segment transcripts before distillation.
    ///
    /// The deterministic local heuristic, the same one lineage capture uses, so
    /// the durable step stays synchronous and free of network IO.
    classifier: Arc<dyn moa_memory_pii::PiiClassifier>,
    /// Tenant embedder reused for the semantic (R2) filing-time routing/dedup.
    /// `None` when the configured vector embedder is disabled or its credential is
    /// missing; the distiller then skips the semantic layer and the lexical path
    /// stands in. Built once so the provider's pacer and limiter are shared.
    embedder: Option<Arc<dyn EmbeddingProvider>>,
}

impl SkillLearningImpl {
    /// Creates a skill-learning workflow with its storage, config, and provider dependencies.
    #[must_use]
    pub fn new(
        session_store: Arc<PostgresSessionStore>,
        config: Arc<MoaConfig>,
        providers: Arc<ProviderRegistry>,
    ) -> Self {
        let embedder = build_learning_embedder(&config);
        Self {
            session_store,
            config,
            providers,
            classifier: Arc::new(moa_memory_pii::HeuristicPiiClassifier),
            embedder,
        }
    }

    /// Replaces the sanitization classifier.
    ///
    /// Exists so a workflow test can drive the gate with an abstaining, failing,
    /// or invalid-span classifier and observe that distillation makes zero
    /// provider calls and writes nothing.
    #[must_use]
    pub fn with_classifier(mut self, classifier: Arc<dyn moa_memory_pii::PiiClassifier>) -> Self {
        self.classifier = classifier;
        self
    }
}

/// Builds the tenant embedder used by the semantic filing-time routing/dedup.
///
/// Reuses the same `memory.vector.embedder` selector and 1024-dim output as the
/// graph-memory index and the embedding-backfill cron, so a probe shares the
/// vector space the stored task/skill embeddings live in. A disabled selector or
/// a missing credential is not fatal: it disables the semantic layer and logs a
/// warning, exactly like the backfill cron.
fn build_learning_embedder(config: &MoaConfig) -> Option<Arc<dyn EmbeddingProvider>> {
    match moa_providers::embedding::build_embedder_from_config(
        config,
        moa_providers::EmbedderConstructionRole::Ingestion,
    ) {
        Ok(embedder) => Some(embedder),
        Err(error) => {
            tracing::warn!(
                %error,
                "skill-learning semantic routing disabled: tenant embedder unavailable"
            );
            None
        }
    }
}

impl SkillLearning for SkillLearningImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<RunSkillLearningRequest>,
    ) -> Result<Json<SkillLearningReport>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("SkillLearning", "run");
        let request = request.into_inner();
        let store = self.session_store.clone();
        let config = self.config.as_ref().clone();
        let router = match self
            .providers
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
        let embedder = self.embedder.clone();
        let classifier = self.classifier.clone();
        let generation = ctx
            .run(move || async move {
                let report = run_skill_learning_for_experience(
                    &config,
                    store,
                    router,
                    embedder,
                    classifier.as_ref(),
                    request_for_run,
                )
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
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    classifier: &dyn moa_memory_pii::PiiClassifier,
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
    // Sanitize before the tool-call gate, before routing, and before any provider
    // call. A refusal ends the pass with its stable reason code and zero writes.
    let sanitized =
        match sanitize_experience_evidence(classifier, &session, &experience, &events).await {
            Ok(sanitized) => sanitized,
            Err(reason) => {
                return Ok(skipped_report(
                    request.session_id,
                    request.experience_id,
                    format!("evidence rejected: {reason}"),
                ));
            }
        };
    let tool_calls = sanitized.tool_call_count();

    // Recurrence dispatch relaxes the per-session tool-call floor: the recurrence
    // count is the evidence the single-session floor stands in for.
    let evidence = dispatch_evidence(config, &request);
    let effective_floor = evidence.effective_min_tool_calls(config.learning.skills.min_tool_calls);
    if tool_calls < effective_floor {
        return Ok(skipped_report(
            request.session_id,
            request.experience_id,
            format!("tool call count {tool_calls} below configured threshold {effective_floor}"),
        ));
    }

    let outcome = distill_skill_from_experience_with_learning(
        config,
        &session,
        ExperienceDistillationInput {
            experience,
            attributions,
            evidence: sanitized,
        },
        model_router.clone(),
        Some(store.clone()),
        embedder,
        &evidence,
    )
    .await?;

    record_distilled_candidate_filed(&outcome, candidate_source(&request));

    // For a recurrence dispatch, feed the remaining cluster members through the
    // sibling-accumulation/re-synthesis path so the just-filed draft generalizes
    // from day one and each member's suite pools as held-out material. Best-effort
    // per member: a load or model failure warns and leaves the rest untouched.
    if let (Some(recurrence), Some(proposal)) =
        (request.recurrence.as_ref(), proposal_from_outcome(&outcome))
    {
        feed_recurrence_siblings(
            store.as_ref(),
            model_router.as_ref(),
            classifier,
            &session,
            proposal,
            &recurrence.siblings,
        )
        .await;
    }

    Ok(report_from_proposal_generation(
        request.session_id,
        request.experience_id,
        proposal_generation_from_distillation(outcome),
    ))
}

/// Builds the dispatch evidence for a learning pass from its request.
///
/// A recurrence request carries the relaxed floor from `learning.recurrence` and
/// the full cluster (exemplar plus siblings) as reviewer evidence; absence of a
/// recurrence context is single-session dispatch with the configured floor.
fn dispatch_evidence(config: &MoaConfig, request: &RunSkillLearningRequest) -> DispatchEvidence {
    match &request.recurrence {
        None => DispatchEvidence::SingleSession,
        Some(recurrence) => {
            let mut member_experience_ids = Vec::with_capacity(recurrence.siblings.len() + 1);
            member_experience_ids.push(request.experience_id);
            member_experience_ids.extend(
                recurrence
                    .siblings
                    .iter()
                    .map(|sibling| sibling.experience_id),
            );
            DispatchEvidence::Recurrence(RecurrenceEvidence {
                occurrences: recurrence.occurrences,
                member_experience_ids,
                merged_fingerprints: recurrence.merged_fingerprints.clone(),
                first_seen: recurrence.first_seen,
                last_seen: recurrence.last_seen,
                relaxed_min_tool_calls: config.learning.recurrence.relaxed_min_tool_calls,
            })
        }
    }
}

/// The loop-stage `source` label a filed candidate is metered under.
fn candidate_source(request: &RunSkillLearningRequest) -> &'static str {
    if request.recurrence.is_some() {
        "recurrence_mined"
    } else {
        "distilled"
    }
}

/// Returns the filed/open proposal a distillation outcome produced, if any.
fn proposal_from_outcome(outcome: &DistillationOutcome) -> Option<&SkillDraftProposal> {
    match outcome {
        DistillationOutcome::NewSkillProposed { proposal }
        | DistillationOutcome::ImprovementProposed {
            proposal: Some(proposal),
            ..
        }
        | DistillationOutcome::DedupedOntoOpenProposal { proposal, .. } => Some(proposal),
        DistillationOutcome::ImprovementProposed { proposal: None, .. }
        | DistillationOutcome::Skipped { .. } => None,
    }
}

/// Loads every recurrence sibling's events, then accumulates and generalizes once.
///
/// Event loading is best-effort per member: a member whose events cannot be loaded
/// is logged and dropped so one bad member never aborts the rest. The successfully
/// loaded members are handed to the combined accumulation path, which durably pools
/// each member's suite and then runs a *single* generalization model call over the
/// whole batch (rather than one paid call per member). The accumulation caps at the
/// open proposal's sibling cap and leaves a claimed candidate untouched.
async fn feed_recurrence_siblings(
    store: &PostgresSessionStore,
    model_router: &ModelRouter,
    classifier: &dyn moa_memory_pii::PiiClassifier,
    session: &moa_core::types::session::SessionMeta,
    proposal: &SkillDraftProposal,
    siblings: &[RecurrenceSiblingRef],
) {
    let mut loaded: Vec<(SanitizedLearningEvidence, RecurrenceSiblingRef)> = Vec::new();
    for sibling in siblings {
        match load_sibling_evidence(store, classifier, sibling).await {
            Ok(evidence) => loaded.push((evidence, sibling.clone())),
            Err(error) => {
                tracing::warn!(
                    sibling_session_id = %sibling.session_id,
                    sibling_experience_id = %sibling.experience_id,
                    error = %error,
                    "recurrence sibling evidence unavailable; skipping this member"
                );
            }
        }
    }
    if loaded.is_empty() {
        return;
    }
    let inputs: Vec<RecurrenceSiblingSuite<'_>> = loaded
        .iter()
        .map(|(evidence, sibling)| RecurrenceSiblingSuite {
            evidence,
            source_experience_id: sibling.experience_id,
            source_session_id: sibling.session_id,
        })
        .collect();
    if let Err(error) =
        accumulate_recurrence_siblings(store, model_router, session.tenant_id, proposal, &inputs)
            .await
    {
        tracing::warn!(
            tenant_id = %session.tenant_id,
            candidate_id = %proposal.candidate_id,
            error = %error,
            "recurrence sibling accumulation failed"
        );
    }
}

/// Loads and sanitizes one sibling's bounded segment evidence for accumulation.
///
/// Each sibling is gated on its own: one member whose transcript refuses is
/// dropped without affecting the rest, so a single unreleasable session cannot
/// suppress an entire recurrence cluster's held-out material — nor can it slip
/// through on the strength of its siblings.
async fn load_sibling_evidence(
    store: &PostgresSessionStore,
    classifier: &dyn moa_memory_pii::PiiClassifier,
    sibling: &RecurrenceSiblingRef,
) -> MoaResult<SanitizedLearningEvidence> {
    let session = store.get_session(sibling.session_id).await?;
    let experience =
        load_experience_record(store, sibling.session_id, sibling.experience_id).await?;
    let events = bounded_segment_events(store, sibling.session_id, experience.segment_id).await?;
    sanitize_experience_evidence(classifier, &session, &experience, &events)
        .await
        .map_err(MoaError::StorageError)
}

/// Sanitizes one experience's bounded segment transcript into learning evidence.
///
/// The experience's own task summary and assessment evidence summaries ride the
/// same gate as the transcript: both are model-written text derived from it, and
/// both reach the learning provider.
///
/// The error is the stable reason code and carrier only. Nothing from the
/// refused content reaches the durable workflow report.
async fn sanitize_experience_evidence(
    classifier: &dyn moa_memory_pii::PiiClassifier,
    session: &moa_core::types::session::SessionMeta,
    experience: &moa_core::types::experience::ExperienceRecord,
    events: &[EventRecord],
) -> Result<SanitizedLearningEvidence, String> {
    let assessment_summaries = experience
        .evidence
        .iter()
        .map(|evidence| evidence.summary.clone())
        .collect::<Vec<_>>();
    let scope = EvidenceScope {
        tenant_id: experience.tenant_id,
        contact_id: session.contact.as_ref().map(|contact| contact.contact_id),
        session_id: experience.session_id,
        segment_id: experience.segment_id,
        experience_id: experience.id,
    };
    sanitize_segment_evidence(
        classifier,
        scope,
        events,
        SegmentNarrative {
            task_summary: experience.task_summary.as_deref(),
            assessment_summaries: &assessment_summaries,
        },
    )
    .await
    .map_err(|rejection| {
        format!(
            "carrier={} reason={}",
            rejection.carrier.as_str(),
            rejection.code()
        )
    })
}

/// Records a filed skill candidate for loop observability under a source stage.
///
/// `source` is the loop stage that filed it: `distilled` for a single-session
/// dispatch, `recurrence_mined` for a recurrence-cron dispatch. Metric recording
/// never affects the distillation outcome. See [`distilled_candidate_kind`] for
/// the outcome-to-kind mapping.
fn record_distilled_candidate_filed(outcome: &DistillationOutcome, source: &str) {
    if let Some(kind) = distilled_candidate_kind(outcome) {
        moa_observability::runtime_metrics::record_skill_learning_candidates_filed(source, kind, 1);
    }
}

/// Maps a distillation outcome onto the bounded `kind` label it files under, or
/// `None` when the outcome filed no new candidate.
///
/// A new-skill draft counts as `created` and an accepted improvement draft as
/// `improved`. A dedupe-hit that re-synthesized (rewrote) an open draft counts
/// as `resynthesized`; a dedupe-hit that kept the draft unchanged filed nothing
/// new, mirroring how an unchanged improvement or a skip files nothing.
fn distilled_candidate_kind(outcome: &DistillationOutcome) -> Option<&'static str> {
    match outcome {
        DistillationOutcome::NewSkillProposed { .. } => Some("created"),
        DistillationOutcome::ImprovementProposed {
            proposal: Some(_), ..
        } => Some("improved"),
        DistillationOutcome::DedupedOntoOpenProposal {
            resynthesis: SiblingResynthesis::DraftRewritten,
            ..
        } => Some("resynthesized"),
        DistillationOutcome::DedupedOntoOpenProposal { .. }
        | DistillationOutcome::ImprovementProposed { .. }
        | DistillationOutcome::Skipped { .. } => None,
    }
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
            skill_learning_failure_event(experience_id, error),
            None,
        )
        .await
}

async fn record_skill_learning_failure_from_workflow(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    experience_id: Uuid,
    error: &str,
) {
    let append = crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .append_event(Json(AppendEventRequest {
                session_id,
                event: skill_learning_failure_event(experience_id, error),
                dedupe_key: None,
            })),
    )
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

/// Builds the warning event emitted when detached skill-learning generation
/// fails. Both the production Restate append path and the direct-store test
/// helper construct their event here so the payload shape stays identical.
fn skill_learning_failure_event(experience_id: Uuid, error: &str) -> Event {
    Event::Warning {
        message: format!(
            "skill learning proposal generation failed for experience {experience_id}: {error}"
        ),
    }
}

async fn load_experience_record(
    store: &PostgresSessionStore,
    session_id: SessionId,
    experience_id: Uuid,
) -> MoaResult<moa_core::types::experience::ExperienceRecord> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::types::memory::SkillMetadata;
    use moa_skills::distiller::DistillationSkipReason;
    use moa_skills::proposals::{EditableSurface, SkillDraftProposal, SkillProposalOperation};

    fn proposal() -> SkillDraftProposal {
        SkillDraftProposal {
            candidate_id: Uuid::now_v7(),
            draft_artifact_revision_uid: Uuid::now_v7(),
            metadata: SkillMetadata {
                artifact_revision_uid: None,
                path: "tenants/x/skills/resynth-flow/SKILL.md".to_string(),
                name: "resynth-flow".to_string(),
                description: "recurring workflow".to_string(),
                tags: Vec::new(),
                allowed_tools: Vec::new(),
                actions: Vec::new(),
                has_execution_plan: false,
                estimated_tokens: 100,
            },
            operation: SkillProposalOperation::Created,
            surface: EditableSurface::SkillMarkdown,
        }
    }

    #[test]
    fn distilled_candidate_kind_maps_each_filed_outcome() {
        // Pins: the bounded `kind` label the loop metric files under. A fresh new/improved
        // draft files created/improved; a dedupe-hit that rewrote an open draft files
        // resynthesized; every non-filing outcome (unchanged dedupe, unchanged improvement,
        // skip) files nothing.
        assert_eq!(
            distilled_candidate_kind(&DistillationOutcome::NewSkillProposed {
                proposal: proposal(),
            }),
            Some("created")
        );
        assert_eq!(
            distilled_candidate_kind(&DistillationOutcome::ImprovementProposed {
                existing_skill_id: "resynth-flow".to_string(),
                proposal: Some(proposal()),
            }),
            Some("improved")
        );
        assert_eq!(
            distilled_candidate_kind(&DistillationOutcome::DedupedOntoOpenProposal {
                proposal: proposal(),
                resynthesis: SiblingResynthesis::DraftRewritten,
            }),
            Some("resynthesized")
        );
        // A dedupe-hit that kept the draft filed nothing new.
        assert_eq!(
            distilled_candidate_kind(&DistillationOutcome::DedupedOntoOpenProposal {
                proposal: proposal(),
                resynthesis: SiblingResynthesis::DraftUnchanged,
            }),
            None
        );
        assert_eq!(
            distilled_candidate_kind(&DistillationOutcome::ImprovementProposed {
                existing_skill_id: "resynth-flow".to_string(),
                proposal: None,
            }),
            None
        );
        assert_eq!(
            distilled_candidate_kind(&DistillationOutcome::Skipped {
                reason: DistillationSkipReason::UnlearnableOutcome,
            }),
            None
        );
    }

    #[test]
    fn proposed_generation_reports_candidate_and_draft_ids() {
        // Pins: a proposed draft surfaces both review identifiers so operators can jump
        // from the workflow report to the candidate and artifact.
        let session_id = SessionId::new();
        let experience_id = Uuid::now_v7();
        let candidate_id = Uuid::now_v7();
        let draft_uid = Uuid::now_v7();

        let report = report_from_proposal_generation(
            session_id,
            experience_id,
            SkillProposalGeneration::Proposed {
                candidate_id,
                draft_artifact_revision_uid: draft_uid,
            },
        );

        assert_eq!(report.outcome, "proposed");
        assert_eq!(report.candidate_id, Some(candidate_id));
        assert_eq!(report.draft_artifact_revision_uid, Some(draft_uid));
        assert_eq!(report.session_id, session_id);
        assert_eq!(report.experience_id, experience_id);
        assert_eq!(report.message, None);
    }

    #[test]
    fn unchanged_generation_reports_skip_without_identifiers() {
        // Pins: "existing skill already covers the run" is a skip with a reason, never a
        // phantom candidate reference.
        let report = report_from_proposal_generation(
            SessionId::new(),
            Uuid::now_v7(),
            SkillProposalGeneration::Unchanged,
        );

        assert_eq!(report.outcome, "skipped");
        assert_eq!(
            report.message.as_deref(),
            Some("existing skill did not need a draft")
        );
        assert_eq!(report.candidate_id, None);
        assert_eq!(report.draft_artifact_revision_uid, None);
    }

    #[test]
    fn skipped_generation_reports_the_stable_skip_reason() {
        // Pins: gate skips carry their stable reason for observability queries.
        let report = report_from_proposal_generation(
            SessionId::new(),
            Uuid::now_v7(),
            SkillProposalGeneration::Skipped {
                reason: DistillationSkipReason::UnlearnableOutcome,
            },
        );

        assert_eq!(report.outcome, "skipped");
        assert_eq!(report.message.as_deref(), Some("UnlearnableOutcome"));
        assert_eq!(report.candidate_id, None);
    }
}
