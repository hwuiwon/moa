//! Slow-path graph-memory ingestion algorithms and explicit runtime stages.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::{
    ClassifiedFact, Conflict, ContradictionContext, ContradictionDetector, EmbeddedFact,
    EntityResolutionPlan, EntityResolutionRequest, EntityResolver, Error, ExtractedFact,
    ExtractedFactScopeHint, FactExtractor, HeuristicFactExtractor, IngestApplyReport, IngestCtx,
    IngestDecision, IngestRuntime, RrfPlusJudgeDetector, SessionTurn, TurnChunk, chunk_turn,
    extraction_confidence_hint, fact_hash, fact_uid_from_hash, scoped_fact_uid,
    should_ingest_degraded,
};
use futures_util::{StreamExt, TryStreamExt, stream};
use moa_config::MoaConfig;
use moa_core::traits::EmbeddingProvider;
use moa_core::types::memory::RlsContext;
use moa_crypto::KeyManagementProvider;
use moa_db::ScopedConn;
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, NodeLabel, NodeWriteIntent, PostgresGraphStore,
};
use moa_memory_pii::{PiiClassifier, PiiResult, PiiSpan, redact_text, redaction_replacement};
use moa_memory_types::{FactEdgeLabel, normalize_entity_name};
use moa_memory_vector::{VectorStore, VectorStoreFactory};
use restate_sdk::prelude::{HandlerError, TerminalError};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

const CHUNK_TARGET_TOKENS: usize = 700;
const CHUNK_OVERLAP_TOKENS: usize = 100;
/// Maximum concurrent PII classification requests issued for one turn's facts.
const PII_CLASSIFY_CONCURRENCY: usize = 8;
/// Maximum concurrent contradiction pipelines evaluated for one turn's facts.
const CONTRADICTION_CONCURRENCY: usize = 8;

/// Cohesive slow-path stage runner over one explicitly owned runtime.
#[derive(Clone)]
pub struct SlowPathIngestor {
    runtime: Arc<IngestRuntime>,
}

impl SlowPathIngestor {
    /// Creates a stage runner over the runtime owned by the host composition root.
    #[must_use]
    pub fn new(runtime: Arc<IngestRuntime>) -> Self {
        Self { runtime }
    }

    /// Returns whether a degraded partition should skip this turn.
    pub async fn should_skip_degraded(&self, turn: &SessionTurn) -> Result<bool, HandlerError> {
        Ok(storage_partition_degraded(self.runtime.pool(), turn).await?
            && !should_ingest_degraded(turn))
    }

    /// Deterministically chunks one finalized turn.
    pub fn chunk(&self, turn: &SessionTurn) -> Result<Vec<TurnChunk>, HandlerError> {
        chunk_turn(turn, CHUNK_TARGET_TOKENS, CHUNK_OVERLAP_TOKENS).map_err(HandlerError::from)
    }

    /// Extracts candidate facts from deterministic turn chunks.
    pub async fn extract(&self, chunks: &[TurnChunk]) -> Result<Vec<ExtractedFact>, HandlerError> {
        self.runtime
            .extractor()
            .extract(chunks)
            .await
            .map_err(HandlerError::from)
    }

    /// Classifies and redacts extracted facts before any durable write.
    pub async fn classify_pii(
        &self,
        facts: &[ExtractedFact],
    ) -> Result<Vec<ClassifiedFact>, HandlerError> {
        let classifier = self.runtime.pii_classifier();
        classify_facts_with(classifier.as_ref(), facts).await
    }

    /// Embeds classified facts, preserving explicit no-vector mode.
    pub async fn embed(&self, facts: &[ClassifiedFact]) -> Result<Vec<EmbeddedFact>, HandlerError> {
        let embedder = self.runtime.embedder();
        embed_batch_shared(embedder.as_deref(), facts).await
    }

    /// Detects contradictions for one turn against its admitted scope.
    pub async fn contradict(
        &self,
        turn: &SessionTurn,
        facts: &[EmbeddedFact],
    ) -> Result<Vec<IngestDecision>, HandlerError> {
        let detector = self.runtime.contradiction_detector();
        let vector_factory = self.runtime.vector_store_factory();
        detect_contradictions_with(
            detector.as_ref(),
            self.runtime.pool().clone(),
            &vector_factory,
            turn,
            facts,
        )
        .await
    }

    /// Applies decisions atomically and drains configured vector projections.
    pub async fn apply(
        &self,
        turn: &SessionTurn,
        decisions: &[IngestDecision],
    ) -> Result<IngestApplyReport, HandlerError> {
        let kms = self.runtime.kms();
        let vector_factory = self.runtime.vector_store_factory();
        let entity_resolver = self.runtime.entity_resolver();
        apply_decisions(
            self.runtime.pool(),
            &kms,
            &vector_factory,
            entity_resolver.as_ref(),
            self.runtime.entity_blocking_embedder(),
            turn,
            decisions,
        )
        .await
    }
}

/// Runs the slow-path ingestion steps directly against an explicit Postgres pool for local/tests.
///
/// This is intended for embedded hosts that own more than one pool in the same
/// process, such as integration tests. Restate hosts should use their durable
/// virtual-object adapter so the step journal remains durable. The helper
/// takes a transaction-scoped Postgres advisory fence before graph/vector writes
/// so duplicate direct callers for the same turn serialize across pods.
pub async fn ingest_turn_direct_with_pool(
    pool: PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    turn: SessionTurn,
) -> Result<IngestApplyReport, HandlerError> {
    let config = MoaConfig::load_from_env().map_err(HandlerError::from)?;
    let deps = direct_ingest_deps_from_config(pool, kms, &config)?;
    ingest_turn_direct_with_pool_and_pii(deps, turn).await
}

struct DirectIngestDeps {
    pool: PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    pii_classifier: Arc<dyn PiiClassifier>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    extractor: Arc<dyn FactExtractor>,
    entity_resolver: Arc<EntityResolver>,
    entity_blocking_embedder: Option<Arc<dyn EmbeddingProvider>>,
    contradiction_detector: Arc<dyn ContradictionDetector>,
    vector_factory: VectorStoreFactory,
}

fn direct_ingest_deps_from_config(
    pool: PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    config: &MoaConfig,
) -> Result<DirectIngestDeps, HandlerError> {
    let embedder =
        crate::ctx::build_configured_ingestion_embedder(config).map_err(HandlerError::from)?;
    Ok(DirectIngestDeps {
        pool,
        kms,
        pii_classifier: crate::ctx::build_shared_pii_classifier(
            config.memory.pii_service_url.as_deref(),
        ),
        entity_blocking_embedder: embedder.clone(),
        embedder,
        extractor: Arc::new(HeuristicFactExtractor),
        entity_resolver: Arc::new(EntityResolver::deterministic_for_app_role()),
        contradiction_detector: Arc::new(RrfPlusJudgeDetector::from_config_or_heuristic(config)),
        vector_factory: VectorStoreFactory::from_config(config),
    })
}

async fn ingest_turn_direct_with_pool_and_pii(
    deps: DirectIngestDeps,
    turn: SessionTurn,
) -> Result<IngestApplyReport, HandlerError> {
    let DirectIngestDeps {
        pool,
        kms,
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
        &kms,
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
        &ctx.kms,
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

/// Builds a finalized turn transcript from the durable user-turn text.
///
/// Ingestion must never read the compiled provider request: compiled messages
/// carry replayed history, injected memory reminders, digests, and planning
/// hints, all user-role, so extracting from them re-ingests retrieved memory
/// and old turns every turn (a self-reinforcing feedback loop). Callers pass
/// the user-turn text exactly as persisted in the session event log. Assistant
/// response text is excluded until facts can carry assistant provenance:
/// model-generated claims must not be stored with the same trust as user
/// statements.
#[must_use]
pub fn turn_transcript(user_turn_text: &str) -> String {
    let user = user_turn_text.trim();
    if user.is_empty() {
        return String::new();
    }
    format!("user: {user}")
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

    if let Some(result) = results.iter().find(|result| result.abstained) {
        return Err(Error::PiiClassificationUnavailable {
            model_version: result.model_version.clone(),
        }
        .into());
    }

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
        event_time: fact.event_time,
        category: fact.category,
        edge_label: fact.edge_label,
        functional: fact.functional,
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
    let embeddable = facts
        .iter()
        .enumerate()
        .filter(|(_, fact)| !fact.pii_class.is_sealed())
        .collect::<Vec<_>>();
    let texts = embeddable
        .iter()
        .map(|(_, fact)| fact.fact.summary.clone())
        .collect::<Vec<_>>();
    let embeddings = if texts.is_empty() {
        Vec::new()
    } else {
        embedder.embed(&texts).await.map_err(HandlerError::from)?
    };
    // F08: the embedding provider must return exactly one vector per input. Reject
    // the whole batch on a cardinality mismatch instead of zipping, which would
    // silently drop trailing facts (short response) or truncate vectors (long).
    if embeddings.len() != texts.len() {
        return Err(HandlerError::from(Error::EmbeddingCardinalityMismatch {
            expected: texts.len(),
            actual: embeddings.len(),
        }));
    }
    let mut embedded = facts
        .iter()
        .cloned()
        .map(|classified| EmbeddedFact {
            classified,
            embedding: None,
            embedding_model: None,
            embedding_model_version: None,
        })
        .collect::<Vec<_>>();
    for ((index, _), embedding) in embeddable.into_iter().zip(embeddings) {
        let fact = &mut embedded[index];
        fact.embedding = Some(embedding);
        fact.embedding_model = Some(embedder.model_id().to_string());
        fact.embedding_model_version = Some(embedder.model_version());
    }
    Ok(embedded)
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
        Conflict::Duplicate(fact_uid) => IngestDecision::SkipDuplicate { fact_uid, fact },
    }
}

fn decision_scope(turn: &SessionTurn, decision: &IngestDecision) -> RlsContext {
    let scope = match decision {
        IngestDecision::Insert { fact }
        | IngestDecision::Supersede { fact, .. }
        | IngestDecision::SkipDuplicate { fact, .. } => fact_scope(turn, fact),
    };
    match turn.barrier.clone() {
        Some(barrier) => scope.with_cleared_barriers([barrier].into_iter().collect()),
        None => scope,
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
    kms: &Arc<dyn KeyManagementProvider>,
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
        kms,
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
            Ok(ApplyOutcome::Reinforced) => report.reinforced += 1,
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
    kms: &'a Arc<dyn KeyManagementProvider>,
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
    // Re-observation confirms the survivor instead of dropping the fact, and
    // skips the entity/vector setup below that only insert paths need.
    if let IngestDecision::SkipDuplicate { fact_uid, fact } = decision {
        return reinforce_duplicate(deps.pool, scope, turn, fact, *fact_uid).await;
    }
    let Some(fact) = decision_fact(decision) else {
        return Ok(ApplyOutcome::Skipped);
    };
    let use_entity_embeddings =
        deps.entity_blocking_embedder.is_some() && !fact.classified.pii_class.is_sealed();
    let entity_vector = if use_entity_embeddings {
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
        deps.kms.clone(),
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
    kms: Arc<dyn KeyManagementProvider>,
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
    let store = PostgresGraphStore::scoped_for_app_role(pool.clone(), scope.clone(), kms);
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
        barrier: turn.barrier.clone(),
        uid: fact_uid,
        data_subject_id: scope
            .contact_id()
            .map_or(scope.tenant_id().0, |contact_id| contact_id.0),
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
            // Structured extraction semantics persisted for downstream lifecycle
            // passes: digest ordering reads `category`, the contradiction sweep
            // reads `functional`. `edge_label` is stored for provenance; the edge
            // itself is written from the same field at ingestion time.
            "category": extracted.category,
            "edge_label": extracted.edge_label,
            "functional": extracted.functional,
        }),
        pii_class: fact.classified.pii_class,
        confidence: Some(extracted_confidence(extracted)),
        // A stated event time backdates validity so recency ranking and as-of
        // reads reflect when the fact became true; future-dated values fall
        // back to the turn instant.
        valid_from: extracted
            .event_time
            .filter(|event_time| *event_time <= turn.finalized_at)
            .unwrap_or(turn.finalized_at),
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
                barrier: turn.barrier.as_ref(),
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
                barrier: turn.barrier.as_ref(),
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
        .get(&normalize_entity_name(name))
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
        if fact.classified.pii_class.is_sealed() {
            continue;
        }
        let extracted = &fact.classified.fact;
        for raw in [extracted.subject.as_str(), extracted.object.as_str()] {
            let normalized = normalize_entity_name(raw);
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
            fact.classified.fact.edge_label,
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
    edge_label: FactEdgeLabel,
    alias_mention: Option<&str>,
) -> EdgeWriteIntent {
    EdgeWriteIntent {
        uid: uuid::Uuid::now_v7(),
        label: fact_object_edge_label(edge_label),
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

/// Maps the extraction-assigned edge label onto the stored graph edge label.
///
/// Extraction decides the fact-to-object relationship once as a structured
/// field; this is a total mapping over that controlled vocabulary, never a
/// re-parse of predicate prose. Unrecognized inputs cannot occur because
/// [`FactEdgeLabel`] already collapses unknown values to `RelatesTo`.
fn fact_object_edge_label(edge_label: FactEdgeLabel) -> EdgeLabel {
    match edge_label {
        FactEdgeLabel::DependsOn => EdgeLabel::DependsOn,
        FactEdgeLabel::OwnedBy => EdgeLabel::OwnedBy,
        FactEdgeLabel::RelatesTo => EdgeLabel::RelatesTo,
    }
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

fn turn_seq_i64(turn: &SessionTurn) -> Result<i64, HandlerError> {
    i64::try_from(turn.turn_seq).map_err(|_| {
        TerminalError::new(format!("turn_seq {} does not fit into i64", turn.turn_seq)).into()
    })
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

enum ApplyOutcome {
    Inserted,
    Superseded,
    Reinforced,
    Skipped,
}

/// Confirms a re-observed fact by reinforcing the surviving node.
///
/// The same-turn `ingest_dedup` row committed with the boost makes
/// reinforcement idempotent under Restate replays: a retried turn takes the
/// `dedup_fact_uid` early return instead of boosting twice.
async fn reinforce_duplicate(
    pool: &PgPool,
    scope: &RlsContext,
    turn: &SessionTurn,
    fact: &EmbeddedFact,
    existing_uid: uuid::Uuid,
) -> Result<ApplyOutcome, HandlerError> {
    let hash = fact_hash(&fact.classified.fact).map_err(HandlerError::from)?;
    if dedup_fact_uid(pool, scope, turn, &hash).await?.is_some() {
        return Ok(ApplyOutcome::Skipped);
    }
    let mut conn = ScopedConn::begin_as_app(pool, scope, true)
        .await
        .map_err(HandlerError::from)?;
    let reinforced = moa_memory_graph::write::reinforce_node_in_conn(
        conn.as_mut(),
        moa_memory_graph::NodeReinforcementIntent {
            uid: existing_uid,
            step: crate::extract::REINFORCE_CONFIDENCE_STEP,
            cap: crate::extract::REINFORCE_CONFIDENCE_CAP,
        },
    )
    .await
    .map_err(HandlerError::from)?;
    insert_dedup_in_conn(conn.as_mut(), scope, turn, &hash, existing_uid).await?;
    conn.commit().await.map_err(HandlerError::from)?;
    // A closed survivor means the duplicate verdict raced a supersession;
    // report a skip rather than reviving stale confidence.
    Ok(if reinforced {
        ApplyOutcome::Reinforced
    } else {
        ApplyOutcome::Skipped
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::Utc;
    use moa_config::MoaConfig;
    use moa_core::traits::EmbeddingProvider;
    use moa_core::types::security::SensitivityClass;
    use moa_core::{
        types::contact::ContactId, types::identifiers::SessionId, types::identifiers::TenantId,
    };
    use moa_crypto::LocalKmsProvider;
    use moa_memory_graph::EdgeLabel;
    use moa_memory_pii::{PiiCategory, PiiClassifier, PiiResult, PiiSpan, classify_heuristic};
    use moa_memory_types::{FactCategory, FactEdgeLabel};
    use sqlx::postgres::PgPoolOptions;

    use super::{
        classify_facts_with, direct_ingest_claim_key, direct_ingest_deps_from_config,
        embed_batch_shared, entity_fact_edge_intent, fact_entity_edge_intent,
        fact_object_edge_label, precompute_entity_embeddings, redact_fact, turn_tenant_scope,
        turn_transcript,
    };
    use crate::{
        ClassifiedFact, EmbeddedFact, Error, ExtractedFact, ExtractedFactScopeHint, IngestDecision,
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
                class: SensitivityClass::None,
                spans: Vec::new(),
                model_version: text.to_string(),
                abstained: false,
            })
        }
    }

    /// PII classifier that returns one fixed response for boundary tests.
    struct FixedResultClassifier(PiiResult);

    #[async_trait::async_trait]
    impl PiiClassifier for FixedResultClassifier {
        async fn classify(&self, _text: &str) -> moa_memory_pii::Result<PiiResult> {
            Ok(self.0.clone())
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

        async fn embed(&self, inputs: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
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

    /// Embedder that returns one fewer vector than it was given, violating the
    /// provider cardinality contract.
    struct MiscountingEmbedder {
        dim: usize,
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for MiscountingEmbedder {
        fn model_id(&self) -> &str {
            "miscounting"
        }

        fn dimensions(&self) -> usize {
            self.dim
        }

        async fn embed(&self, inputs: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
            let short = inputs.len().saturating_sub(1);
            Ok((0..short).map(|_| vec![0.0; self.dim]).collect())
        }
    }

    #[tokio::test]
    async fn embed_batch_rejects_provider_cardinality_mismatch() {
        // Pins: F08 — an embedding provider that returns a different number of
        // vectors than facts fails the whole batch with a typed error instead of
        // silently zipping (which would drop the trailing fact).
        let facts = vec![
            ClassifiedFact {
                fact: plain_fact("first fact"),
                pii_class: SensitivityClass::None,
                pii_spans: Vec::new(),
            },
            ClassifiedFact {
                fact: plain_fact("second fact"),
                pii_class: SensitivityClass::None,
                pii_spans: Vec::new(),
            },
        ];
        let embedder = MiscountingEmbedder { dim: 8 };

        let result = super::embed_batch_with(&embedder, &facts).await;

        let Err(error) = result else {
            panic!("cardinality mismatch must fail the batch");
        };
        let rendered = format!("{error:?}");
        assert!(
            rendered.contains("EmbeddingCardinalityMismatch")
                && rendered.contains("expected: 2")
                && rendered.contains("actual: 1"),
            "expected a typed cardinality error carrying the counts, got: {rendered}"
        );
    }

    #[tokio::test]
    async fn embed_batch_omits_sealed_classes_before_provider_offline() {
        // Pins: PHI and restricted facts never enter the embedding-provider
        // batch and retain no vector identity, while ordinary and PII facts in
        // the same batch remain semantically indexable.
        let calls = Arc::new(AtomicUsize::new(0));
        let batch_len = Arc::new(AtomicUsize::new(0));
        let embedder = CountingEmbedder {
            calls: calls.clone(),
            last_batch_len: batch_len.clone(),
            dim: 8,
        };
        let facts = [
            ("ordinary fact", SensitivityClass::None),
            ("contact fact", SensitivityClass::Pii),
            ("health fact", SensitivityClass::Phi),
            ("credential fact", SensitivityClass::Restricted),
        ]
        .into_iter()
        .map(|(summary, pii_class)| ClassifiedFact {
            fact: plain_fact(summary),
            pii_class,
            pii_spans: Vec::new(),
        })
        .collect::<Vec<_>>();

        let embedded = super::embed_batch_with(&embedder, &facts)
            .await
            .expect("mixed-sensitivity embedding batch should succeed");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(batch_len.load(Ordering::SeqCst), 2);
        for index in [0, 1] {
            assert!(embedded[index].embedding.is_some());
            assert_eq!(embedded[index].embedding_model.as_deref(), Some("counting"));
            assert_eq!(embedded[index].embedding_model_version, Some(1));
        }
        for index in [2, 3] {
            assert_eq!(embedded[index].embedding, None);
            assert_eq!(embedded[index].embedding_model, None);
            assert_eq!(embedded[index].embedding_model_version, None);
        }
    }

    #[tokio::test]
    async fn embed_batch_with_only_sealed_classes_skips_provider_offline() {
        // Pins: an all-sealed turn performs no empty or content-bearing provider
        // request and returns facts with no semantic-index identity.
        let calls = Arc::new(AtomicUsize::new(0));
        let batch_len = Arc::new(AtomicUsize::new(0));
        let embedder = CountingEmbedder {
            calls: calls.clone(),
            last_batch_len: batch_len.clone(),
            dim: 8,
        };
        let facts = [SensitivityClass::Phi, SensitivityClass::Restricted]
            .into_iter()
            .map(|pii_class| ClassifiedFact {
                fact: plain_fact("sealed fact"),
                pii_class,
                pii_spans: Vec::new(),
            })
            .collect::<Vec<_>>();

        let embedded = super::embed_batch_with(&embedder, &facts)
            .await
            .expect("all-sealed batch should stay in no-vector mode");

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(batch_len.load(Ordering::SeqCst), 0);
        assert_eq!(embedded.len(), 2);
        assert!(embedded.iter().all(|fact| fact.embedding.is_none()));
        assert!(
            embedded.iter().all(
                |fact| fact.embedding_model.is_none() && fact.embedding_model_version.is_none()
            )
        );
    }

    fn embedded_decision(
        subject: &str,
        object: &str,
        summary: &str,
        embedding: Option<Vec<f32>>,
    ) -> IngestDecision {
        embedded_decision_with_class(subject, object, summary, embedding, SensitivityClass::None)
    }

    fn embedded_decision_with_class(
        subject: &str,
        object: &str,
        summary: &str,
        embedding: Option<Vec<f32>>,
        pii_class: SensitivityClass,
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
            event_time: None,
            category: FactCategory::Other,
            edge_label: FactEdgeLabel::RelatesTo,
            functional: false,
        };
        let hash = fact_hash(&fact).expect("fact hashes");
        fact.uid = crate::fact_uid_from_hash(&hash);
        let embedding_model = embedding.as_ref().map(|_| "counting".to_string());
        IngestDecision::Insert {
            fact: EmbeddedFact {
                classified: ClassifiedFact {
                    fact,
                    pii_class,
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
        // configured, so no vectors are precomputed.
        let decisions = vec![embedded_decision("api", "db", "api uses db", None)];

        let map = precompute_entity_embeddings(None, &decisions)
            .await
            .expect("precompute without embedder");

        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn precompute_entity_embeddings_omits_sealed_fact_names_offline() {
        // Pins: entity blocking cannot make a second provider call for names
        // inherited from a sealed fact after fact embedding has been withheld.
        let calls = Arc::new(AtomicUsize::new(0));
        let batch_len = Arc::new(AtomicUsize::new(0));
        let embedder = CountingEmbedder {
            calls: calls.clone(),
            last_batch_len: batch_len.clone(),
            dim: 8,
        };
        let decisions = vec![
            embedded_decision("api", "db", "api uses db", None),
            embedded_decision_with_class(
                "patient",
                "123-45-6789",
                "patient SSN is 123-45-6789",
                None,
                SensitivityClass::Phi,
            ),
        ];

        let map = precompute_entity_embeddings(Some(&embedder), &decisions)
            .await
            .expect("precompute should ignore sealed entity names");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(batch_len.load(Ordering::SeqCst), 2);
        assert_eq!(map.len(), 2);
        assert!(map.contains_key("api"));
        assert!(map.contains_key("db"));
        assert!(!map.contains_key("patient"));
        assert!(!map.contains_key("123 45 6789"));
    }

    #[tokio::test]
    async fn slow_path_without_configured_embedder_preserves_no_vector_facts() {
        // Pins: disabled or uncredentialed runtime construction leaves the slow
        // path writable while omitting vector bytes and their model identity.
        let facts = vec![ClassifiedFact {
            fact: plain_fact("the api uses postgres"),
            pii_class: SensitivityClass::None,
            pii_spans: Vec::new(),
        }];

        let embedded = embed_batch_shared(None, &facts)
            .await
            .expect("slow no-vector mode should preserve classified facts");

        assert_eq!(embedded.len(), 1);
        assert_eq!(embedded[0].classified.fact.summary, "the api uses postgres");
        assert_eq!(embedded[0].embedding, None);
        assert_eq!(embedded[0].embedding_model, None);
        assert_eq!(embedded[0].embedding_model_version, None);
    }

    #[tokio::test]
    async fn direct_helper_uses_configured_selector_and_reuses_one_embedder() {
        // Pins: the explicit-pool helper builds the configured ingestion provider
        // once per invocation and shares that exact Arc with entity blocking.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let mut config = MoaConfig::default();
        config.memory.vector.embedder.name = "gemini:gemini-embedding-2".to_string();
        config.providers.google.api_key = "test-google-key".to_string();

        let deps = direct_ingest_deps_from_config(pool, Arc::new(LocalKmsProvider::new()), &config)
            .expect("direct helper dependencies should build without provider calls");
        let fact_embedder = deps
            .embedder
            .as_ref()
            .expect("configured direct helper embedder should be available");
        let entity_embedder = deps
            .entity_blocking_embedder
            .as_ref()
            .expect("entity blocking should reuse the direct helper embedder");

        assert_eq!(fact_embedder.model_id(), "gemini-embedding-2");
        assert!(Arc::ptr_eq(fact_embedder, entity_embedder));
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
            event_time: None,
            category: FactCategory::Other,
            edge_label: FactEdgeLabel::RelatesTo,
            functional: false,
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
            assert_eq!(entry.pii_class, SensitivityClass::None);
        }
    }

    #[tokio::test]
    async fn classify_facts_with_rejects_abstained_results() {
        // Pins: a fail-closed classifier abstention is a retryable ingestion error
        // before plaintext can reach redaction, embedding, or durable graph writes.
        let error = classify_facts_with(
            &FixedResultClassifier(PiiResult::fail_closed("privacy-filter:v9")),
            &[plain_fact("alice@example.com owns secret sk-live")],
        )
        .await
        .expect_err("abstaining PII classification must abort ingestion");

        let source =
            std::error::Error::source(error.as_ref() as &(dyn std::error::Error + 'static))
                .expect("retryable handler error should preserve the ingestion error source");
        assert!(matches!(
            source.downcast_ref::<Error>(),
            Some(Error::PiiClassificationUnavailable { model_version })
                if model_version == "privacy-filter:v9"
        ));
    }

    #[tokio::test]
    async fn classify_facts_with_preserves_successful_pii_redaction() {
        // Pins: the abstention boundary does not reject a successful PII result;
        // its class and spans still drive redaction and the redacted fact identity.
        let fact = pii_fact("alice@example.com owns secret sk-live");
        let result = pii_result(&fact.summary, "alice@example.com", PiiCategory::Email);

        let classified = classify_facts_with(
            &FixedResultClassifier(result.clone()),
            std::slice::from_ref(&fact),
        )
        .await
        .expect("non-abstaining PII classification should proceed");

        assert_eq!(classified.len(), 1);
        assert_eq!(classified[0].pii_class, SensitivityClass::Pii);
        assert_eq!(classified[0].pii_spans, result.spans);
        assert_eq!(
            classified[0].fact.summary,
            "[EMAIL_REDACTED] owns secret sk-live"
        );
        assert_ne!(classified[0].fact.uid, fact.uid);
        let redacted_hash = fact_hash(&classified[0].fact).expect("redacted fact hashes");
        assert_eq!(
            classified[0].fact.uid,
            crate::fact_uid_from_hash(&redacted_hash)
        );
    }

    #[test]
    fn turn_transcript_carries_only_the_durable_user_turn() {
        // Pins: ingestion transcripts are built from the durable user event, so
        // injected reminders and assistant claims can never re-enter extraction.
        let transcript = turn_transcript("  Remember that auth uses JWT.  ");

        assert_eq!(transcript, "user: Remember that auth uses JWT.");
    }

    #[test]
    fn turn_transcript_is_empty_for_blank_user_turns() {
        // Pins: attachment-only or empty turns produce no ingestable transcript.
        assert_eq!(turn_transcript("   \n\t"), "");
    }

    #[test]
    fn pii_heuristic_classifier_marks_secrets_restricted() {
        // Pins: ingest fallback PII classification uses the shared heuristic classifier.
        let result = classify_heuristic("The API secret is sk-test-123.");

        assert_eq!(result.class, SensitivityClass::Restricted);
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
    fn structured_edge_label_maps_totally_onto_storage_label() {
        // Pins: the fact-to-object edge label comes from extraction's structured
        // FactEdgeLabel, mapped one-to-one onto the storage EdgeLabel. Edge typing
        // is no longer re-derived from predicate prose.
        assert_eq!(
            fact_object_edge_label(FactEdgeLabel::DependsOn),
            EdgeLabel::DependsOn
        );
        assert_eq!(
            fact_object_edge_label(FactEdgeLabel::OwnedBy),
            EdgeLabel::OwnedBy
        );
        assert_eq!(
            fact_object_edge_label(FactEdgeLabel::RelatesTo),
            EdgeLabel::RelatesTo
        );
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
            FactEdgeLabel::DependsOn,
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
            event_time: None,
            category: FactCategory::Other,
            edge_label: FactEdgeLabel::RelatesTo,
            functional: false,
        };
        let hash = fact_hash(&fact).expect("fact hashes");
        fact.uid = crate::fact_uid_from_hash(&hash);
        fact
    }

    fn pii_result(summary: &str, needle: &str, category: PiiCategory) -> PiiResult {
        let start = summary.find(needle).expect("needle present");
        PiiResult {
            class: SensitivityClass::Pii,
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
            barrier: None,
        }
    }
}
