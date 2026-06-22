//! Restate virtual object for slow-path graph-memory ingestion.

use std::sync::Arc;
use std::time::Duration;

use crate::{
    ClassifiedFact, Conflict, ContradictionContext, ContradictionDetector, EmbeddedFact,
    EntityResolutionRequest, EntityResolver, ExtractedFact, ExtractedFactScopeHint, FactExtractor,
    HeuristicFactExtractor, IngestApplyReport, IngestCtx, IngestDecision, IngestError,
    ResolvedEntity, RrfPlusJudgeDetector, SessionTurn, chunk_turn, current_runtime,
    extraction_confidence_hint, fact_hash, fact_uid_from_hash, scoped_fact_uid,
    should_ingest_degraded,
};
use moa_core::{
    ContactId, MoaConfig, MoaError, ScopeContext, ScopedConn, TenantId, traits::EmbeddingProvider,
};
use moa_memory_graph::{
    AgeGraphStore, EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent,
};
use moa_memory_pii::{
    OpenAiPrivacyFilterClassifier, PiiClassifier, PiiResult, PiiSpan, classify_heuristic,
    redact_text,
};
use moa_memory_vector::{CohereV4Embedder, PgvectorStore, VectorStore};
use restate_sdk::prelude::*;
use secrecy::SecretString;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

const DONE_KEY_PREFIX: &str = "done";
const CHUNK_TARGET_TOKENS: usize = 700;
const CHUNK_OVERLAP_TOKENS: usize = 100;

/// Restate virtual object surface for slow-path turn ingestion.
#[restate_sdk::object]
pub trait IngestionVO {
    /// Ingests one finalized session turn into graph memory.
    async fn ingest_turn(turn: Json<SessionTurn>) -> Result<Json<IngestApplyReport>, HandlerError>;
}

/// Concrete ingestion virtual object implementation.
pub struct IngestionVOImpl;

impl IngestionVO for IngestionVOImpl {
    #[tracing::instrument(skip(self, ctx, turn))]
    async fn ingest_turn(
        &self,
        ctx: ObjectContext<'_>,
        turn: Json<SessionTurn>,
    ) -> Result<Json<IngestApplyReport>, HandlerError> {
        let turn = turn.into_inner();
        let done_key = done_key(turn.turn_seq);
        if ctx
            .get::<Json<bool>>(&done_key)
            .await?
            .map(Json::into_inner)
            .unwrap_or(false)
        {
            return Ok(Json::from(IngestApplyReport::default()));
        }

        let runtime = current_runtime().map_err(HandlerError::from)?;
        let pool = runtime.pool().clone();
        let pii_service_url = runtime.pii_service_url().map(str::to_string);
        let cohere_api_key_env = runtime.cohere_api_key_env().to_string();
        let contradiction_detector = runtime.contradiction_detector();
        let degraded = workspace_degraded(&pool, &turn).await?;
        if degraded && !should_ingest_degraded(&turn) {
            ctx.set(&done_key, Json::from(true));
            return Ok(Json::from(IngestApplyReport {
                skipped: 1,
                ..IngestApplyReport::default()
            }));
        }

        let turn_for_chunking = turn.clone();
        let chunks = ctx
            .run(|| async move {
                chunk_turn(
                    &turn_for_chunking,
                    CHUNK_TARGET_TOKENS,
                    CHUNK_OVERLAP_TOKENS,
                )
                .map(Json::from)
                .map_err(HandlerError::from)
            })
            .name("chunk")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let extract_chunks = chunks.clone();
        let extractor = runtime.extractor();
        let extracted = ctx
            .run(|| async move {
                extractor
                    .extract(&extract_chunks)
                    .await
                    .map(Json::from)
                    .map_err(HandlerError::from)
            })
            .name("extract")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let classify_facts_input = extracted.clone();
        let classified = ctx
            .run(|| async move {
                classify_facts(&classify_facts_input, pii_service_url.as_deref())
                    .await
                    .map(Json::from)
            })
            .name("classify_pii")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let embed_input = classified.clone();
        let embed_cohere_api_key_env = cohere_api_key_env.clone();
        let embedded = ctx
            .run(|| async move {
                embed_batch(&embed_input, &embed_cohere_api_key_env)
                    .await
                    .map(Json::from)
            })
            .name("embed")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let contradiction_turn = turn.clone();
        let contradiction_input = embedded.clone();
        let contradiction_pool = pool.clone();
        let contradiction_detector = contradiction_detector.clone();
        let decisions = ctx
            .run(|| async move {
                detect_contradictions_with(
                    contradiction_detector.as_ref(),
                    contradiction_pool,
                    &contradiction_turn,
                    &contradiction_input,
                )
                .await
                .map(Json::from)
            })
            .name("contradict")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let upsert_turn = turn.clone();
        let upsert_pool = pool.clone();
        let upsert_entity_resolver = runtime.entity_resolver();
        let upsert_entity_blocking_embedder = runtime.entity_blocking_embedder();
        let report = ctx
            .run(|| async move {
                apply_decisions(
                    &upsert_pool,
                    upsert_entity_resolver.as_ref(),
                    upsert_entity_blocking_embedder,
                    &upsert_turn,
                    &decisions,
                )
                .await
                .map(Json::from)
            })
            .name("upsert")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        ctx.set(&done_key, Json::from(true));
        Ok(Json::from(report))
    }
}

/// Runs the slow-path ingestion steps directly in the current process.
///
/// Hosts that call this helper must first install an ingestion runtime with
/// [`crate::install_runtime_with_pool`]. Restate handlers should continue to use
/// [`IngestionVO::ingest_turn`] so the step journal remains durable.
pub async fn ingest_turn_direct(turn: SessionTurn) -> Result<IngestApplyReport, HandlerError> {
    let runtime = current_runtime().map_err(HandlerError::from)?;
    ingest_turn_direct_with_pool_and_pii(
        DirectIngestDeps {
            pool: runtime.pool().clone(),
            pii_service_url: runtime.pii_service_url().map(str::to_string),
            cohere_api_key_env: runtime.cohere_api_key_env().to_string(),
            extractor: runtime.extractor(),
            entity_resolver: runtime.entity_resolver(),
            entity_blocking_embedder: runtime.entity_blocking_embedder(),
            contradiction_detector: runtime.contradiction_detector(),
        },
        turn,
    )
    .await
}

/// Runs the slow-path ingestion steps directly against an explicit Postgres pool.
///
/// This is intended for embedded hosts that own more than one pool in the same
/// process, such as integration tests. Restate handlers should continue to use
/// [`IngestionVO::ingest_turn`] so the step journal remains durable.
pub async fn ingest_turn_direct_with_pool(
    pool: PgPool,
    turn: SessionTurn,
) -> Result<IngestApplyReport, HandlerError> {
    let config = MoaConfig::load_from_env().map_err(HandlerError::from)?;
    let contradiction_detector = Arc::new(RrfPlusJudgeDetector::from_config_or_heuristic(&config));
    let memory = config.memory;
    ingest_turn_direct_with_pool_and_pii(
        DirectIngestDeps {
            pool,
            pii_service_url: memory.pii_service_url,
            cohere_api_key_env: memory.vector.embedder.cohere.api_key_env,
            extractor: Arc::new(HeuristicFactExtractor),
            entity_resolver: Arc::new(EntityResolver::deterministic_for_app_role()),
            entity_blocking_embedder: None,
            contradiction_detector,
        },
        turn,
    )
    .await
}

struct DirectIngestDeps {
    pool: PgPool,
    pii_service_url: Option<String>,
    cohere_api_key_env: String,
    extractor: Arc<dyn FactExtractor>,
    entity_resolver: Arc<EntityResolver>,
    entity_blocking_embedder: Option<Arc<dyn EmbeddingProvider>>,
    contradiction_detector: Arc<dyn ContradictionDetector>,
}

async fn ingest_turn_direct_with_pool_and_pii(
    deps: DirectIngestDeps,
    turn: SessionTurn,
) -> Result<IngestApplyReport, HandlerError> {
    let DirectIngestDeps {
        pool,
        pii_service_url,
        cohere_api_key_env,
        extractor,
        entity_resolver,
        entity_blocking_embedder,
        contradiction_detector,
    } = deps;
    let degraded = workspace_degraded(&pool, &turn).await?;
    if degraded && !should_ingest_degraded(&turn) {
        return Ok(IngestApplyReport {
            skipped: 1,
            ..IngestApplyReport::default()
        });
    }

    let chunks =
        chunk_turn(&turn, CHUNK_TARGET_TOKENS, CHUNK_OVERLAP_TOKENS).map_err(HandlerError::from)?;
    let extracted = extractor
        .extract(&chunks)
        .await
        .map_err(HandlerError::from)?;
    let classified = classify_facts(&extracted, pii_service_url.as_deref()).await?;
    let embedded = embed_batch(&classified, &cohere_api_key_env).await?;
    let decisions = detect_contradictions_with(
        contradiction_detector.as_ref(),
        pool.clone(),
        &turn,
        &embedded,
    )
    .await?;
    apply_decisions(
        &pool,
        entity_resolver.as_ref(),
        entity_blocking_embedder,
        &turn,
        &decisions,
    )
    .await
}

/// Runs the slow-path ingestion steps with explicit deterministic dependencies.
///
/// This helper is intended for integration tests that need to exercise the M10 pipeline without
/// depending on process-global environment variables or billed provider calls.
pub async fn ingest_turn_direct_with_ctx(
    ctx: IngestCtx,
    turn: SessionTurn,
) -> Result<IngestApplyReport, HandlerError> {
    let degraded = workspace_degraded(&ctx.pool, &turn).await?;
    if degraded && !should_ingest_degraded(&turn) {
        return Ok(IngestApplyReport {
            skipped: 1,
            ..IngestApplyReport::default()
        });
    }

    let chunks =
        chunk_turn(&turn, CHUNK_TARGET_TOKENS, CHUNK_OVERLAP_TOKENS).map_err(HandlerError::from)?;
    let extracted = ctx
        .extractor
        .extract(&chunks)
        .await
        .map_err(HandlerError::from)?;
    let classified = classify_facts_with(ctx.pii.as_ref(), &extracted).await?;
    let embedded = embed_batch_with(ctx.embedder.as_ref(), &classified).await?;
    let decisions =
        detect_contradictions_with(ctx.contradict.as_ref(), ctx.pool.clone(), &turn, &embedded)
            .await?;
    let entity_blocking_embedder = ctx.entity_blocking_enabled.then(|| ctx.embedder.clone());
    apply_decisions(
        &ctx.pool,
        ctx.entity_resolver.as_ref(),
        entity_blocking_embedder,
        &turn,
        &decisions,
    )
    .await
}

/// Builds the object key used to serialize ingestion per workspace/session.
#[must_use]
pub fn ingestion_object_key(turn: &SessionTurn) -> String {
    format!("{}:{}", turn.workspace_id, turn.session_id)
}

/// Builds a finalized turn transcript from an LLM request and response.
#[must_use]
pub fn turn_transcript(messages: &[moa_core::ContextMessage], response_text: &str) -> String {
    let mut lines = messages
        .iter()
        .filter(|message| matches!(message.role, moa_core::MessageRole::User))
        .map(|message| format!("user: {}", message.content.trim()))
        .collect::<Vec<_>>();
    if !response_text.trim().is_empty() {
        lines.push(format!("assistant: {}", response_text.trim()));
    }
    lines.join("\n")
}

async fn classify_facts(
    facts: &[ExtractedFact],
    pii_service_url: Option<&str>,
) -> Result<Vec<ClassifiedFact>, HandlerError> {
    if let Some(url) = pii_service_url {
        let classifier =
            OpenAiPrivacyFilterClassifier::new(url.to_string()).map_err(HandlerError::from)?;
        return classify_facts_with(&classifier, facts).await;
    }

    let mut classified = Vec::with_capacity(facts.len());
    for fact in facts {
        let result = classify_heuristic(&fact.summary);
        let redacted_fact = redact_fact(fact, &result);
        classified.push(ClassifiedFact {
            fact: redacted_fact,
            pii_class: result.class,
            pii_spans: result.spans,
        });
    }
    Ok(classified)
}

async fn classify_facts_with(
    classifier: &dyn PiiClassifier,
    facts: &[ExtractedFact],
) -> Result<Vec<ClassifiedFact>, HandlerError> {
    let mut classified = Vec::with_capacity(facts.len());
    for fact in facts {
        let result = classifier
            .classify(&fact.summary)
            .await
            .map_err(HandlerError::from)?;
        let redacted_fact = redact_fact(fact, &result);
        classified.push(ClassifiedFact {
            fact: redacted_fact,
            pii_class: result.class,
            pii_spans: result.spans,
        });
    }
    Ok(classified)
}

fn redact_fact(fact: &ExtractedFact, result: &PiiResult) -> ExtractedFact {
    let redacted_summary = redact_text(&fact.summary, &result.spans);
    let redacted_subject = redact_fact_part(&fact.subject, &fact.summary, &result.spans);
    let redacted_predicate = redact_fact_part(&fact.predicate, &fact.summary, &result.spans);
    let redacted_object = redact_fact_part(&fact.object, &fact.summary, &result.spans);
    if redacted_summary == fact.summary
        && redacted_subject == fact.subject
        && redacted_predicate == fact.predicate
        && redacted_object == fact.object
    {
        return fact.clone();
    }

    let mut redacted = ExtractedFact {
        uid: fact.uid,
        subject: redacted_subject,
        predicate: redacted_predicate,
        object: redacted_object,
        summary: redacted_summary,
        source_chunk: fact.source_chunk,
        scope_hint: fact.scope_hint,
        confidence: fact.confidence,
    };
    match fact_hash(&redacted) {
        Ok(hash) => redacted.uid = fact_uid_from_hash(&hash),
        Err(error) => tracing::warn!(
            error = %error,
            "failed to recompute redacted fact uid; preserving original extracted uid"
        ),
    }
    redacted
}

fn redact_fact_part(part: &str, summary: &str, spans: &[PiiSpan]) -> String {
    let mut redacted = part.to_string();
    for span in spans {
        if span.start >= span.end
            || span.end > summary.len()
            || !summary.is_char_boundary(span.start)
            || !summary.is_char_boundary(span.end)
        {
            continue;
        }
        let source_text = &summary[span.start..span.end];
        if source_text.is_empty() {
            continue;
        }
        let replacement = redaction_replacement(source_text, span);
        redacted = redacted.replace(source_text, &replacement);
        let trimmed_source = source_text.trim_matches(|character: char| {
            character.is_ascii_punctuation() && !matches!(character, '[' | ']')
        });
        if !trimmed_source.is_empty() && trimmed_source != source_text {
            let trimmed_replacement = redaction_replacement(trimmed_source, span);
            redacted = redacted.replace(trimmed_source, &trimmed_replacement);
        }
    }
    redacted
}

fn redaction_replacement(source_text: &str, span: &PiiSpan) -> String {
    redact_text(
        source_text,
        &[PiiSpan::with_replacement(
            0,
            source_text.len(),
            span.category,
            span.confidence,
            span.redaction_replacement(),
        )],
    )
}

async fn embed_batch(
    facts: &[ClassifiedFact],
    cohere_api_key_env: &str,
) -> Result<Vec<EmbeddedFact>, HandlerError> {
    let Some(api_key) = std::env::var(cohere_api_key_env).ok() else {
        return Ok(facts
            .iter()
            .cloned()
            .map(|classified| EmbeddedFact {
                classified,
                embedding: None,
                embedding_model: None,
                embedding_model_version: None,
            })
            .collect());
    };

    let embedder = CohereV4Embedder::new(SecretString::from(api_key));
    embed_batch_with(&embedder, facts).await
}

async fn embed_batch_with(
    embedder: &dyn EmbeddingProvider,
    facts: &[ClassifiedFact],
) -> Result<Vec<EmbeddedFact>, HandlerError> {
    let texts = facts
        .iter()
        .map(|fact| fact.fact.summary.clone())
        .collect::<Vec<_>>();
    let embeddings = embedder.embed(&texts).await.map_err(HandlerError::from)?;
    Ok(facts
        .iter()
        .cloned()
        .zip(embeddings)
        .map(|(classified, embedding)| EmbeddedFact {
            classified,
            embedding: Some(embedding),
            embedding_model: Some(embedder.model_name().to_string()),
            embedding_model_version: Some(embedder.model_version()),
        })
        .collect())
}

async fn detect_contradictions_with(
    detector: &dyn ContradictionDetector,
    pool: PgPool,
    turn: &SessionTurn,
    embedded: &[EmbeddedFact],
) -> Result<Vec<IngestDecision>, HandlerError> {
    let mut decisions = Vec::with_capacity(embedded.len());

    for fact in embedded {
        let scope = fact_scope(turn, fact).map_err(HandlerError::from)?;
        let vector = Arc::new(PgvectorStore::new_for_app_role(pool.clone(), scope.clone()));
        let ctx = ContradictionContext::for_app_role(pool.clone(), scope, vector);
        let conflict = detector
            .check_one_slow(fact, &ctx)
            .await
            .map_err(HandlerError::from)?;
        decisions.push(decision_from_conflict(conflict, fact.clone()));
    }

    Ok(decisions)
}

fn decision_from_conflict(conflict: Conflict, fact: EmbeddedFact) -> IngestDecision {
    match conflict {
        Conflict::Insert | Conflict::Indeterminate => IngestDecision::Insert { fact },
        Conflict::Supersede(old_uid) => IngestDecision::Supersede { old_uid, fact },
        Conflict::Duplicate(fact_uid) => IngestDecision::SkipDuplicate { fact_uid },
    }
}

fn decision_scope(
    turn: &SessionTurn,
    decision: &IngestDecision,
) -> Result<ScopeContext, IngestError> {
    match decision_fact(decision) {
        Some(fact) => fact_scope(turn, fact),
        None => turn_contact_scope(turn),
    }
}

fn fact_scope(turn: &SessionTurn, fact: &EmbeddedFact) -> Result<ScopeContext, IngestError> {
    match fact.classified.fact.scope_hint {
        ExtractedFactScopeHint::User => turn_contact_scope(turn),
        ExtractedFactScopeHint::Workspace => turn_tenant_scope(turn),
    }
}

fn turn_tenant_scope(turn: &SessionTurn) -> Result<ScopeContext, IngestError> {
    Ok(ScopeContext::tenant(turn_tenant_id(turn)?))
}

fn turn_contact_scope(turn: &SessionTurn) -> Result<ScopeContext, IngestError> {
    Ok(ScopeContext::contact(
        turn_tenant_id(turn)?,
        turn_contact_id(turn)?,
    ))
}

fn turn_tenant_id(turn: &SessionTurn) -> Result<TenantId, IngestError> {
    Uuid::parse_str(turn.workspace_id.as_str())
        .map(TenantId::from)
        .map_err(|error| {
            IngestError::Scope(MoaError::ValidationError(format!(
                "slow-path turn workspace_id must be a tenant UUID: {error}"
            )))
        })
}

fn turn_contact_id(turn: &SessionTurn) -> Result<ContactId, IngestError> {
    Uuid::parse_str(turn.user_id.as_str())
        .map(ContactId)
        .map_err(|error| {
            IngestError::Scope(MoaError::ValidationError(format!(
                "slow-path turn user_id must be a contact UUID: {error}"
            )))
        })
}

async fn apply_decisions(
    pool: &PgPool,
    entity_resolver: &EntityResolver,
    entity_blocking_embedder: Option<Arc<dyn EmbeddingProvider>>,
    turn: &SessionTurn,
    decisions: &[IngestDecision],
) -> Result<IngestApplyReport, HandlerError> {
    let mut report = IngestApplyReport::default();

    for decision in decisions {
        let scope = decision_scope(turn, decision).map_err(HandlerError::from)?;
        match apply_one_decision(
            pool,
            &scope,
            entity_resolver,
            entity_blocking_embedder.clone(),
            turn,
            decision,
        )
        .await
        {
            Ok(ApplyOutcome::Inserted) => report.inserted += 1,
            Ok(ApplyOutcome::Superseded) => report.superseded += 1,
            Ok(ApplyOutcome::Skipped) => report.skipped += 1,
            Err(error) => {
                report.failed += 1;
                let error_message = format!("{error:?}");
                write_dlq(pool, &scope, turn, decision, &error_message).await?;
                tracing::warn!(
                    error = ?error,
                    session_id = %turn.session_id,
                    turn_seq = turn.turn_seq,
                    "slow-path ingestion fact failed and was written to DLQ"
                );
            }
        }
    }

    Ok(report)
}

async fn apply_one_decision(
    pool: &PgPool,
    scope: &ScopeContext,
    entity_resolver: &EntityResolver,
    entity_blocking_embedder: Option<Arc<dyn EmbeddingProvider>>,
    turn: &SessionTurn,
    decision: &IngestDecision,
) -> Result<ApplyOutcome, HandlerError> {
    let Some(fact) = decision_fact(decision) else {
        return Ok(ApplyOutcome::Skipped);
    };
    let entity_vector = entity_blocking_embedder.as_ref().map(|_| {
        Arc::new(PgvectorStore::new_for_app_role(pool.clone(), scope.clone()))
            as Arc<dyn VectorStore>
    });
    let scoped_resolver =
        entity_blocking_embedder
            .zip(entity_vector.clone())
            .map(|(embedder, vector)| {
                entity_resolver
                    .clone()
                    .with_embedding_blocking(embedder, vector, 0.80)
            });
    let resolver = scoped_resolver.as_ref().unwrap_or(entity_resolver);
    let graph = graph_store(pool.clone(), scope.clone(), fact, entity_vector);
    apply_one_decision_with_graph(pool, scope, &graph, resolver, turn, decision).await
}

async fn apply_one_decision_with_graph(
    pool: &PgPool,
    scope: &ScopeContext,
    graph: &dyn GraphStore,
    entity_resolver: &EntityResolver,
    turn: &SessionTurn,
    decision: &IngestDecision,
) -> Result<ApplyOutcome, HandlerError> {
    let Some(fact) = decision_fact(decision) else {
        return Ok(ApplyOutcome::Skipped);
    };
    let hash = fact_hash(&fact.classified.fact).map_err(HandlerError::from)?;
    if dedup_fact_uid(pool, scope, turn, &hash).await?.is_some() {
        return Ok(ApplyOutcome::Skipped);
    }

    let entities = resolve_fact_entities(pool, scope, graph, entity_resolver, turn, fact).await?;
    let fact_uid = scoped_fact_uid(&turn.workspace_id, &turn.session_id, turn.turn_seq, &hash);
    match decision {
        IngestDecision::Insert { fact } => {
            let uid = graph
                .create_node(node_intent(turn, scope, fact, &hash, fact_uid))
                .await
                .map_err(HandlerError::from)?;
            attach_fact_entity_edges(graph, turn, scope, uid, fact, &entities).await?;
            insert_dedup(pool, scope, turn, &hash, uid).await?;
            Ok(ApplyOutcome::Inserted)
        }
        IngestDecision::Supersede { old_uid, fact } => {
            let uid = graph
                .supersede_node(*old_uid, node_intent(turn, scope, fact, &hash, fact_uid))
                .await
                .map_err(HandlerError::from)?;
            attach_fact_entity_edges(graph, turn, scope, uid, fact, &entities).await?;
            insert_dedup(pool, scope, turn, &hash, uid).await?;
            Ok(ApplyOutcome::Superseded)
        }
        IngestDecision::SkipDuplicate { .. } => Ok(ApplyOutcome::Skipped),
    }
}

fn graph_store(
    pool: PgPool,
    scope: ScopeContext,
    fact: &EmbeddedFact,
    entity_vector: Option<Arc<dyn VectorStore>>,
) -> AgeGraphStore {
    let store = AgeGraphStore::scoped_for_app_role(pool.clone(), scope.clone());
    if fact.embedding.is_some() {
        store.with_vector_store(Arc::new(PgvectorStore::new_for_app_role(pool, scope)))
    } else if let Some(vector) = entity_vector {
        store.with_vector_store(vector)
    } else {
        store
    }
}

fn node_intent(
    turn: &SessionTurn,
    scope: &ScopeContext,
    fact: &EmbeddedFact,
    hash: &[u8],
    fact_uid: uuid::Uuid,
) -> NodeWriteIntent {
    let extracted = &fact.classified.fact;
    let workspace_id = scope_workspace_id(scope);
    let user_id = scope_user_id(scope);
    let scope_tier = scope.tier_str();
    NodeWriteIntent {
        uid: fact_uid,
        label: NodeLabel::Fact,
        workspace_id: workspace_id.clone(),
        user_id: user_id.clone(),
        scope: scope_tier.to_string(),
        name: extracted.subject.clone(),
        properties: json!({
            "uid": fact_uid.to_string(),
            "extracted_uid": extracted.uid.to_string(),
            "workspace_id": workspace_id,
            "user_id": user_id,
            "scope": scope_tier,
            "name": extracted.subject,
            "subject": extracted.subject,
            "predicate": extracted.predicate,
            "object": extracted.object,
            "summary": extracted.summary,
            "source_session_id": turn.session_id.to_string(),
            "source_turn_seq": turn.turn_seq,
            "source_chunk": extracted.source_chunk,
            "fact_hash": hex_bytes(hash),
            "pii_class": fact.classified.pii_class.as_str(),
        }),
        pii_class: fact.classified.pii_class,
        confidence: Some(extracted_confidence(extracted)),
        valid_from: turn.finalized_at,
        embedding: fact.embedding.clone(),
        embedding_model: fact.embedding_model.clone(),
        embedding_model_version: fact.embedding_model_version,
        actor_id: turn.user_id.to_string(),
        actor_kind: "user".to_string(),
    }
}

fn scope_workspace_id(scope: &ScopeContext) -> Option<String> {
    Some(scope.tenant_id().to_string())
}

fn scope_user_id(scope: &ScopeContext) -> Option<String> {
    scope.contact_id().map(|contact_id| contact_id.to_string())
}

#[derive(Debug, Clone)]
struct ResolvedFactEntities {
    subject: ResolvedEntity,
    object: ResolvedEntity,
}

async fn resolve_fact_entities(
    pool: &PgPool,
    scope: &ScopeContext,
    graph: &dyn GraphStore,
    entity_resolver: &EntityResolver,
    turn: &SessionTurn,
    fact: &EmbeddedFact,
) -> Result<ResolvedFactEntities, HandlerError> {
    let extracted = &fact.classified.fact;
    let confidence = extracted_confidence(extracted);
    let actor_id = turn.user_id.to_string();
    let subject = entity_resolver
        .resolve(
            pool,
            graph,
            EntityResolutionRequest {
                scope,
                name: &extracted.subject,
                pii_class: fact.classified.pii_class,
                confidence,
                valid_from: turn.finalized_at,
                actor_id: &actor_id,
                actor_kind: "user",
            },
        )
        .await
        .map_err(HandlerError::from)?;
    let object = entity_resolver
        .resolve(
            pool,
            graph,
            EntityResolutionRequest {
                scope,
                name: &extracted.object,
                pii_class: fact.classified.pii_class,
                confidence,
                valid_from: turn.finalized_at,
                actor_id: &actor_id,
                actor_kind: "user",
            },
        )
        .await
        .map_err(HandlerError::from)?;

    Ok(ResolvedFactEntities { subject, object })
}

fn extracted_confidence(fact: &ExtractedFact) -> f64 {
    fact.confidence
        .unwrap_or_else(|| extraction_confidence_hint(&fact.summary))
        .clamp(0.0, 1.0)
}

async fn attach_fact_entity_edges(
    graph: &dyn GraphStore,
    turn: &SessionTurn,
    scope: &ScopeContext,
    fact_uid: uuid::Uuid,
    fact: &EmbeddedFact,
    entities: &ResolvedFactEntities,
) -> Result<(), HandlerError> {
    graph
        .create_edge(entity_fact_edge_intent(
            turn,
            scope,
            entities.subject.uid,
            fact_uid,
            "subject",
            entities.subject.alias_mention.as_deref(),
        ))
        .await
        .map_err(HandlerError::from)?;
    graph
        .create_edge(fact_entity_edge_intent(
            turn,
            scope,
            fact_uid,
            entities.object.uid,
            "object",
            &fact.classified.fact.predicate,
            entities.object.alias_mention.as_deref(),
        ))
        .await
        .map_err(HandlerError::from)?;
    Ok(())
}

fn entity_fact_edge_intent(
    turn: &SessionTurn,
    scope: &ScopeContext,
    entity_uid: uuid::Uuid,
    fact_uid: uuid::Uuid,
    role: &str,
    alias_mention: Option<&str>,
) -> EdgeWriteIntent {
    EdgeWriteIntent {
        uid: uuid::Uuid::now_v7(),
        label: EdgeLabel::RelatesTo,
        start_uid: entity_uid,
        end_uid: fact_uid,
        properties: entity_edge_properties(turn, role, alias_mention),
        workspace_id: scope_workspace_id(scope),
        user_id: scope_user_id(scope),
        scope: scope.tier_str().to_string(),
        actor_id: turn.user_id.to_string(),
        actor_kind: "user".to_string(),
    }
}

fn fact_entity_edge_intent(
    turn: &SessionTurn,
    scope: &ScopeContext,
    fact_uid: uuid::Uuid,
    entity_uid: uuid::Uuid,
    role: &str,
    predicate: &str,
    alias_mention: Option<&str>,
) -> EdgeWriteIntent {
    EdgeWriteIntent {
        uid: uuid::Uuid::now_v7(),
        label: fact_object_edge_label(predicate),
        start_uid: fact_uid,
        end_uid: entity_uid,
        properties: entity_edge_properties(turn, role, alias_mention),
        workspace_id: scope_workspace_id(scope),
        user_id: scope_user_id(scope),
        scope: scope.tier_str().to_string(),
        actor_id: turn.user_id.to_string(),
        actor_kind: "user".to_string(),
    }
}

fn fact_object_edge_label(predicate: &str) -> EdgeLabel {
    let normalized = normalize_predicate(predicate);
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    if tokens
        .iter()
        .any(|token| matches!(*token, "depends" | "uses" | "requires" | "calls" | "built"))
        || normalized == "require runbook"
    {
        return EdgeLabel::DependsOn;
    }
    if (tokens.contains(&"owned") && tokens.contains(&"by"))
        || (tokens.contains(&"belongs") && tokens.contains(&"to"))
        || (tokens.contains(&"maintained") && tokens.contains(&"by"))
    {
        return EdgeLabel::OwnedBy;
    }
    EdgeLabel::RelatesTo
}

fn normalize_predicate(predicate: &str) -> String {
    let mut tokens = Vec::new();
    let mut token = String::new();
    for character in predicate.chars() {
        if character.is_alphanumeric() {
            token.extend(character.to_lowercase());
        } else if !token.is_empty() {
            tokens.push(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens.join(" ")
}

fn entity_edge_properties(
    turn: &SessionTurn,
    role: &str,
    alias_mention: Option<&str>,
) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    properties.insert("role".to_string(), json!(role));
    properties.insert("source".to_string(), json!("slow_path_entity_resolution"));
    properties.insert(
        "source_session_id".to_string(),
        json!(turn.session_id.to_string()),
    );
    properties.insert("source_turn_seq".to_string(), json!(turn.turn_seq));
    // GraphStore has no node-property update API yet; prompt 09 can consolidate edge aliases onto nodes.
    if let Some(alias) = alias_mention.filter(|alias| !alias.trim().is_empty()) {
        properties.insert("alias_mention".to_string(), json!(alias));
    }
    serde_json::Value::Object(properties)
}

async fn workspace_degraded(pool: &PgPool, turn: &SessionTurn) -> Result<bool, HandlerError> {
    let scope = turn_tenant_scope(turn).map_err(HandlerError::from)?;
    let mut conn = ScopedConn::begin(pool, &scope)
        .await
        .map_err(HandlerError::from)?;
    let degraded = sqlx::query_scalar::<_, bool>(
        "SELECT slow_path_degraded FROM moa.workspace_state WHERE workspace_id = $1",
    )
    .bind(scope.tenant_id().to_string())
    .fetch_optional(conn.as_mut())
    .await
    .map_err(HandlerError::from)?
    .unwrap_or(false);
    conn.commit().await.map_err(HandlerError::from)?;
    Ok(degraded)
}

async fn dedup_fact_uid(
    pool: &PgPool,
    scope: &ScopeContext,
    turn: &SessionTurn,
    hash: &[u8],
) -> Result<Option<uuid::Uuid>, HandlerError> {
    let mut conn = ScopedConn::begin(pool, scope)
        .await
        .map_err(HandlerError::from)?;
    let turn_seq = turn_seq_i64(turn)?;
    let user_id = scope_user_id(scope);
    let uid = sqlx::query_scalar::<_, uuid::Uuid>(
        r#"
        SELECT fact_uid
        FROM moa.ingest_dedup
        WHERE workspace_id = $1
          AND session_id = $2
          AND turn_seq = $3
          AND fact_hash = $4
          AND scope = $5
          AND (($6::text IS NULL AND user_id IS NULL) OR user_id = $6)
        "#,
    )
    .bind(scope.tenant_id().to_string())
    .bind(turn.session_id.0)
    .bind(turn_seq)
    .bind(hash)
    .bind(scope.tier_str())
    .bind(user_id.as_deref())
    .fetch_optional(conn.as_mut())
    .await
    .map_err(HandlerError::from)?;
    conn.commit().await.map_err(HandlerError::from)?;
    Ok(uid)
}

async fn insert_dedup(
    pool: &PgPool,
    scope: &ScopeContext,
    turn: &SessionTurn,
    hash: &[u8],
    fact_uid: uuid::Uuid,
) -> Result<(), HandlerError> {
    let mut conn = ScopedConn::begin(pool, scope)
        .await
        .map_err(HandlerError::from)?;
    let turn_seq = turn_seq_i64(turn)?;
    let user_id = scope_user_id(scope);
    sqlx::query(
        r#"
        INSERT INTO moa.ingest_dedup
            (workspace_id, user_id, session_id, turn_seq, fact_hash, fact_uid)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (workspace_id, session_id, turn_seq, fact_hash) DO NOTHING
        "#,
    )
    .bind(scope.tenant_id().to_string())
    .bind(user_id.as_deref())
    .bind(turn.session_id.0)
    .bind(turn_seq)
    .bind(hash)
    .bind(fact_uid)
    .execute(conn.as_mut())
    .await
    .map_err(HandlerError::from)?;
    conn.commit().await.map_err(HandlerError::from)
}

async fn write_dlq(
    pool: &PgPool,
    scope: &ScopeContext,
    turn: &SessionTurn,
    decision: &IngestDecision,
    error: &str,
) -> Result<(), HandlerError> {
    let mut conn = ScopedConn::begin(pool, scope)
        .await
        .map_err(HandlerError::from)?;
    let turn_seq = turn_seq_i64(turn)?;
    let payload = serde_json::to_value(decision).map_err(HandlerError::from)?;
    let user_id = scope_user_id(scope);
    sqlx::query(
        r#"
        INSERT INTO moa.ingest_dlq
            (workspace_id, user_id, session_id, turn_seq, payload, error, next_retry_at)
        VALUES ($1, $2, $3, $4, $5, $6, now() + INTERVAL '5 minutes')
        "#,
    )
    .bind(scope.tenant_id().to_string())
    .bind(user_id.as_deref())
    .bind(turn.session_id.0)
    .bind(turn_seq)
    .bind(payload)
    .bind(error)
    .execute(conn.as_mut())
    .await
    .map_err(HandlerError::from)?;
    conn.commit().await.map_err(HandlerError::from)
}

fn decision_fact(decision: &IngestDecision) -> Option<&EmbeddedFact> {
    match decision {
        IngestDecision::Insert { fact } | IngestDecision::Supersede { fact, .. } => Some(fact),
        IngestDecision::SkipDuplicate { .. } => None,
    }
}

fn done_key(turn_seq: u64) -> String {
    format!("{DONE_KEY_PREFIX}:{turn_seq}")
}

fn turn_seq_i64(turn: &SessionTurn) -> Result<i64, HandlerError> {
    i64::try_from(turn.turn_seq).map_err(|_| {
        TerminalError::new(format!("turn_seq {} does not fit into i64", turn.turn_seq)).into()
    })
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn ingest_step_retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_millis(250))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(5))
        .max_attempts(5)
}

enum ApplyOutcome {
    Inserted,
    Superseded,
    Skipped,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{ContextMessage, SessionId, UserId, WorkspaceId};
    use moa_memory_graph::{EdgeLabel, PiiClass};
    use moa_memory_pii::{PiiCategory, PiiResult, PiiSpan, classify_heuristic};

    use super::{
        entity_fact_edge_intent, fact_entity_edge_intent, fact_object_edge_label, redact_fact,
        turn_tenant_scope, turn_transcript,
    };
    use crate::{ExtractedFact, ExtractedFactScopeHint, fact_hash};

    #[test]
    fn turn_transcript_keeps_user_messages_and_response() {
        let messages = vec![
            ContextMessage::system("system prompt"),
            ContextMessage::user("Remember that auth uses JWT."),
            ContextMessage::assistant("Previous answer"),
        ];

        let transcript = turn_transcript(&messages, "Stored that.");

        assert!(transcript.contains("user: Remember that auth uses JWT."));
        assert!(transcript.contains("assistant: Stored that."));
        assert!(!transcript.contains("system prompt"));
        assert!(!transcript.contains("Previous answer"));
    }

    #[test]
    fn pii_heuristic_classifier_marks_secrets_restricted() {
        // Pins: ingest fallback PII classification uses the shared heuristic classifier.
        let result = classify_heuristic("The API secret is sk-test-123.");

        assert_eq!(result.class, PiiClass::Restricted);
        assert!(
            result
                .spans
                .iter()
                .any(|span| matches!(span.category, PiiCategory::Secret))
        );
    }

    #[test]
    fn redact_fact_scrubs_subject_object_without_reextraction() {
        // Pins: redaction preserves LLM-produced structure instead of heuristic re-extraction.
        let fact = pii_fact("alice@example.com owns secret sk-live");
        let result = pii_result(&fact.summary, "alice@example.com", PiiCategory::Email);

        let redacted = redact_fact(&fact, &result);

        assert_eq!(redacted.subject, "[EMAIL_REDACTED]");
        assert_eq!(redacted.predicate, "owns");
        assert_eq!(redacted.object, "secret sk-live");
        assert_eq!(redacted.summary, "[EMAIL_REDACTED] owns secret sk-live");
        assert_eq!(redacted.source_chunk, fact.source_chunk);
        assert_eq!(redacted.scope_hint, fact.scope_hint);
        assert_eq!(redacted.confidence, fact.confidence);
    }

    #[test]
    fn redact_fact_recomputes_uid_from_redacted_parts() {
        // Pins: redacted fact dedup identity follows the redacted structured fields.
        let fact = pii_fact("alice@example.com owns secret sk-live");
        let result = pii_result(&fact.summary, "sk-live", PiiCategory::Secret);

        let redacted = redact_fact(&fact, &result);
        let hash = fact_hash(&redacted).expect("redacted fact hashes");

        assert_ne!(redacted.uid, fact.uid);
        assert_eq!(redacted.uid, crate::fact_uid_from_hash(&hash));
        assert_eq!(redacted.object, "secret [SECRET_REDACTED]");
    }

    #[test]
    fn redact_fact_replaces_field_value_when_summary_span_has_punctuation() {
        // Pins: field redaction works even when summary tokenization includes punctuation.
        let mut fact = pii_fact("User 00 uses contact email alice@example.com.");
        fact.subject = "User 00".to_string();
        fact.predicate = "uses contact email".to_string();
        fact.object = "alice@example.com".to_string();
        let result = pii_result(&fact.summary, "alice@example.com.", PiiCategory::Email);

        let redacted = redact_fact(&fact, &result);

        assert_eq!(
            redacted.summary,
            "User 00 uses contact email [EMAIL_REDACTED]"
        );
        assert_eq!(redacted.object, "[EMAIL_REDACTED]");
    }

    #[test]
    fn predicate_mapping_covers_every_generator_predicate() {
        // Pins: every predicate emitted by the memory eval generator has an intentional edge label.
        let cases = [
            ("cache_backend_conflict", EdgeLabel::RelatesTo),
            ("contact_email", EdgeLabel::RelatesTo),
            ("depends_on", EdgeLabel::DependsOn),
            ("deploy_target", EdgeLabel::RelatesTo),
            ("on_call_primary", EdgeLabel::RelatesTo),
            ("owned_by", EdgeLabel::OwnedBy),
            ("private_repository", EdgeLabel::RelatesTo),
            ("require_runbook", EdgeLabel::DependsOn),
            ("response_style", EdgeLabel::RelatesTo),
        ];

        for (predicate, expected) in cases {
            assert_eq!(fact_object_edge_label(predicate), expected, "{predicate}");
        }
        assert_eq!(
            fact_object_edge_label("uses contact email"),
            EdgeLabel::DependsOn
        );
        assert_eq!(fact_object_edge_label("is owned by"), EdgeLabel::OwnedBy);
    }

    #[test]
    fn subject_edge_stays_relates_to_object_edge_gets_typed_label() {
        // Pins: predicate semantics live only on the Fact-to-object edge.
        let turn = test_turn();
        let scope = turn_tenant_scope(&turn).expect("test turn carries tenant UUID scope");
        let entity_uid = uuid::Uuid::now_v7();
        let fact_uid = uuid::Uuid::now_v7();

        let subject = entity_fact_edge_intent(
            &turn,
            &scope,
            entity_uid,
            fact_uid,
            "subject",
            Some("the checkout service"),
        );
        let object = fact_entity_edge_intent(
            &turn,
            &scope,
            fact_uid,
            entity_uid,
            "object",
            "depends_on",
            Some("Lib Foo"),
        );

        assert_eq!(subject.label, EdgeLabel::RelatesTo);
        assert_eq!(object.label, EdgeLabel::DependsOn);
        assert_eq!(
            subject
                .properties
                .get("alias_mention")
                .and_then(|value| value.as_str()),
            Some("the checkout service")
        );
        assert_eq!(
            object
                .properties
                .get("alias_mention")
                .and_then(|value| value.as_str()),
            Some("Lib Foo")
        );
    }

    fn pii_fact(summary: &str) -> ExtractedFact {
        let mut fact = ExtractedFact {
            uid: uuid::Uuid::nil(),
            subject: "alice@example.com".to_string(),
            predicate: "owns".to_string(),
            object: "secret sk-live".to_string(),
            summary: summary.to_string(),
            source_chunk: 3,
            scope_hint: ExtractedFactScopeHint::User,
            confidence: Some(0.93),
        };
        let hash = fact_hash(&fact).expect("fact hashes");
        fact.uid = crate::fact_uid_from_hash(&hash);
        fact
    }

    fn pii_result(summary: &str, needle: &str, category: PiiCategory) -> PiiResult {
        let start = summary.find(needle).expect("needle present");
        PiiResult {
            class: PiiClass::Pii,
            spans: vec![PiiSpan::new(start, start + needle.len(), category, 0.99)],
            model_version: "test".to_string(),
            abstained: false,
        }
    }

    fn test_turn() -> crate::SessionTurn {
        let tenant_id = uuid::Uuid::from_u128(0x1000);
        let contact_id = uuid::Uuid::from_u128(0x2000);
        crate::SessionTurn {
            workspace_id: WorkspaceId::new(tenant_id.to_string()),
            user_id: UserId::new(contact_id.to_string()),
            session_id: SessionId(uuid::Uuid::now_v7()),
            turn_seq: 1,
            transcript: "user: checkout-service depends on libfoo".to_string(),
            dominant_pii_class: "none".to_string(),
            finalized_at: Utc::now(),
        }
    }
}
