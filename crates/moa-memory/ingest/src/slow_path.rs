//! Restate virtual object for slow-path graph-memory ingestion.

use std::time::Duration;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::{
    ClassifiedFact, Conflict, ContradictionContext, ContradictionDetector, EmbeddedFact,
    EntityResolutionPlan, EntityResolutionRequest, EntityResolver, ExtractedFact,
    ExtractedFactScopeHint, FactExtractor, HeuristicFactExtractor, IngestApplyReport, IngestCtx,
    IngestDecision, RrfPlusJudgeDetector, SessionTurn, chunk_turn, current_runtime,
    extraction_confidence_hint, fact_hash, fact_uid_from_hash, scoped_fact_uid,
    should_ingest_degraded,
};
use futures_util::{StreamExt, TryStreamExt, stream};
use moa_core::RlsContext;
use moa_core::{MoaConfig, traits::EmbeddingProvider};
use moa_db::ScopedConn;
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, NodeLabel, NodeWriteIntent, PostgresGraphStore,
};
use moa_memory_pii::{PiiClassifier, PiiResult, PiiSpan, redact_text, redaction_replacement};
use moa_memory_vector::{VectorStore, VectorStoreFactory};
use restate_sdk::prelude::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

const DONE_KEY_PREFIX: &str = "done";
const CHUNK_TARGET_TOKENS: usize = 700;
const CHUNK_OVERLAP_TOKENS: usize = 100;
/// Maximum concurrent PII classification requests issued for one turn's facts.
const PII_CLASSIFY_CONCURRENCY: usize = 8;
/// Maximum concurrent contradiction pipelines evaluated for one turn's facts.
const CONTRADICTION_CONCURRENCY: usize = 8;

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
        let pii_classifier = runtime.pii_classifier();
        let embedder = runtime.embedder();
        let contradiction_detector = runtime.contradiction_detector();
        let vector_factory = runtime.vector_store_factory();
        let degraded = storage_partition_degraded(&pool, &turn).await?;
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
        let classify_pii = pii_classifier.clone();
        let classified = ctx
            .run(|| async move {
                classify_facts_with(classify_pii.as_ref(), &classify_facts_input)
                    .await
                    .map(Json::from)
            })
            .name("classify_pii")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let embed_input = classified.clone();
        let embed_embedder = embedder.clone();
        let embedded = ctx
            .run(|| async move {
                embed_batch_shared(embed_embedder.as_deref(), &embed_input)
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
        let contradiction_vector_factory = vector_factory.clone();
        let decisions = ctx
            .run(|| async move {
                detect_contradictions_with(
                    contradiction_detector.as_ref(),
                    contradiction_pool,
                    &contradiction_vector_factory,
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
        let upsert_vector_factory = vector_factory.clone();
        let report = ctx
            .run(|| async move {
                apply_decisions(
                    &upsert_pool,
                    &upsert_vector_factory,
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

/// Runs the slow-path ingestion steps directly in the current process for local/test hosts.
///
/// Hosts that call this helper must first install an ingestion runtime with
/// [`crate::install_runtime_with_pool`]. Restate handlers should continue to use
/// [`IngestionVO::ingest_turn`] so the step journal remains durable. The helper
/// takes a transaction-scoped Postgres advisory fence before graph/vector writes
/// so duplicate direct callers for the same turn serialize across pods.
pub async fn ingest_turn_direct(turn: SessionTurn) -> Result<IngestApplyReport, HandlerError> {
    let runtime = current_runtime().map_err(HandlerError::from)?;
    ingest_turn_direct_with_pool_and_pii(
        DirectIngestDeps {
            pool: runtime.pool().clone(),
            pii_classifier: runtime.pii_classifier(),
            embedder: runtime.embedder(),
            extractor: runtime.extractor(),
            entity_resolver: runtime.entity_resolver(),
            entity_blocking_embedder: runtime.entity_blocking_embedder(),
            contradiction_detector: runtime.contradiction_detector(),
            vector_factory: runtime.vector_store_factory(),
        },
        turn,
    )
    .await
}

/// Runs the slow-path ingestion steps directly against an explicit Postgres pool for local/tests.
///
/// This is intended for embedded hosts that own more than one pool in the same
/// process, such as integration tests. Restate handlers should continue to use
/// [`IngestionVO::ingest_turn`] so the step journal remains durable. The helper
/// takes a transaction-scoped Postgres advisory fence before graph/vector writes
/// so duplicate direct callers for the same turn serialize across pods.
pub async fn ingest_turn_direct_with_pool(
    pool: PgPool,
    turn: SessionTurn,
) -> Result<IngestApplyReport, HandlerError> {
    let config = MoaConfig::load_from_env().map_err(HandlerError::from)?;
    let contradiction_detector = Arc::new(RrfPlusJudgeDetector::from_config_or_heuristic(&config));
    let vector_factory = VectorStoreFactory::from_config(&config);
    let pii_classifier =
        crate::ctx::build_shared_pii_classifier(config.memory.pii_service_url.as_deref());
    let embedder = crate::ctx::build_shared_embedder(&config.providers.cohere.api_key);
    ingest_turn_direct_with_pool_and_pii(
        DirectIngestDeps {
            pool,
            pii_classifier,
            embedder,
            extractor: Arc::new(HeuristicFactExtractor),
            entity_resolver: Arc::new(EntityResolver::deterministic_for_app_role()),
            entity_blocking_embedder: None,
            contradiction_detector,
            vector_factory,
        },
        turn,
    )
    .await
}

struct DirectIngestDeps {
    pool: PgPool,
    pii_classifier: Arc<dyn PiiClassifier>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    extractor: Arc<dyn FactExtractor>,
    entity_resolver: Arc<EntityResolver>,
    entity_blocking_embedder: Option<Arc<dyn EmbeddingProvider>>,
    contradiction_detector: Arc<dyn ContradictionDetector>,
    vector_factory: VectorStoreFactory,
}

async fn ingest_turn_direct_with_pool_and_pii(
    deps: DirectIngestDeps,
    turn: SessionTurn,
) -> Result<IngestApplyReport, HandlerError> {
    let DirectIngestDeps {
        pool,
        pii_classifier,
        embedder,
        extractor,
        entity_resolver,
        entity_blocking_embedder,
        contradiction_detector,
        vector_factory,
    } = deps;
    let direct_claim = claim_direct_ingest_turn(&pool, &turn).await?;
    let degraded = storage_partition_degraded(&pool, &turn).await?;
    if degraded && !should_ingest_degraded(&turn) {
        let report = IngestApplyReport {
            skipped: 1,
            ..IngestApplyReport::default()
        };
        direct_claim.release().await?;
        return Ok(report);
    }

    let chunks =
        chunk_turn(&turn, CHUNK_TARGET_TOKENS, CHUNK_OVERLAP_TOKENS).map_err(HandlerError::from)?;
    let extracted = extractor
        .extract(&chunks)
        .await
        .map_err(HandlerError::from)?;
    let classified = classify_facts_with(pii_classifier.as_ref(), &extracted).await?;
    let embedded = embed_batch_shared(embedder.as_deref(), &classified).await?;
    let decisions = detect_contradictions_with(
        contradiction_detector.as_ref(),
        pool.clone(),
        &vector_factory,
        &turn,
        &embedded,
    )
    .await?;
    let report = apply_decisions(
        &pool,
        &vector_factory,
        entity_resolver.as_ref(),
        entity_blocking_embedder,
        &turn,
        &decisions,
    )
    .await?;
    direct_claim.release().await?;
    Ok(report)
}

/// Runs the slow-path ingestion steps with explicit deterministic dependencies for local/tests.
///
/// This helper is intended for integration tests that need to exercise the M10 pipeline without
/// depending on process-global environment variables or billed provider calls. It takes a
/// transaction-scoped Postgres advisory fence before graph/vector writes so duplicate direct
/// callers for the same turn serialize across pods.
pub async fn ingest_turn_direct_with_ctx(
    ctx: IngestCtx,
    turn: SessionTurn,
) -> Result<IngestApplyReport, HandlerError> {
    let direct_claim = claim_direct_ingest_turn(&ctx.pool, &turn).await?;
    let degraded = storage_partition_degraded(&ctx.pool, &turn).await?;
    if degraded && !should_ingest_degraded(&turn) {
        let report = IngestApplyReport {
            skipped: 1,
            ..IngestApplyReport::default()
        };
        direct_claim.release().await?;
        return Ok(report);
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
    let vector_factory = VectorStoreFactory::default();
    let decisions = detect_contradictions_with(
        ctx.contradict.as_ref(),
        ctx.pool.clone(),
        &vector_factory,
        &turn,
        &embedded,
    )
    .await?;
    let entity_blocking_embedder = ctx.entity_blocking_enabled.then(|| ctx.embedder.clone());
    let report = apply_decisions(
        &ctx.pool,
        &vector_factory,
        ctx.entity_resolver.as_ref(),
        entity_blocking_embedder,
        &turn,
        &decisions,
    )
    .await?;
    direct_claim.release().await?;
    Ok(report)
}

/// Builds the object key used to serialize ingestion per workspace/session.
#[must_use]
pub fn ingestion_object_key(turn: &SessionTurn) -> String {
    format!("{}:{}", turn.tenant_id, turn.session_id)
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

async fn classify_facts_with(
    classifier: &dyn PiiClassifier,
    facts: &[ExtractedFact],
) -> Result<Vec<ClassifiedFact>, HandlerError> {
    // Classify a turn's facts with bounded concurrency. The privacy-filter
    // sidecar exposes only a single-text endpoint, so facts are classified as
    // concurrent requests capped at `PII_CLASSIFY_CONCURRENCY`. `buffered`
    // preserves fact order, keeping redaction and downstream dedup identity
    // deterministic regardless of which request completes first.
    let summaries = facts
        .iter()
        .map(|fact| fact.summary.clone())
        .collect::<Vec<_>>();
    let results: Vec<PiiResult> = stream::iter(summaries)
        .map(|summary| async move { classifier.classify(&summary).await })
        .buffered(PII_CLASSIFY_CONCURRENCY)
        .try_collect()
        .await
        .map_err(HandlerError::from)?;

    Ok(facts
        .iter()
        .zip(results)
        .map(|(fact, result)| {
            let redacted_fact = redact_fact(fact, &result);
            ClassifiedFact {
                fact: redacted_fact,
                pii_class: result.class,
                pii_spans: result.spans,
            }
        })
        .collect())
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
        let replacement = redaction_replacement(span.category);
        redacted = redacted.replace(source_text, replacement);
        let trimmed_source = source_text.trim_matches(|character: char| {
            character.is_ascii_punctuation() && !matches!(character, '[' | ']')
        });
        if !trimmed_source.is_empty() && trimmed_source != source_text {
            let trimmed_replacement = redaction_replacement(span.category);
            redacted = redacted.replace(trimmed_source, trimmed_replacement);
        }
    }
    redacted
}

async fn embed_batch_shared(
    embedder: Option<&dyn EmbeddingProvider>,
    facts: &[ClassifiedFact],
) -> Result<Vec<EmbeddedFact>, HandlerError> {
    match embedder {
        Some(embedder) => embed_batch_with(embedder, facts).await,
        None => Ok(facts
            .iter()
            .cloned()
            .map(|classified| EmbeddedFact {
                classified,
                embedding: None,
                embedding_model: None,
                embedding_model_version: None,
            })
            .collect()),
    }
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
            embedding_model: Some(embedder.model_id().to_string()),
            embedding_model_version: Some(embedder.model_version()),
        })
        .collect())
}

async fn detect_contradictions_with(
    detector: &dyn ContradictionDetector,
    pool: PgPool,
    vector_factory: &VectorStoreFactory,
    turn: &SessionTurn,
    embedded: &[EmbeddedFact],
) -> Result<Vec<IngestDecision>, HandlerError> {
    // Resolve one vector store per distinct scope up front (cheap, cached) so the
    // per-fact contradiction pipelines can run concurrently without sharing a
    // mutable cache across tasks.
    let mut vector_cache = ConfiguredVectorStoreCache::default();
    let mut scoped_vectors = Vec::with_capacity(embedded.len());
    for fact in embedded {
        let scope = fact_scope(turn, fact);
        let vector = vector_cache
            .configured_for_scope(&pool, vector_factory, scope.clone(), true)
            .await
            .map_err(HandlerError::from)?;
        scoped_vectors.push((scope, vector));
    }

    // Contradiction detection reads only already-committed graph state, so the
    // per-fact pipelines are independent and evaluated with bounded concurrency.
    // `buffered` preserves fact order so the resulting decisions apply
    // deterministically.
    let pipelines = embedded
        .iter()
        .cloned()
        .zip(scoped_vectors)
        .map(|(fact, (scope, vector))| {
            let pool = pool.clone();
            async move {
                let ctx = ContradictionContext::for_app_role(pool, scope, vector);
                detector.check_one_slow(&fact, &ctx).await
            }
        })
        .collect::<Vec<_>>();
    let conflicts: Vec<Conflict> = stream::iter(pipelines)
        .buffered(CONTRADICTION_CONCURRENCY)
        .try_collect()
        .await
        .map_err(HandlerError::from)?;

    Ok(embedded
        .iter()
        .cloned()
        .zip(conflicts)
        .map(|(fact, conflict)| decision_from_conflict(conflict, fact))
        .collect())
}

struct DirectIngestClaim {
    tx: Transaction<'static, Postgres>,
}

impl DirectIngestClaim {
    async fn release(self) -> Result<(), HandlerError> {
        self.tx.commit().await.map_err(HandlerError::from)
    }
}

async fn claim_direct_ingest_turn(
    pool: &PgPool,
    turn: &SessionTurn,
) -> Result<DirectIngestClaim, HandlerError> {
    let lock_key = direct_ingest_claim_key(turn);
    let mut tx = pool.begin().await.map_err(HandlerError::from)?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(lock_key)
        .execute(&mut *tx)
        .await
        .map_err(HandlerError::from)?;
    Ok(DirectIngestClaim { tx })
}

fn direct_ingest_claim_key(turn: &SessionTurn) -> i64 {
    let mut hasher = Sha256::new();
    hasher.update(b"moa:memory:direct_ingest:v1");
    hasher.update(turn.tenant_id.0.as_bytes());
    hasher.update(turn.session_id.0.as_bytes());
    hasher.update(turn.turn_seq.to_be_bytes());
    let digest = hasher.finalize();
    let mut key_bytes = [0_u8; 8];
    key_bytes.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(key_bytes)
}

fn decision_from_conflict(conflict: Conflict, fact: EmbeddedFact) -> IngestDecision {
    match conflict {
        Conflict::Insert | Conflict::Indeterminate => IngestDecision::Insert { fact },
        Conflict::Supersede(old_uid) => IngestDecision::Supersede { old_uid, fact },
        Conflict::Duplicate(fact_uid) => IngestDecision::SkipDuplicate { fact_uid },
    }
}

fn decision_scope(turn: &SessionTurn, decision: &IngestDecision) -> RlsContext {
    match decision_fact(decision) {
        Some(fact) => fact_scope(turn, fact),
        None => turn_default_scope(turn),
    }
}

fn fact_scope(turn: &SessionTurn, fact: &EmbeddedFact) -> RlsContext {
    match fact.classified.fact.scope_hint {
        ExtractedFactScopeHint::Contact => turn_default_scope(turn),
        ExtractedFactScopeHint::Tenant => turn_tenant_scope(turn),
    }
}

fn turn_tenant_scope(turn: &SessionTurn) -> RlsContext {
    RlsContext::tenant(turn.tenant_id)
}

fn turn_default_scope(turn: &SessionTurn) -> RlsContext {
    match turn.contact_id {
        Some(contact_id) => RlsContext::contact(turn.tenant_id, contact_id),
        None => turn_tenant_scope(turn),
    }
}

async fn apply_decisions(
    pool: &PgPool,
    vector_factory: &VectorStoreFactory,
    entity_resolver: &EntityResolver,
    entity_blocking_embedder: Option<Arc<dyn EmbeddingProvider>>,
    turn: &SessionTurn,
    decisions: &[IngestDecision],
) -> Result<IngestApplyReport, HandlerError> {
    let mut report = IngestApplyReport::default();
    // Embed every distinct entity name for the whole turn's facts in one provider
    // call up front, so per-fact entity resolution reuses the precomputed vectors
    // instead of issuing one batch-size-one embed per subject and object.
    let entity_embeddings =
        precompute_entity_embeddings(entity_blocking_embedder.as_deref(), decisions).await?;
    let mut deps = ApplyDecisionDeps {
        pool,
        vector_factory,
        vector_cache: ConfiguredVectorStoreCache::default(),
        entity_resolver,
        entity_blocking_embedder,
        entity_embeddings,
    };

    let mut drain_scopes: HashMap<String, RlsContext> = HashMap::new();
    for decision in decisions {
        let scope = decision_scope(turn, decision);
        match apply_one_decision(&mut deps, &scope, turn, decision).await {
            Ok(ApplyOutcome::Inserted) => {
                report.inserted += 1;
                record_drain_scope(&mut drain_scopes, &scope);
            }
            Ok(ApplyOutcome::Superseded) => {
                report.superseded += 1;
                record_drain_scope(&mut drain_scopes, &scope);
            }
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

    drain_external_vector_sync(pool, vector_factory, drain_scopes).await;

    Ok(report)
}

/// Records one write scope for a single post-batch external vector-sync drain.
///
/// Scopes are deduplicated by storage partition because the outbox drain claims
/// rows per storage partition, covering every scope tier within it.
fn record_drain_scope(drain_scopes: &mut HashMap<String, RlsContext>, scope: &RlsContext) {
    drain_scopes
        .entry(scope.storage_partition_id().to_string())
        .or_insert_with(|| scope.clone());
}

/// Drains the external vector-sync outbox once per storage partition after a turn.
///
/// Partitions whose vector backend is pgvector never enqueue outbox rows, so the
/// drain is skipped entirely for them; only partitions with an external backend
/// (for example Turbopuffer) pay the single post-batch drain.
async fn drain_external_vector_sync(
    pool: &PgPool,
    vector_factory: &VectorStoreFactory,
    drain_scopes: HashMap<String, RlsContext>,
) {
    for (storage_partition_id, scope) in drain_scopes {
        match vector_factory
            .partition_uses_external_backend(pool, &storage_partition_id)
            .await
        {
            Ok(true) => {
                let backend = vector_factory.transactional_graph_backend(pool.clone(), scope, true);
                if let Err(error) = backend.sync_post_commit().await {
                    tracing::warn!(
                        error = %error,
                        storage_partition_id = %storage_partition_id,
                        "post-batch vector sync drain failed; queued rows remain pending"
                    );
                }
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(
                error = %error,
                storage_partition_id = %storage_partition_id,
                "failed to resolve vector backend for post-batch drain; queued rows remain pending"
            ),
        }
    }
}

struct ApplyDecisionDeps<'a> {
    pool: &'a PgPool,
    vector_factory: &'a VectorStoreFactory,
    vector_cache: ConfiguredVectorStoreCache,
    entity_blocking_embedder: Option<Arc<dyn EmbeddingProvider>>,
    entity_resolver: &'a EntityResolver,
    /// Precomputed embeddings keyed by normalized entity name for this turn's facts.
    entity_embeddings: HashMap<String, Vec<f32>>,
}

async fn apply_one_decision(
    deps: &mut ApplyDecisionDeps<'_>,
    scope: &RlsContext,
    turn: &SessionTurn,
    decision: &IngestDecision,
) -> Result<ApplyOutcome, HandlerError> {
    let Some(fact) = decision_fact(decision) else {
        return Ok(ApplyOutcome::Skipped);
    };
    let entity_vector = if deps.entity_blocking_embedder.is_some() {
        Some(
            deps.vector_cache
                .configured_for_scope(deps.pool, deps.vector_factory, scope.clone(), true)
                .await
                .map_err(HandlerError::from)?,
        )
    } else {
        None
    };
    let scoped_resolver = deps
        .entity_blocking_embedder
        .clone()
        .zip(entity_vector)
        .map(|(embedder, vector)| {
            deps.entity_resolver
                .clone()
                .with_embedding_blocking(embedder, vector, 0.80)
        });
    let resolver = scoped_resolver.as_ref().unwrap_or(deps.entity_resolver);
    let graph = graph_store(
        deps.pool.clone(),
        scope.clone(),
        fact,
        deps.vector_factory,
        scoped_resolver.is_some(),
    );
    apply_one_decision_with_graph(
        deps.pool,
        scope,
        &graph,
        resolver,
        turn,
        decision,
        &deps.entity_embeddings,
    )
    .await
}

async fn apply_one_decision_with_graph(
    pool: &PgPool,
    scope: &RlsContext,
    store: &PostgresGraphStore,
    entity_resolver: &EntityResolver,
    turn: &SessionTurn,
    decision: &IngestDecision,
    entity_embeddings: &HashMap<String, Vec<f32>>,
) -> Result<ApplyOutcome, HandlerError> {
    let Some(fact) = decision_fact(decision) else {
        return Ok(ApplyOutcome::Skipped);
    };
    let hash = fact_hash(&fact.classified.fact).map_err(HandlerError::from)?;
    if dedup_fact_uid(pool, scope, turn, &hash).await?.is_some() {
        return Ok(ApplyOutcome::Skipped);
    }

    // Entity resolution reads run before the transaction and yield node-create
    // intents; the actual writes join the fact's transaction below.
    let entities =
        resolve_fact_entities(pool, scope, entity_resolver, turn, fact, entity_embeddings).await?;
    let fact_uid = scoped_fact_uid(&turn.tenant_id, &turn.session_id, turn.turn_seq, &hash);

    // Apply the entity nodes, the fact node, both entity edges, and the dedup row
    // in one scoped transaction via the in-conn write primitives, instead of the
    // previous separate transaction per write (entity nodes previously committed
    // in their own transactions). The external vector-sync outbox is drained once
    // per turn by the caller (`drain_external_vector_sync`).
    let mut conn = ScopedConn::begin_as_app(pool, scope, true)
        .await
        .map_err(HandlerError::from)?;
    write_entity_nodes_in_conn(store, conn.as_mut(), &entities).await?;
    let (uid, outcome) = match decision {
        IngestDecision::Insert { fact } => {
            let uid = moa_memory_graph::write::create_node_in_conn(
                store,
                conn.as_mut(),
                node_intent(turn, scope, fact, &hash, fact_uid),
            )
            .await
            .map_err(HandlerError::from)?;
            (uid, ApplyOutcome::Inserted)
        }
        IngestDecision::Supersede { old_uid, fact } => {
            let uid = moa_memory_graph::write::supersede_node_in_conn(
                store,
                conn.as_mut(),
                *old_uid,
                node_intent(turn, scope, fact, &hash, fact_uid),
            )
            .await
            .map_err(HandlerError::from)?;
            (uid, ApplyOutcome::Superseded)
        }
        IngestDecision::SkipDuplicate { .. } => return Ok(ApplyOutcome::Skipped),
    };
    write_fact_entity_edges_in_conn(store, conn.as_mut(), turn, scope, uid, fact, &entities)
        .await?;
    insert_dedup_in_conn(conn.as_mut(), scope, turn, &hash, uid).await?;
    conn.commit().await.map_err(HandlerError::from)?;
    Ok(outcome)
}

#[derive(Default)]
struct ConfiguredVectorStoreCache {
    stores: HashMap<(RlsContext, bool), Arc<dyn VectorStore>>,
}

impl ConfiguredVectorStoreCache {
    async fn configured_for_scope(
        &mut self,
        pool: &PgPool,
        vector_factory: &VectorStoreFactory,
        scope: RlsContext,
        assume_app_role: bool,
    ) -> moa_memory_vector::Result<Arc<dyn VectorStore>> {
        let key = (scope.clone(), assume_app_role);
        if let Some(store) = self.stores.get(&key) {
            return Ok(store.clone());
        }
        let store = vector_factory
            .configured_for_scope(pool, scope, assume_app_role)
            .await?;
        self.stores.insert(key, store.clone());
        Ok(store)
    }
}

fn graph_store(
    pool: PgPool,
    scope: RlsContext,
    fact: &EmbeddedFact,
    vector_factory: &VectorStoreFactory,
    needs_entity_vector_writes: bool,
) -> PostgresGraphStore {
    // Attach only the transactional pgvector store, not the post-commit external
    // sync hook. Ingesting a turn writes many nodes and edges; draining the
    // external vector-sync outbox after every individual write is wasteful (and
    // for pgvector-only partitions the outbox is always empty). The outbox is
    // drained once per storage partition after the whole batch commits — see
    // `apply_decisions` and `drain_external_vector_sync`.
    let store = PostgresGraphStore::scoped_for_app_role(pool.clone(), scope.clone());
    if fact.embedding.is_some() || needs_entity_vector_writes {
        let vector_backend = vector_factory.transactional_graph_backend(pool, scope, true);
        store.with_vector_store(vector_backend.vector_store())
    } else {
        store
    }
}

fn node_intent(
    turn: &SessionTurn,
    scope: &RlsContext,
    fact: &EmbeddedFact,
    hash: &[u8],
    fact_uid: uuid::Uuid,
) -> NodeWriteIntent {
    let extracted = &fact.classified.fact;
    let storage_partition_id = scope_storage_partition_id(scope);
    let contact_id = scope_user_id(scope);
    let scope_tier = scope.tier_str();
    NodeWriteIntent {
        uid: fact_uid,
        label: NodeLabel::Fact,
        storage_partition_id: storage_partition_id.clone(),
        contact_id: contact_id.clone(),
        scope: scope_tier.to_string(),
        name: extracted.subject.clone(),
        properties: json!({
            "uid": fact_uid.to_string(),
            "extracted_uid": extracted.uid.to_string(),
            "storage_partition_id": storage_partition_id,
            "user_id": contact_id,
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
        embedding_text: None,
        actor_id: turn_actor_id(turn),
        actor_kind: turn_actor_kind(turn).to_string(),
    }
}

fn scope_storage_partition_id(scope: &RlsContext) -> Option<String> {
    Some(scope.tenant_id().to_string())
}

fn scope_user_id(scope: &RlsContext) -> Option<String> {
    scope.contact_id().map(|contact_id| contact_id.to_string())
}

fn turn_actor_id(turn: &SessionTurn) -> String {
    turn.contact_id
        .map(|contact_id| contact_id.to_string())
        .unwrap_or_else(|| turn.tenant_id.to_string())
}

fn turn_actor_kind(turn: &SessionTurn) -> &'static str {
    if turn.contact_id.is_some() {
        "contact"
    } else {
        "system"
    }
}

#[derive(Debug, Clone)]
struct ResolvedFactEntities {
    subject: EntityResolutionPlan,
    object: EntityResolutionPlan,
}

async fn resolve_fact_entities(
    pool: &PgPool,
    scope: &RlsContext,
    entity_resolver: &EntityResolver,
    turn: &SessionTurn,
    fact: &EmbeddedFact,
    entity_embeddings: &HashMap<String, Vec<f32>>,
) -> Result<ResolvedFactEntities, HandlerError> {
    let extracted = &fact.classified.fact;
    let confidence = extracted_confidence(extracted);
    let actor_id = turn_actor_id(turn);
    let actor_kind = turn_actor_kind(turn);
    let subject = entity_resolver
        .plan_resolution(
            pool,
            EntityResolutionRequest {
                scope,
                name: &extracted.subject,
                pii_class: fact.classified.pii_class,
                confidence,
                valid_from: turn.finalized_at,
                actor_id: &actor_id,
                actor_kind,
                precomputed_embedding: entity_embedding_for(entity_embeddings, &extracted.subject),
            },
        )
        .await
        .map_err(HandlerError::from)?;
    let object = entity_resolver
        .plan_resolution(
            pool,
            EntityResolutionRequest {
                scope,
                name: &extracted.object,
                pii_class: fact.classified.pii_class,
                confidence,
                valid_from: turn.finalized_at,
                actor_id: &actor_id,
                actor_kind,
                precomputed_embedding: entity_embedding_for(entity_embeddings, &extracted.object),
            },
        )
        .await
        .map_err(HandlerError::from)?;

    Ok(ResolvedFactEntities { subject, object })
}

/// Looks up the precomputed embedding for one entity mention by its normalized name.
fn entity_embedding_for<'a>(
    entity_embeddings: &'a HashMap<String, Vec<f32>>,
    name: &str,
) -> Option<&'a [f32]> {
    entity_embeddings
        .get(&crate::entity_resolution::normalize_entity_name(name))
        .map(Vec::as_slice)
}

/// Writes the resolved entities' node-create intents into the fact's transaction.
///
/// Only mentions that resolved to a new entity carry a create intent. Intents are
/// deduplicated by uid so a fact whose subject and object normalize to the same
/// entity writes that node once instead of hitting a primary-key conflict.
async fn write_entity_nodes_in_conn(
    store: &PostgresGraphStore,
    conn: &mut PgConnection,
    entities: &ResolvedFactEntities,
) -> Result<(), HandlerError> {
    let mut written: HashSet<uuid::Uuid> = HashSet::new();
    for plan in [&entities.subject, &entities.object] {
        if let Some(intent) = plan.create.as_ref()
            && written.insert(intent.uid)
        {
            moa_memory_graph::write::create_node_in_conn(store, &mut *conn, intent.clone())
                .await
                .map_err(HandlerError::from)?;
        }
    }
    Ok(())
}

/// Precomputes the embedding of every distinct entity name in the batch in one call.
///
/// Returns a map keyed by normalized entity name. When embedding blocking is
/// disabled (`embedder` is `None`) the map is empty and resolution stays
/// deterministic. A name whose normalized form is byte-identical to a fact's
/// summary reuses that fact's already-computed embedding (embedding the same text
/// yields the same vector), avoiding a redundant provider input.
async fn precompute_entity_embeddings(
    embedder: Option<&dyn EmbeddingProvider>,
    decisions: &[IngestDecision],
) -> Result<HashMap<String, Vec<f32>>, HandlerError> {
    let Some(embedder) = embedder else {
        return Ok(HashMap::new());
    };
    let mut map: HashMap<String, Vec<f32>> = HashMap::new();
    let mut pending: Vec<String> = Vec::new();
    let mut pending_seen: HashSet<String> = HashSet::new();
    for decision in decisions {
        let Some(fact) = decision_fact(decision) else {
            continue;
        };
        let extracted = &fact.classified.fact;
        for raw in [extracted.subject.as_str(), extracted.object.as_str()] {
            let normalized = crate::entity_resolution::normalize_entity_name(raw);
            if normalized.is_empty()
                || map.contains_key(&normalized)
                || pending_seen.contains(&normalized)
            {
                continue;
            }
            if let Some(embedding) = fact
                .embedding
                .as_ref()
                .filter(|_| normalized == extracted.summary)
            {
                map.insert(normalized, embedding.clone());
            } else {
                pending_seen.insert(normalized.clone());
                pending.push(normalized);
            }
        }
    }
    if !pending.is_empty() {
        let embeddings = embedder.embed(&pending).await.map_err(HandlerError::from)?;
        if embeddings.len() != pending.len() {
            return Err(TerminalError::new(format!(
                "entity name embedding count {} does not match {} requested names",
                embeddings.len(),
                pending.len()
            ))
            .into());
        }
        for (name, embedding) in pending.into_iter().zip(embeddings) {
            map.insert(name, embedding);
        }
    }
    Ok(map)
}

fn extracted_confidence(fact: &ExtractedFact) -> f64 {
    fact.confidence
        .unwrap_or_else(|| extraction_confidence_hint(&fact.summary))
        .clamp(0.0, 1.0)
}

async fn write_fact_entity_edges_in_conn(
    store: &PostgresGraphStore,
    conn: &mut PgConnection,
    turn: &SessionTurn,
    scope: &RlsContext,
    fact_uid: uuid::Uuid,
    fact: &EmbeddedFact,
    entities: &ResolvedFactEntities,
) -> Result<(), HandlerError> {
    moa_memory_graph::write::create_edge_in_conn(
        store,
        &mut *conn,
        entity_fact_edge_intent(
            turn,
            scope,
            entities.subject.resolved.uid,
            fact_uid,
            "subject",
            entities.subject.resolved.alias_mention.as_deref(),
        ),
    )
    .await
    .map_err(HandlerError::from)?;
    moa_memory_graph::write::create_edge_in_conn(
        store,
        &mut *conn,
        fact_entity_edge_intent(
            turn,
            scope,
            fact_uid,
            entities.object.resolved.uid,
            "object",
            &fact.classified.fact.predicate,
            entities.object.resolved.alias_mention.as_deref(),
        ),
    )
    .await
    .map_err(HandlerError::from)?;
    Ok(())
}

fn entity_fact_edge_intent(
    turn: &SessionTurn,
    scope: &RlsContext,
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
        valid_from: turn.finalized_at,
        properties: entity_edge_properties(turn, role, alias_mention),
        storage_partition_id: scope_storage_partition_id(scope),
        contact_id: scope_user_id(scope),
        scope: scope.tier_str().to_string(),
        actor_id: turn_actor_id(turn),
        actor_kind: turn_actor_kind(turn).to_string(),
    }
}

fn fact_entity_edge_intent(
    turn: &SessionTurn,
    scope: &RlsContext,
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
        valid_from: turn.finalized_at,
        properties: entity_edge_properties(turn, role, alias_mention),
        storage_partition_id: scope_storage_partition_id(scope),
        contact_id: scope_user_id(scope),
        scope: scope.tier_str().to_string(),
        actor_id: turn_actor_id(turn),
        actor_kind: turn_actor_kind(turn).to_string(),
    }
}

fn fact_object_edge_label(predicate: &str) -> EdgeLabel {
    let normalized = crate::entity_resolution::normalize_entity_name(predicate);
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

async fn storage_partition_degraded(
    pool: &PgPool,
    turn: &SessionTurn,
) -> Result<bool, HandlerError> {
    let scope = turn_tenant_scope(turn);
    let mut conn = ScopedConn::begin(pool, &scope)
        .await
        .map_err(HandlerError::from)?;
    let degraded = sqlx::query_scalar::<_, bool>(
        "SELECT slow_path_degraded FROM moa.storage_partition_state WHERE storage_partition_id = $1",
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
    scope: &RlsContext,
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
        WHERE storage_partition_id = $1
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

async fn insert_dedup_in_conn(
    conn: &mut PgConnection,
    scope: &RlsContext,
    turn: &SessionTurn,
    hash: &[u8],
    fact_uid: uuid::Uuid,
) -> Result<(), HandlerError> {
    let turn_seq = turn_seq_i64(turn)?;
    let user_id = scope_user_id(scope);
    sqlx::query(
        r#"
        INSERT INTO moa.ingest_dedup
            (storage_partition_id, user_id, session_id, turn_seq, fact_hash, fact_uid)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (storage_partition_id, session_id, turn_seq, fact_hash) DO NOTHING
        "#,
    )
    .bind(scope.tenant_id().to_string())
    .bind(user_id.as_deref())
    .bind(turn.session_id.0)
    .bind(turn_seq)
    .bind(hash)
    .bind(fact_uid)
    .execute(conn)
    .await
    .map_err(HandlerError::from)?;
    Ok(())
}

async fn write_dlq(
    pool: &PgPool,
    scope: &RlsContext,
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
            (storage_partition_id, user_id, session_id, turn_seq, payload, error, next_retry_at)
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::Utc;
    use moa_core::traits::EmbeddingProvider;
    use moa_core::{ContactId, ContextMessage, SessionId, TenantId};
    use moa_memory_graph::{EdgeLabel, PiiClass};
    use moa_memory_pii::{PiiCategory, PiiClassifier, PiiResult, PiiSpan, classify_heuristic};

    use super::{
        classify_facts_with, direct_ingest_claim_key, entity_fact_edge_intent,
        fact_entity_edge_intent, fact_object_edge_label, precompute_entity_embeddings, redact_fact,
        turn_tenant_scope, turn_transcript,
    };
    use crate::{
        ClassifiedFact, EmbeddedFact, ExtractedFact, ExtractedFactScopeHint, IngestDecision,
        fact_hash,
    };

    /// PII classifier that counts calls and echoes input so ordering can be pinned.
    struct CountingClassifier {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl PiiClassifier for CountingClassifier {
        async fn classify(&self, text: &str) -> moa_memory_pii::Result<PiiResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(PiiResult {
                class: PiiClass::None,
                spans: Vec::new(),
                model_version: text.to_string(),
                abstained: false,
            })
        }
    }

    /// Embedder that counts calls and records the last batch size for one-call pins.
    struct CountingEmbedder {
        calls: Arc<AtomicUsize>,
        last_batch_len: Arc<AtomicUsize>,
        dim: usize,
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for CountingEmbedder {
        fn model_id(&self) -> &str {
            "counting"
        }

        fn dimensions(&self) -> usize {
            self.dim
        }

        async fn embed(&self, inputs: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.last_batch_len.store(inputs.len(), Ordering::SeqCst);
            Ok(inputs
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    let mut vector = vec![0.0; self.dim];
                    vector[index % self.dim] = 1.0;
                    vector
                })
                .collect())
        }
    }

    fn embedded_decision(
        subject: &str,
        object: &str,
        summary: &str,
        embedding: Option<Vec<f32>>,
    ) -> IngestDecision {
        let mut fact = ExtractedFact {
            uid: uuid::Uuid::nil(),
            subject: subject.to_string(),
            predicate: "predicate".to_string(),
            object: object.to_string(),
            summary: summary.to_string(),
            source_chunk: 0,
            scope_hint: ExtractedFactScopeHint::Contact,
            confidence: Some(0.9),
        };
        let hash = fact_hash(&fact).expect("fact hashes");
        fact.uid = crate::fact_uid_from_hash(&hash);
        let embedding_model = embedding.as_ref().map(|_| "counting".to_string());
        IngestDecision::Insert {
            fact: EmbeddedFact {
                classified: ClassifiedFact {
                    fact,
                    pii_class: PiiClass::None,
                    pii_spans: Vec::new(),
                },
                embedding,
                embedding_model,
                embedding_model_version: Some(1),
            },
        }
    }

    #[tokio::test]
    async fn precompute_entity_embeddings_batches_one_call_dedupes_and_reuses_fact_embedding() {
        // Pins: entity-name embedding for a whole fact batch is a single deduped
        // provider call; a name whose normalized form equals a fact summary reuses
        // that fact's own embedding instead of re-embedding it (item: batch
        // entity-name embeddings).
        let calls = Arc::new(AtomicUsize::new(0));
        let batch_len = Arc::new(AtomicUsize::new(0));
        let embedder = CountingEmbedder {
            calls: calls.clone(),
            last_batch_len: batch_len.clone(),
            dim: 8,
        };
        let reuse_vector = vec![0.5; 8];
        let decisions = vec![
            embedded_decision("API Service", "DB", "api service uses db", None),
            embedded_decision("api-service", "cache", "api service uses cache", None),
            embedded_decision(
                "payments",
                "ignored object",
                "payments",
                Some(reuse_vector.clone()),
            ),
        ];

        let map = precompute_entity_embeddings(Some(&embedder), &decisions)
            .await
            .expect("precompute entity embeddings");

        // One batched provider call for the four distinct non-reused names
        // ("api service", "db", "cache", "ignored object"); "payments" reuses the
        // fact embedding because its normalized name equals the fact summary.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(batch_len.load(Ordering::SeqCst), 4);
        assert_eq!(map.len(), 5);
        assert_eq!(map.get("payments"), Some(&reuse_vector));
        assert!(map.contains_key("api service"));
        assert!(map.contains_key("ignored object"));
    }

    #[tokio::test]
    async fn precompute_entity_embeddings_is_empty_without_embedder() {
        // Pins: entity embedding blocking stays gated off when no embedder is
        // configured (Cohere key absent), so no vectors are precomputed.
        let decisions = vec![embedded_decision("api", "db", "api uses db", None)];

        let map = precompute_entity_embeddings(None, &decisions)
            .await
            .expect("precompute without embedder");

        assert!(map.is_empty());
    }

    fn plain_fact(summary: &str) -> ExtractedFact {
        let mut fact = ExtractedFact {
            uid: uuid::Uuid::nil(),
            subject: "subject".to_string(),
            predicate: "predicate".to_string(),
            object: "object".to_string(),
            summary: summary.to_string(),
            source_chunk: 0,
            scope_hint: ExtractedFactScopeHint::Contact,
            confidence: Some(0.9),
        };
        let hash = fact_hash(&fact).expect("fact hashes");
        fact.uid = crate::fact_uid_from_hash(&hash);
        fact
    }

    #[tokio::test]
    async fn classify_facts_with_issues_one_call_per_fact_in_order() {
        // Pins: batch PII classification issues exactly one request per fact and
        // preserves fact order regardless of completion order (item: batch classify).
        let calls = Arc::new(AtomicUsize::new(0));
        let classifier = CountingClassifier {
            calls: calls.clone(),
        };
        let facts = (0..12)
            .map(|index| plain_fact(&format!("fact number {index}")))
            .collect::<Vec<_>>();

        let classified = classify_facts_with(&classifier, &facts)
            .await
            .expect("classify batch succeeds");

        assert_eq!(calls.load(Ordering::SeqCst), facts.len());
        assert_eq!(classified.len(), facts.len());
        for (index, entry) in classified.iter().enumerate() {
            assert_eq!(entry.fact.summary, format!("fact number {index}"));
            assert_eq!(entry.pii_class, PiiClass::None);
        }
    }

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
        let scope = turn_tenant_scope(&turn);
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

    #[test]
    fn duplicate_direct_ingest_attempts_share_claim_key() {
        // Pins: direct slow-path callers contend on one Postgres claim before graph/vector writes.
        let turn = test_turn();
        let duplicate = turn.clone();
        let mut next_turn = turn.clone();
        next_turn.turn_seq += 1;
        let mut other_session = turn.clone();
        other_session.session_id = SessionId(uuid::Uuid::now_v7());

        assert_eq!(
            direct_ingest_claim_key(&turn),
            direct_ingest_claim_key(&duplicate)
        );
        assert_ne!(
            direct_ingest_claim_key(&turn),
            direct_ingest_claim_key(&next_turn)
        );
        assert_ne!(
            direct_ingest_claim_key(&turn),
            direct_ingest_claim_key(&other_session)
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
            scope_hint: ExtractedFactScopeHint::Contact,
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
        let tenant_id = TenantId(uuid::Uuid::from_u128(0x1000));
        let contact_id = ContactId(uuid::Uuid::from_u128(0x2000));
        crate::SessionTurn {
            tenant_id,
            contact_id: Some(contact_id),
            session_id: SessionId(uuid::Uuid::now_v7()),
            turn_seq: 1,
            transcript: "user: checkout-service depends on libfoo".to_string(),
            dominant_pii_class: "none".to_string(),
            finalized_at: Utc::now(),
        }
    }
}
