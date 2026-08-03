//! Low-latency graph-memory ingestion for explicit remember, forget, and supersede commands.

use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use moa_core::types::memory::{InformationBarrierId, RlsContext};
use moa_core::types::security::SensitivityClass;
use moa_core::{
    error::MoaError, traits::EmbeddingProvider, traits::MemoryToolExecutor,
    types::contact::ContactId, types::contact::SessionActorRef,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::session::SessionMeta, types::tools::ToolOutput,
};
use moa_db::ScopedConn;
use moa_memory_graph::{
    Error as GraphError, GraphStore, NodeLabel, NodeWriteIntent, PostgresGraphStore,
};
use moa_memory_pii::{Error as PiiError, PiiClassifier, PiiResult, redact_text};
use moa_memory_vector::VectorStoreFactory;
use moa_memory_vector::{Error as VectorError, VECTOR_DIMENSION, VectorStore};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::{sync::OnceCell, time::timeout};
use tracing::warn;
use uuid::Uuid;

use crate::{Conflict, ContradictionContext, ContradictionDetector, Error, IngestRuntime};

const JUDGE_TIMEOUT: Duration = Duration::from_millis(250);
const SUPERSEDE_TIMEOUT: Duration = Duration::from_millis(500);

/// Request for an explicit fast-path memory write.
#[derive(Debug, Clone)]
pub struct FastRememberRequest {
    /// Tenant that owns the memory row.
    pub tenant_id: Uuid,
    /// Optional contact owner inside the tenant.
    pub contact_id: Option<Uuid>,
    /// Scope tier string: `tenant` or `contact`.
    pub scope: String,
    /// Free-form fact or decision text.
    pub text: String,
    /// Graph node label for the remembered text.
    pub label: NodeLabel,
    /// Explicit supersession target, bypassing contradiction detection.
    pub supersedes_specific: Option<Uuid>,
    /// Information-barrier the write is running under, inherited from the calling
    /// session. `Some(tag)` restricts the node to callers cleared for the barrier;
    /// `None` (the common case) leaves it unrestricted.
    pub barrier: Option<InformationBarrierId>,
    /// Principal that triggered the write.
    pub actor_id: Uuid,
    /// Principal kind written to the changelog.
    pub actor_kind: String,
}

/// Fast-path forget target.
#[derive(Debug, Clone)]
pub enum ForgetPattern {
    /// Forget one explicit node id.
    Uid(Uuid),
    /// Forget all active nodes whose projected name exactly matches this value.
    NameMatch(String),
    /// Forget all active contact-scoped nodes for this contact in the current tenant.
    SoftAll(Uuid),
}

/// Dependencies needed by fast-path memory commands.
#[derive(Clone)]
pub struct FastPathCtx {
    pool: PgPool,
    scope: RlsContext,
    graph: Arc<dyn GraphStore>,
    vector_factory: VectorStoreFactory,
    vector: Arc<OnceCell<Arc<dyn VectorStore>>>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    pii: Arc<dyn PiiClassifier>,
    contradict: Arc<dyn ContradictionDetector>,
    assume_app_role: bool,
}

impl FastPathCtx {
    /// Creates a fast-path context from explicit dependencies.
    #[must_use]
    pub fn new(
        pool: PgPool,
        scope: RlsContext,
        graph: Arc<dyn GraphStore>,
        vector: Arc<dyn VectorStore>,
        embedder: Arc<dyn EmbeddingProvider>,
        pii: Arc<dyn PiiClassifier>,
        contradict: Arc<dyn ContradictionDetector>,
    ) -> Self {
        let vector_cell = OnceCell::const_new();
        let _ = vector_cell.set(vector);
        Self {
            pool,
            scope,
            graph,
            vector_factory: VectorStoreFactory::default(),
            vector: Arc::new(vector_cell),
            embedder: Some(embedder),
            pii,
            contradict,
            assume_app_role: false,
        }
    }

    /// Creates a fast-path context that selects the read-side vector store lazily.
    #[must_use]
    pub fn new_with_vector_factory(
        pool: PgPool,
        scope: RlsContext,
        graph: Arc<dyn GraphStore>,
        vector_factory: VectorStoreFactory,
        embedder: Arc<dyn EmbeddingProvider>,
        pii: Arc<dyn PiiClassifier>,
        contradict: Arc<dyn ContradictionDetector>,
    ) -> Self {
        Self::new_with_optional_embedder(
            pool,
            scope,
            graph,
            vector_factory,
            Some(embedder),
            pii,
            contradict,
        )
    }

    fn new_with_optional_embedder(
        pool: PgPool,
        scope: RlsContext,
        graph: Arc<dyn GraphStore>,
        vector_factory: VectorStoreFactory,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
        pii: Arc<dyn PiiClassifier>,
        contradict: Arc<dyn ContradictionDetector>,
    ) -> Self {
        Self {
            pool,
            scope,
            graph,
            vector_factory,
            vector: Arc::new(OnceCell::const_new()),
            embedder,
            pii,
            contradict,
            assume_app_role: false,
        }
    }

    /// Configures test-mode role assumption for owner-role integration tests.
    #[must_use]
    pub fn with_assume_app_role(mut self, assume_app_role: bool) -> Self {
        self.assume_app_role = assume_app_role;
        self
    }

    /// Returns the scope used for direct SQL lookups.
    #[must_use]
    pub fn scope(&self) -> &RlsContext {
        &self.scope
    }

    async fn contradiction_context(&self) -> Result<ContradictionContext, FastError> {
        let vector = self.read_vector().await?;
        if self.assume_app_role {
            Ok(ContradictionContext::for_app_role(
                self.pool.clone(),
                self.scope.clone(),
                vector,
            ))
        } else {
            Ok(ContradictionContext::new(
                self.pool.clone(),
                self.scope.clone(),
                vector,
            ))
        }
    }

    async fn read_vector(&self) -> Result<Arc<dyn VectorStore>, FastError> {
        let pool = self.pool.clone();
        let scope = self.scope.clone();
        let assume_app_role = self.assume_app_role;
        let vector_factory = self.vector_factory.clone();
        self.vector
            .get_or_try_init(|| async move {
                vector_factory
                    .configured_for_scope(&pool, scope, assume_app_role)
                    .await
                    .map_err(FastError::from)
            })
            .await
            .map(Clone::clone)
    }
}

/// Errors returned by the fast memory path.
#[derive(Debug, thiserror::Error)]
pub enum FastError {
    /// The request was invalid.
    #[error("invalid fast memory request: {0}")]
    Invalid(String),
    /// A latency budget expired.
    #[error("fast memory operation timed out: {0}")]
    Timeout(&'static str),
    /// A vector-producing write was requested without the configured ingestion embedder.
    #[error("configured ingestion embedder is unavailable for fast memory vector writes")]
    ConfiguredEmbedderUnavailable,
    /// Graph write protocol failed.
    #[error("graph: {0}")]
    Graph(#[from] GraphError),
    /// Vector embedding or vector-store operation failed.
    #[error("vector: {0}")]
    Vector(#[from] VectorError),
    /// PII classifier setup failed.
    #[error("pii: {0}")]
    Pii(#[from] PiiError),
    /// PII classification failed closed before safe text could be produced.
    #[error("pii classification unavailable for fast memory write: {model_version}")]
    PiiClassificationUnavailable {
        /// Classifier model or fallback marker that produced the fail-closed result.
        model_version: String,
    },
    /// Postgres query failed.
    #[error("postgres: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// Core storage helper failed.
    #[error("core: {0}")]
    Core(#[from] MoaError),
    /// JSON input could not be parsed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Slow/fast ingestion helper failed.
    #[error("ingest: {0}")]
    Ingest(#[from] Error),
}

/// Remembers one fact through the graph write protocol.
pub async fn fast_remember(req: FastRememberRequest, ctx: &FastPathCtx) -> Result<Uuid, FastError> {
    let started = Instant::now();
    let result = fast_remember_inner(req, ctx).await;
    record_remember_metrics(started.elapsed(), &result);
    result
}

/// Soft-invalidates graph memory rows matched by a forget pattern.
pub async fn fast_forget(pattern: ForgetPattern, ctx: &FastPathCtx) -> Result<u64, FastError> {
    let started = Instant::now();
    let uids = active_uids_for_pattern(&pattern, ctx).await?;
    let mut invalidated = 0_u64;
    for uid in uids {
        ctx.graph.invalidate_node(uid, "user_forget").await?;
        invalidated += 1;
    }
    metrics::histogram!("moa_fast_forget_latency_seconds").record(started.elapsed().as_secs_f64());
    metrics::counter!("moa_fast_forget_total").increment(1);
    Ok(invalidated)
}

/// Supersedes an existing graph node inside the fast-path latency budget.
pub async fn fast_supersede(
    old_uid: Uuid,
    new: NodeWriteIntent,
    ctx: &FastPathCtx,
) -> Result<Uuid, FastError> {
    match timeout(SUPERSEDE_TIMEOUT, ctx.graph.supersede_node(old_uid, new)).await {
        Ok(result) => result.map_err(FastError::from),
        Err(_) => Err(FastError::Timeout("supersede")),
    }
}

/// A durable failure to preserve as retrievable negative-results memory.
///
/// Mirrors [`FastRememberRequest`] scoping: `tenant`/`contact` must agree with
/// `contact_id`. `attempted` and `failure` are short, controlled strings (a tool
/// name and an error class); both are PII-classified and redacted before the node
/// is written, identical to the explicit fast-path write path.
#[derive(Debug, Clone)]
pub struct IncidentRecord {
    /// Tenant that owns the incident row.
    pub tenant_id: Uuid,
    /// Optional contact owner inside the tenant.
    pub contact_id: Option<Uuid>,
    /// Scope tier string: `tenant` or `contact`.
    pub scope: String,
    /// Session the failure occurred in; also the dedup partition.
    pub session_id: Uuid,
    /// Turn sequence number the failure concluded on.
    pub turn_seq: i64,
    /// What was attempted (for example the failing tool name).
    pub attempted: String,
    /// Why it failed (a stable error class, not a raw message).
    pub failure: String,
    /// Information-barrier the failing session is running under, inherited so the
    /// incident node is need-to-know restricted like the rest of the session's
    /// memory. `None` leaves it unrestricted.
    pub barrier: Option<InformationBarrierId>,
    /// Principal recorded in the changelog for the write.
    pub actor_id: Uuid,
    /// Principal kind recorded in the changelog for the write.
    pub actor_kind: String,
}

/// Records one durable failure as an `Incident` node using an explicit runtime.
///
/// Fire-and-forget from the brain turn loop: returns `Ok(None)` (rather than an
/// error) when memory learning is disabled in config, when the same failure was
/// already recorded for this session, or when PII classification cannot vouch for
/// the text. The write is scoped to the session's contact when present and to the
/// tenant otherwise, mirroring the explicit fast-path write.
pub async fn record_incident(
    runtime: &IngestRuntime,
    session: &SessionMeta,
    turn_seq: i64,
    attempted: &str,
    failure: &str,
) -> Result<Option<Uuid>, FastError> {
    if !runtime.fact_extraction_enabled() {
        return Ok(None);
    }
    let barrier = pinned_write_barrier(session)?;
    let tenant_id = tenant_uuid(session);
    let contact_id = session.contact.as_ref().map(|contact| contact.contact_id.0);
    let (scope_ctx, scope) = match contact_id {
        Some(contact_id) => (
            RlsContext::contact(TenantId::from(tenant_id), ContactId(contact_id)),
            "contact",
        ),
        None => (RlsContext::tenant(TenantId::from(tenant_id)), "tenant"),
    };
    let scope_ctx = with_write_barrier_clearance(scope_ctx, barrier.as_ref());
    let ctx = runtime_fast_ctx(runtime, scope_ctx).await?;
    record_incident_with_ctx(
        IncidentRecord {
            tenant_id,
            contact_id,
            scope: scope.to_string(),
            session_id: session.id.0,
            turn_seq,
            attempted: attempted.to_string(),
            failure: failure.to_string(),
            barrier,
            actor_id: actor_id_from_session(session),
            actor_kind: "system".to_string(),
        },
        &ctx,
    )
    .await
}

/// Writes one `Incident` node through the graph write protocol.
///
/// Returns `Ok(None)` when the failure duplicates an active incident already
/// recorded for the same session (dedup by node name), or when PII classification
/// fails closed for either field. `attempted` and `failure` are classified and
/// redacted independently so a stored field never carries un-vouched text.
pub async fn record_incident_with_ctx(
    req: IncidentRecord,
    ctx: &FastPathCtx,
) -> Result<Option<Uuid>, FastError> {
    validate_incident_request(&req)?;
    let embedder = require_configured_embedder(ctx.embedder.as_ref())?;

    let Some((attempted_safe, attempted_class)) = classify_and_redact(&req.attempted, ctx).await?
    else {
        return Ok(None);
    };
    let Some((failure_safe, failure_class)) = classify_and_redact(&req.failure, ctx).await? else {
        return Ok(None);
    };
    let pii_class = more_restrictive(attempted_class, failure_class);
    let summary = format!("{attempted_safe}: {failure_safe}");
    let title = short_name(&summary);

    if incident_already_recorded(&title, req.session_id, ctx).await? {
        return Ok(None);
    }

    let embedding = embedder
        .embed(std::slice::from_ref(&summary))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| FastError::Invalid("embedder returned no result".to_string()))?;
    if embedding.len() != VECTOR_DIMENSION {
        return Err(FastError::Invalid(format!(
            "expected {VECTOR_DIMENSION}-dimension embedding, got {}",
            embedding.len()
        )));
    }

    let intent = NodeWriteIntent {
        barrier: req.barrier.clone(),
        uid: Uuid::now_v7(),
        data_subject_id: req.contact_id.unwrap_or(req.tenant_id),
        label: NodeLabel::Incident,
        storage_partition_id: Some(
            StoragePartitionId::for_tenant(TenantId::from(req.tenant_id)).to_string(),
        ),
        contact_id: req.contact_id.map(|contact_id| contact_id.to_string()),
        scope: req.scope.clone(),
        name: title,
        properties: json!({
            "summary": summary,
            "attempted": attempted_safe,
            "failure": failure_safe,
            "session_id": req.session_id,
            "turn_seq": req.turn_seq,
            "source": "incident",
        }),
        pii_class,
        confidence: Some(0.9),
        valid_from: Utc::now(),
        embedding: Some(embedding),
        embedding_model: Some(embedder.model_id().to_string()),
        embedding_model_version: Some(embedder.model_version()),
        embedding_text: None,
        actor_id: req.actor_id.to_string(),
        actor_kind: req.actor_kind.clone(),
    };
    let uid = ctx.graph.create_node(intent).await?;
    metrics::counter!("moa_incident_recorded_total").increment(1);
    Ok(Some(uid))
}

fn validate_incident_request(req: &IncidentRecord) -> Result<(), FastError> {
    if req.attempted.trim().is_empty() || req.failure.trim().is_empty() {
        return Err(FastError::Invalid(
            "incident requires attempted and failure text".to_string(),
        ));
    }
    match req.scope.as_str() {
        "tenant" if req.contact_id.is_none() => Ok(()),
        "contact" if req.contact_id.is_some() => Ok(()),
        _ => Err(FastError::Invalid(
            "incident scope must be `tenant` without a contact or `contact` with one".to_string(),
        )),
    }
}

/// Classifies and redacts one incident field, skipping the write (returning
/// `None`) when the classifier fails closed and cannot vouch for the text.
async fn classify_and_redact(
    text: &str,
    ctx: &FastPathCtx,
) -> Result<Option<(String, SensitivityClass)>, FastError> {
    let pii = match ctx.pii.classify(text).await {
        Ok(result) => result,
        Err(error) => {
            warn!(%error, "PII classifier failed for incident; failing closed");
            PiiResult::fail_closed("incident-fallback")
        }
    };
    if pii.abstained && pii.spans.is_empty() {
        return Ok(None);
    }
    Ok(Some((redact_text(text, &pii.spans), pii.class)))
}

async fn incident_already_recorded(
    title: &str,
    session_id: Uuid,
    ctx: &FastPathCtx,
) -> Result<bool, FastError> {
    let mut conn = begin_scoped(ctx).await?;
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS ( \
             SELECT 1 FROM moa.node_index \
             WHERE label = 'Incident' \
               AND name = $1 \
               AND valid_to IS NULL \
               AND properties_summary->>'session_id' = $2 \
         )",
    )
    .bind(title)
    .bind(session_id.to_string())
    .fetch_one(conn.as_mut())
    .await?;
    conn.commit().await?;
    Ok(exists)
}

fn more_restrictive(a: SensitivityClass, b: SensitivityClass) -> SensitivityClass {
    if a.rank() >= b.rank() { a } else { b }
}

/// Executes a memory tool request using an explicitly injected runtime.
pub async fn execute_memory_tool(
    runtime: &IngestRuntime,
    session: &SessionMeta,
    tool_name: &str,
    input: &Value,
) -> moa_core::error::Result<ToolOutput> {
    let started = Instant::now();
    let output = match tool_name {
        "memory_remember" => Ok(execute_remember_tool(runtime, session, input, started)
            .await
            .unwrap_or_else(|error| memory_tool_failure_output(tool_name, &error, started))),
        "memory_forget" => Ok(execute_forget_tool(runtime, session, input, started)
            .await
            .unwrap_or_else(|error| memory_tool_failure_output(tool_name, &error, started))),
        "memory_supersede" => Ok(execute_supersede_tool(runtime, session, input, started)
            .await
            .unwrap_or_else(|error| memory_tool_failure_output(tool_name, &error, started))),
        _ => Err(FastError::Invalid(format!(
            "unknown fast memory tool {tool_name}"
        ))),
    };
    match output {
        Ok(output) => Ok(output),
        Err(error) => Err(MoaError::ToolError(error.to_string())),
    }
}

/// Runtime adapter that lets `moa-hands` execute graph-memory tools without depending on ingest.
#[derive(Clone)]
pub struct FastMemoryToolExecutor {
    runtime: Arc<IngestRuntime>,
}

impl FastMemoryToolExecutor {
    /// Creates a fast-memory adapter over the host-owned ingestion runtime.
    #[must_use]
    pub fn new(runtime: Arc<IngestRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl MemoryToolExecutor for FastMemoryToolExecutor {
    async fn execute_memory_tool(
        &self,
        session: &SessionMeta,
        tool_name: &str,
        input: &Value,
    ) -> moa_core::error::Result<ToolOutput> {
        execute_memory_tool(self.runtime.as_ref(), session, tool_name, input).await
    }

    async fn record_memory_incident(
        &self,
        session: &SessionMeta,
        turn_seq: i64,
        attempted: &str,
        failure: &str,
    ) -> moa_core::error::Result<Option<Uuid>> {
        record_incident(self.runtime.as_ref(), session, turn_seq, attempted, failure)
            .await
            .map_err(|error| MoaError::ToolError(error.to_string()))
    }
}

async fn fast_remember_inner(
    req: FastRememberRequest,
    ctx: &FastPathCtx,
) -> Result<Uuid, FastError> {
    validate_remember_request(&req)?;
    let embedder = require_configured_embedder(ctx.embedder.as_ref())?;

    let pii_result = ctx.pii.classify(&req.text).await;
    let pii = match pii_result {
        Ok(result) => result,
        Err(error) => {
            warn!(%error, "PII classifier failed in fast path; failing closed");
            PiiResult::fail_closed("fast-path-fallback")
        }
    };
    let redacted_text = safe_fast_path_text(&req.text, &pii)?;
    let embed_input = vec![redacted_text.clone()];
    let embedding = embedder
        .embed(&embed_input)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| FastError::Invalid("embedder returned no result".to_string()))?;
    if embedding.len() != VECTOR_DIMENSION {
        return Err(FastError::Invalid(format!(
            "expected {VECTOR_DIMENSION}-dimension embedding, got {}",
            embedding.len()
        )));
    }
    let query_embedding = moa_memory_vector::QueryEmbedding::new(embedding, embedder.model_id())?;

    let conflict = if let Some(old_uid) = req.supersedes_specific {
        Conflict::Supersede(old_uid)
    } else {
        let contradiction_ctx = ctx.contradiction_context().await?;
        match timeout(
            JUDGE_TIMEOUT,
            ctx.contradict.check_one_fast(
                &redacted_text,
                Some(query_embedding.clone()),
                req.label,
                pii.class,
                &contradiction_ctx,
            ),
        )
        .await
        {
            Ok(Ok(conflict)) => conflict,
            Ok(Err(error)) => {
                warn!(%error, "fast contradiction check failed; committing indeterminate fact");
                Conflict::Indeterminate
            }
            Err(_) => parse_supersedes_marker(&redacted_text)
                .map(Conflict::Supersede)
                .unwrap_or(Conflict::Indeterminate),
        }
    };

    if let Conflict::Duplicate(existing_uid) = conflict {
        // An agent restating a fact is a confirmation, not a no-op: reinforce
        // the survivor so decay and recency ranking treat it as live.
        // Best-effort — a failed boost must not fail the remember call.
        if let Err(error) = ctx
            .graph
            .reinforce_node(moa_memory_graph::NodeReinforcementIntent {
                uid: existing_uid,
                step: crate::extract::REINFORCE_CONFIDENCE_STEP,
                cap: crate::extract::REINFORCE_CONFIDENCE_CAP,
            })
            .await
        {
            warn!(%error, "fast-path duplicate reinforcement failed");
        }
        return Ok(existing_uid);
    }

    let confidence = if matches!(conflict, Conflict::Indeterminate) {
        0.5
    } else {
        0.9
    };
    let intent = build_intent(
        &req,
        query_embedding.vector(),
        pii.class,
        confidence,
        query_embedding.model(),
        embedder.model_version(),
        &redacted_text,
    );

    match conflict {
        Conflict::Supersede(old_uid) => ctx.graph.supersede_node(old_uid, intent).await,
        Conflict::Insert | Conflict::Indeterminate => ctx.graph.create_node(intent).await,
        Conflict::Duplicate(existing_uid) => Ok(existing_uid),
    }
    .map_err(FastError::from)
}

fn validate_remember_request(req: &FastRememberRequest) -> Result<(), FastError> {
    if req.text.trim().is_empty() {
        return Err(FastError::Invalid("empty text".to_string()));
    }
    match req.scope.as_str() {
        "tenant" if req.contact_id.is_none() => Ok(()),
        "contact" if req.contact_id.is_some() => Ok(()),
        "tenant" => Err(FastError::Invalid(
            "tenant scope must not include contact_id".to_string(),
        )),
        "contact" => Err(FastError::Invalid(
            "contact scope requires contact_id".to_string(),
        )),
        "global" => Err(FastError::Invalid(
            "fast memory writes cannot target global scope".to_string(),
        )),
        other => Err(FastError::Invalid(format!(
            "unsupported memory scope `{other}`"
        ))),
    }
}

fn safe_fast_path_text(text: &str, pii: &PiiResult) -> Result<String, FastError> {
    if pii.abstained && pii.spans.is_empty() {
        return Err(FastError::PiiClassificationUnavailable {
            model_version: pii.model_version.clone(),
        });
    }
    Ok(redact_text(text, &pii.spans))
}

fn build_intent(
    req: &FastRememberRequest,
    embedding: &[f32],
    pii_class: SensitivityClass,
    confidence: f64,
    embedding_model: &str,
    embedding_model_version: i32,
    redacted_text: &str,
) -> NodeWriteIntent {
    NodeWriteIntent {
        barrier: req.barrier.clone(),
        uid: Uuid::now_v7(),
        data_subject_id: req.contact_id.unwrap_or(req.tenant_id),
        label: req.label,
        storage_partition_id: Some(
            StoragePartitionId::for_tenant(TenantId::from(req.tenant_id)).to_string(),
        ),
        contact_id: req.contact_id.map(|contact_id| contact_id.to_string()),
        scope: req.scope.clone(),
        name: short_name(redacted_text),
        properties: json!({
            "summary": redacted_text,
            "source": "fast_path",
        }),
        pii_class,
        confidence: Some(confidence),
        valid_from: Utc::now(),
        embedding: Some(embedding.to_vec()),
        embedding_model: Some(embedding_model.to_string()),
        embedding_model_version: Some(embedding_model_version),
        embedding_text: None,
        actor_id: req.actor_id.to_string(),
        actor_kind: req.actor_kind.clone(),
    }
}

async fn active_uids_for_pattern(
    pattern: &ForgetPattern,
    ctx: &FastPathCtx,
) -> Result<Vec<Uuid>, FastError> {
    let mut conn = begin_scoped(ctx).await?;
    let uids = match pattern {
        ForgetPattern::Uid(uid) => {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT uid FROM moa.node_index WHERE uid = $1 AND valid_to IS NULL",
            )
            .bind(uid)
            .fetch_all(conn.as_mut())
            .await?
        }
        ForgetPattern::NameMatch(name) => {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT uid FROM moa.node_index WHERE name = $1 AND valid_to IS NULL ORDER BY uid",
            )
            .bind(name)
            .fetch_all(conn.as_mut())
            .await?
        }
        ForgetPattern::SoftAll(contact_id) => sqlx::query_scalar::<_, Uuid>(
            "SELECT uid FROM moa.node_index WHERE user_id = $1 AND valid_to IS NULL ORDER BY uid",
        )
        .bind(contact_id.to_string())
        .fetch_all(conn.as_mut())
        .await?,
    };
    conn.commit().await?;
    Ok(uids)
}

async fn begin_scoped(ctx: &FastPathCtx) -> Result<ScopedConn<'_>, FastError> {
    let conn = ScopedConn::begin_as_app(&ctx.pool, &ctx.scope, ctx.assume_app_role).await?;
    Ok(conn)
}

fn short_name(text: &str) -> String {
    let trimmed = text.trim();
    let first_sentence = trimmed
        .split('\n')
        .next()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(trimmed)
        .trim();
    first_sentence.chars().take(80).collect()
}

fn parse_supersedes_marker(text: &str) -> Option<Uuid> {
    let mut previous_was_supersedes = false;
    for token in text.split_whitespace() {
        let stripped = token.strip_prefix("supersedes:");
        if token.eq_ignore_ascii_case("supersedes") {
            previous_was_supersedes = true;
            continue;
        }
        if let Some(candidate) = stripped.or(previous_was_supersedes.then_some(token)) {
            let candidate = candidate.trim_matches([',', ';', '.']);
            if let Ok(uid) = Uuid::parse_str(candidate) {
                return Some(uid);
            }
        }
        previous_was_supersedes = false;
    }
    None
}

fn record_remember_metrics(elapsed: Duration, result: &Result<Uuid, FastError>) {
    metrics::histogram!("moa_fast_remember_latency_seconds").record(elapsed.as_secs_f64());
    let outcome = if result.is_ok() { "ok" } else { "error" };
    metrics::counter!("moa_fast_remember_total", "outcome" => outcome).increment(1);
}

/// Maximum number of facts one `memory_remember` invocation may store.
const MAX_REMEMBER_BATCH: usize = 32;

/// Reason surfaced when a contact-scoped item is requested on a session that has
/// no contact; the item is rejected rather than silently widened to tenant scope.
const CONTACT_SCOPE_WITHOUT_CONTACT_REASON: &str =
    "contact-scoped memory requires a contact on this session";

#[derive(Debug, Deserialize, Serialize)]
struct RememberBatchInput {
    items: Vec<RememberItemInput>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RememberItemInput {
    text: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    supersedes_specific: Option<Uuid>,
}

/// Outcome of one item inside a batched remember call.
#[derive(Debug)]
enum RememberOutcome {
    /// The fact was written; carries the new graph node id.
    Stored(Uuid),
    /// The fact was not written; carries a short human-readable reason.
    Rejected(String),
}

#[derive(Debug, Deserialize)]
struct ForgetToolInput {
    #[serde(default)]
    uid: Option<Uuid>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "soft_all_user_id")]
    soft_all_contact_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
struct SupersedeToolInput {
    old_uid: Uuid,
    new_text: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

async fn execute_remember_tool(
    runtime: &IngestRuntime,
    session: &SessionMeta,
    input: &Value,
    started: Instant,
) -> Result<ToolOutput, FastError> {
    let batch: RememberBatchInput = serde_json::from_value(input.clone())?;
    validate_remember_batch_len(batch.items.len())?;
    let barrier = pinned_write_barrier(session)?;

    // One runtime context per requested scope is reused across items so a large
    // batch does not rebuild the graph/vector/PII stack per fact. Item failures
    // are recorded and never abort the remaining items.
    let mut scope_ctxs: std::collections::HashMap<String, (FastPathCtx, Uuid, Option<Uuid>)> =
        std::collections::HashMap::new();
    let mut outcomes = Vec::with_capacity(batch.items.len());
    for item in &batch.items {
        outcomes.push(
            remember_one_item(runtime, session, item, barrier.as_ref(), &mut scope_ctxs).await,
        );
    }
    Ok(batch_remember_output(&outcomes, started))
}

fn validate_remember_batch_len(len: usize) -> Result<(), FastError> {
    if len == 0 {
        return Err(FastError::Invalid(
            "remember requires at least one item".to_string(),
        ));
    }
    if len > MAX_REMEMBER_BATCH {
        return Err(FastError::Invalid(format!(
            "remember accepts at most {MAX_REMEMBER_BATCH} items per call, got {len}"
        )));
    }
    Ok(())
}

/// Writes one batch item, returning a per-item outcome instead of erroring so a
/// single bad item never aborts the rest of the batch.
async fn remember_one_item(
    runtime: &IngestRuntime,
    session: &SessionMeta,
    item: &RememberItemInput,
    barrier: Option<&InformationBarrierId>,
    scope_ctxs: &mut std::collections::HashMap<String, (FastPathCtx, Uuid, Option<Uuid>)>,
) -> RememberOutcome {
    let label = match parse_node_label(item.label.as_deref()) {
        Ok(label) => label,
        Err(error) => return RememberOutcome::Rejected(remember_item_reason(&error)),
    };
    if requested_contact_scope_without_contact(session, item.scope.as_deref()) {
        return RememberOutcome::Rejected(CONTACT_SCOPE_WITHOUT_CONTACT_REASON.to_string());
    }
    let scope = requested_write_scope(item.scope.as_deref());
    let (ctx, tenant_id, contact_id) =
        match scoped_ctx_for(runtime, session, &scope, barrier, scope_ctxs).await {
            Ok(triple) => triple,
            Err(error) => return RememberOutcome::Rejected(remember_item_reason(&error)),
        };
    let actor_id = actor_id_from_session(session);
    match fast_remember(
        FastRememberRequest {
            tenant_id,
            contact_id,
            scope,
            text: item.text.clone(),
            label,
            supersedes_specific: item.supersedes_specific,
            barrier: barrier.cloned(),
            actor_id,
            actor_kind: "user".to_string(),
        },
        &ctx,
    )
    .await
    {
        Ok(uid) => RememberOutcome::Stored(uid),
        Err(error) => RememberOutcome::Rejected(remember_item_reason(&error)),
    }
}

/// Returns the scope's runtime context, building and caching it on first use.
async fn scoped_ctx_for(
    runtime: &IngestRuntime,
    session: &SessionMeta,
    scope: &str,
    barrier: Option<&InformationBarrierId>,
    scope_ctxs: &mut std::collections::HashMap<String, (FastPathCtx, Uuid, Option<Uuid>)>,
) -> Result<(FastPathCtx, Uuid, Option<Uuid>), FastError> {
    if let Some(existing) = scope_ctxs.get(scope) {
        return Ok(existing.clone());
    }
    let built = runtime_ctx_for_scope(runtime, session, scope, barrier).await?;
    scope_ctxs.insert(scope.to_string(), built.clone());
    Ok(built)
}

/// Maps a fast-path error to a short per-item rejection reason for batch results.
fn remember_item_reason(error: &FastError) -> String {
    match error {
        FastError::Invalid(message) => message.clone(),
        FastError::PiiClassificationUnavailable { .. } => {
            "privacy classification unavailable".to_string()
        }
        FastError::ConfiguredEmbedderUnavailable => {
            "configured ingestion embedder unavailable".to_string()
        }
        FastError::Timeout(operation) => format!("timed out during {operation}"),
        _ => "memory storage unavailable".to_string(),
    }
}

/// Builds the single tool result reporting every item's outcome.
///
/// Partial success (at least one stored) is a successful result. When every item
/// is rejected the result is marked as an error and carries the standard "do not
/// retry this turn" guidance, matching the single-fact environmental-failure
/// contract so the turn loop does not re-run the tool.
fn batch_remember_output(outcomes: &[RememberOutcome], started: Instant) -> ToolOutput {
    let total = outcomes.len();
    let stored = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, RememberOutcome::Stored(_)))
        .count();
    let rejected = total - stored;

    let results: Vec<Value> = outcomes
        .iter()
        .enumerate()
        .map(|(index, outcome)| match outcome {
            RememberOutcome::Stored(uid) => {
                json!({ "index": index, "status": "stored", "uid": uid })
            }
            RememberOutcome::Rejected(reason) => {
                json!({ "index": index, "status": "rejected", "reason": reason })
            }
        })
        .collect();

    let mut summary = format!("Remembered {stored} of {total} fact(s).");
    if rejected > 0 {
        let details: Vec<String> = outcomes
            .iter()
            .enumerate()
            .filter_map(|(index, outcome)| match outcome {
                RememberOutcome::Rejected(reason) => Some(format!("index {index}: {reason}")),
                RememberOutcome::Stored(_) => None,
            })
            .collect();
        summary.push_str(&format!(" Rejected {rejected} ({}).", details.join("; ")));
    }

    let data = json!({
        "operation": "remember",
        "stored": stored,
        "rejected": rejected,
        "results": results,
    });

    if stored == 0 {
        summary.push_str(
            " Do not retry this memory tool in this turn; continue using the current session context.",
        );
        let mut output = ToolOutput::json(summary, data, started.elapsed());
        output.is_error = true;
        output
    } else {
        ToolOutput::json(summary, data, started.elapsed())
    }
}

async fn execute_forget_tool(
    runtime: &IngestRuntime,
    session: &SessionMeta,
    input: &Value,
    started: Instant,
) -> Result<ToolOutput, FastError> {
    let params: ForgetToolInput = serde_json::from_value(input.clone())?;
    let barrier = pinned_write_barrier(session)?;
    let count = match (params.uid, params.name, params.soft_all_contact_id) {
        (Some(uid), None, None) => {
            let ctx =
                runtime_ctx_for_visible_session_scope(runtime, session, barrier.as_ref()).await?;
            fast_forget(ForgetPattern::Uid(uid), &ctx).await?
        }
        (None, Some(name), None) => {
            let ctx =
                runtime_ctx_for_visible_session_scope(runtime, session, barrier.as_ref()).await?;
            fast_forget(ForgetPattern::NameMatch(name), &ctx).await?
        }
        (None, None, Some(contact_id)) => {
            let ctx =
                runtime_ctx_for_contact(runtime, session, contact_id, barrier.as_ref()).await?;
            fast_forget(ForgetPattern::SoftAll(contact_id), &ctx).await?
        }
        _ => {
            return Err(FastError::Invalid(
                "provide exactly one of uid, name, or soft_all_user_id".to_string(),
            ));
        }
    };
    Ok(forget_output(count, started))
}

fn forget_output(count: u64, started: Instant) -> ToolOutput {
    ToolOutput::json(
        format!("Forgot {count} graph memory node(s)"),
        json!({ "invalidated": count, "operation": "forget" }),
        started.elapsed(),
    )
}

async fn execute_supersede_tool(
    runtime: &IngestRuntime,
    session: &SessionMeta,
    input: &Value,
    started: Instant,
) -> Result<ToolOutput, FastError> {
    let params: SupersedeToolInput = serde_json::from_value(input.clone())?;
    let batch = RememberBatchInput {
        items: vec![RememberItemInput {
            text: params.new_text,
            label: params.label,
            scope: params.scope,
            supersedes_specific: Some(params.old_uid),
        }],
    };
    execute_remember_tool(runtime, session, &serde_json::to_value(batch)?, started).await
}

async fn runtime_ctx_for_scope(
    runtime: &IngestRuntime,
    session: &SessionMeta,
    scope: &str,
    barrier: Option<&InformationBarrierId>,
) -> Result<(FastPathCtx, Uuid, Option<Uuid>), FastError> {
    let tenant_id = tenant_uuid(session);
    let contact_id = match scope {
        "tenant" => None,
        "contact" => Some(session_contact_uuid(session)?),
        other => {
            return Err(FastError::Invalid(format!(
                "unsupported memory scope `{other}`"
            )));
        }
    };
    let scope_ctx = match contact_id {
        Some(contact_id) => RlsContext::contact(TenantId::from(tenant_id), ContactId(contact_id)),
        None => RlsContext::tenant(TenantId::from(tenant_id)),
    };
    let scope_ctx = with_write_barrier_clearance(scope_ctx, barrier);
    Ok((
        runtime_fast_ctx(runtime, scope_ctx).await?,
        tenant_id,
        contact_id,
    ))
}

fn requested_contact_scope_without_contact(
    session: &SessionMeta,
    requested_scope: Option<&str>,
) -> bool {
    requested_scope == Some("contact") && session.contact.is_none()
}

fn requested_write_scope(requested_scope: Option<&str>) -> String {
    requested_scope.unwrap_or("tenant").to_string()
}

fn memory_tool_failure_output(tool_name: &str, error: &FastError, started: Instant) -> ToolOutput {
    let message = match error {
        FastError::Invalid(message) => {
            format!("{tool_name} input was invalid: {message}")
        }
        FastError::PiiClassificationUnavailable { .. } => {
            format!(
                "{tool_name} did not persist memory because privacy classification is unavailable. Do not retry this memory tool in this turn; continue using the current session context."
            )
        }
        FastError::ConfiguredEmbedderUnavailable => {
            format!(
                "{tool_name} did not persist memory because the configured ingestion embedder is unavailable. Do not retry this memory tool in this turn; continue using the current session context."
            )
        }
        FastError::Timeout(operation) => {
            format!(
                "{tool_name} timed out while running {operation}. Do not retry this memory tool in this turn; continue using the current session context and retry later if needed."
            )
        }
        _ => {
            format!(
                "{tool_name} could not persist or read graph memory because memory storage is unavailable. Do not retry this memory tool in this turn; continue using the current session context and complete any remaining user request."
            )
        }
    };
    ToolOutput::error(message, started.elapsed())
}

async fn runtime_ctx_for_contact(
    runtime: &IngestRuntime,
    session: &SessionMeta,
    contact_id: Uuid,
    barrier: Option<&InformationBarrierId>,
) -> Result<FastPathCtx, FastError> {
    let tenant_id = tenant_uuid(session);
    let scope_ctx = RlsContext::contact(TenantId::from(tenant_id), ContactId(contact_id));
    let scope_ctx = with_write_barrier_clearance(scope_ctx, barrier);
    runtime_fast_ctx(runtime, scope_ctx).await
}

async fn runtime_ctx_for_visible_session_scope(
    runtime: &IngestRuntime,
    session: &SessionMeta,
    barrier: Option<&InformationBarrierId>,
) -> Result<FastPathCtx, FastError> {
    if let Some(contact) = &session.contact {
        runtime_ctx_for_contact(runtime, session, contact.contact_id.0, barrier).await
    } else {
        let (ctx, _, _) = runtime_ctx_for_scope(runtime, session, "tenant", barrier).await?;
        Ok(ctx)
    }
}

fn tenant_uuid(session: &SessionMeta) -> Uuid {
    session.tenant_id.0
}

fn pinned_write_barrier(session: &SessionMeta) -> Result<Option<InformationBarrierId>, FastError> {
    let Some(agent_context) = session.agent_context.as_ref() else {
        return Ok(None);
    };
    let policy = agent_context.parsed_policy_snapshot()?.knowledge_policy;
    policy.validate()?;
    Ok(policy.write_barrier)
}

fn with_write_barrier_clearance(
    scope: RlsContext,
    barrier: Option<&InformationBarrierId>,
) -> RlsContext {
    match barrier {
        Some(barrier) => scope.with_cleared_barriers([barrier.clone()].into_iter().collect()),
        None => scope,
    }
}

fn session_contact_uuid(session: &SessionMeta) -> Result<Uuid, FastError> {
    session
        .contact
        .as_ref()
        .map(|contact| contact.contact_id.0)
        .ok_or_else(|| {
            FastError::Invalid("contact-scoped graph memory requires a session contact".to_string())
        })
}

fn actor_id_from_session(session: &SessionMeta) -> Uuid {
    match &session.created_by {
        Some(SessionActorRef::Identity { id }) => *id,
        Some(SessionActorRef::Contact { id }) => id.0,
        Some(SessionActorRef::Anonymous) | None => Uuid::now_v7(),
    }
}

async fn runtime_fast_ctx(
    runtime: &IngestRuntime,
    scope: RlsContext,
) -> Result<FastPathCtx, FastError> {
    let pool = runtime.pool().clone();
    let vector_factory = runtime.vector_store_factory();
    let graph_vector =
        vector_factory.transactional_graph_backend(pool.clone(), scope.clone(), false);
    let graph = Arc::new(
        PostgresGraphStore::scoped(pool.clone(), scope.clone(), runtime.kms())
            .with_vector_store(graph_vector.vector_store()),
    );
    let embedder = runtime.embedder();
    let pii = runtime.pii_classifier();
    let contradict = runtime.contradiction_detector();

    Ok(FastPathCtx::new_with_optional_embedder(
        pool,
        scope,
        graph,
        vector_factory,
        embedder,
        pii,
        contradict,
    ))
}

fn require_configured_embedder(
    embedder: Option<&Arc<dyn EmbeddingProvider>>,
) -> Result<&Arc<dyn EmbeddingProvider>, FastError> {
    embedder.ok_or(FastError::ConfiguredEmbedderUnavailable)
}

fn parse_node_label(value: Option<&str>) -> Result<NodeLabel, FastError> {
    match value {
        Some(label) => {
            NodeLabel::from_str(label).map_err(|error| FastError::Invalid(error.to_string()))
        }
        None => Ok(NodeLabel::Fact),
    }
}

#[cfg(test)]
mod tests {
    use moa_config::MoaConfig;
    use moa_core::{
        types::agent::{AgentContext, AgentKnowledgePolicy, AgentPolicySnapshot},
        types::contact::ContactId,
        types::contact::ContactRef,
        types::contact::ContactVerificationState,
        types::identifiers::TenantId,
        types::session::SessionMeta,
        types::tools::ToolContent,
    };
    use moa_crypto::{KeyManagementProvider, LocalKmsProvider};
    use serde_json::json;
    use sqlx::postgres::PgPoolOptions;
    use uuid::Uuid;

    use super::*;

    fn lazy_pool() -> PgPool {
        PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect")
    }

    fn test_kms() -> Arc<dyn KeyManagementProvider> {
        static KMS: std::sync::OnceLock<Arc<dyn KeyManagementProvider>> =
            std::sync::OnceLock::new();
        KMS.get_or_init(|| Arc::new(LocalKmsProvider::new()))
            .clone()
    }

    fn test_runtime() -> IngestRuntime {
        IngestRuntime::new(lazy_pool(), test_kms()).expect("test ingestion runtime")
    }

    fn session_with_knowledge_policy(policy: AgentKnowledgePolicy) -> SessionMeta {
        let mut agent_context = AgentContext::system_default();
        agent_context.policy_snapshot = json!(AgentPolicySnapshot {
            knowledge_policy: policy,
            ..AgentPolicySnapshot::default()
        });
        SessionMeta {
            agent_context: Some(agent_context),
            ..SessionMeta::default()
        }
    }

    #[test]
    fn pinned_write_barrier_comes_from_validated_session_policy() {
        // Pins: session-facing remember, supersede, and incident paths share the
        // typed barrier copied onto the pinned agent policy; sessions without a
        // pinned agent remain unbarriered.
        let barrier = InformationBarrierId::parse("deal-alpha").expect("valid barrier");
        let session = session_with_knowledge_policy(AgentKnowledgePolicy {
            cleared_barriers: [barrier.clone()].into_iter().collect(),
            write_barrier: Some(barrier.clone()),
            ..AgentKnowledgePolicy::default()
        });

        assert_eq!(
            pinned_write_barrier(&session).expect("valid policy"),
            Some(barrier)
        );
        assert_eq!(
            pinned_write_barrier(&SessionMeta::default()).expect("unpinned session"),
            None
        );
    }

    #[test]
    fn pinned_write_barrier_rejects_policy_without_matching_clearance() {
        // Pins: a malformed pinned policy fails closed before a fast-memory write
        // can turn a restricted session into an unrestricted memory node.
        let session = session_with_knowledge_policy(AgentKnowledgePolicy {
            write_barrier: Some(InformationBarrierId::parse("deal-alpha").expect("valid barrier")),
            ..AgentKnowledgePolicy::default()
        });

        assert!(matches!(
            pinned_write_barrier(&session),
            Err(FastError::Core(MoaError::ValidationError(_)))
        ));
    }

    #[tokio::test]
    async fn runtime_fast_ctx_reuses_configured_ingestion_embedder() {
        // Pins: installed slow, fast, and entity paths share one provider client;
        // a fast command must not construct another client with the same model ID.
        let mut config = MoaConfig::default();
        config.memory.vector.embedder.name = "gemini:gemini-embedding-2".to_string();
        config.providers.google.api_key = "test-google-key".to_string();
        let runtime = crate::IngestRuntime::from_config(lazy_pool(), test_kms(), &config)
            .expect("configured runtime should build without a provider call");
        let slow_embedder = runtime
            .embedder()
            .expect("slow runtime embedder should be configured");
        let entity_embedder = runtime
            .entity_blocking_embedder()
            .expect("entity blocking embedder should be configured");

        let fast = runtime_fast_ctx(
            &runtime,
            RlsContext::tenant(TenantId::from(Uuid::from_u128(1))),
        )
        .await
        .expect("fast context should reuse the configured runtime provider");
        let fast_embedder = fast
            .embedder
            .as_ref()
            .expect("fast vector writes should have the configured embedder");

        assert_eq!(fast_embedder.model_id(), "gemini-embedding-2");
        assert!(Arc::ptr_eq(&slow_embedder, fast_embedder));
        assert!(Arc::ptr_eq(&entity_embedder, fast_embedder));
    }

    #[tokio::test]
    async fn vector_operations_fail_with_configured_embedder_unavailable() {
        // Pins: remember, supersede-via-remember, and incident writes all use one
        // dedicated missing-embedder boundary before attempting provider or DB I/O.
        let mut config = MoaConfig::default();
        config.memory.vector.embedder.name = "disabled".to_string();
        let runtime = crate::IngestRuntime::from_config(lazy_pool(), test_kms(), &config)
            .expect("disabled embedding should leave fast graph access available");
        let ctx = runtime_fast_ctx(
            &runtime,
            RlsContext::tenant(TenantId::from(Uuid::from_u128(3))),
        )
        .await
        .expect("fast context should build without an embedder");
        let remember_request = FastRememberRequest {
            tenant_id: Uuid::from_u128(3),
            contact_id: None,
            scope: "tenant".to_string(),
            text: "the api uses postgres".to_string(),
            label: NodeLabel::Fact,
            supersedes_specific: None,
            barrier: None,
            actor_id: Uuid::from_u128(4),
            actor_kind: "user".to_string(),
        };

        let remember_error = fast_remember(remember_request.clone(), &ctx)
            .await
            .expect_err("remember requires the configured ingestion embedder");
        let supersede_error = fast_remember(
            FastRememberRequest {
                supersedes_specific: Some(Uuid::from_u128(5)),
                ..remember_request
            },
            &ctx,
        )
        .await
        .expect_err("supersede-via-remember requires the configured ingestion embedder");
        let incident_error = record_incident_with_ctx(
            IncidentRecord {
                tenant_id: Uuid::from_u128(3),
                contact_id: None,
                scope: "tenant".to_string(),
                session_id: Uuid::from_u128(6),
                turn_seq: 1,
                attempted: "memory_search".to_string(),
                failure: "timeout".to_string(),
                barrier: None,
                actor_id: Uuid::from_u128(4),
                actor_kind: "system".to_string(),
            },
            &ctx,
        )
        .await
        .expect_err("incident capture requires the configured ingestion embedder");

        assert!(matches!(
            remember_error,
            FastError::ConfiguredEmbedderUnavailable
        ));
        assert!(matches!(
            supersede_error,
            FastError::ConfiguredEmbedderUnavailable
        ));
        assert!(matches!(
            incident_error,
            FastError::ConfiguredEmbedderUnavailable
        ));
        let output = memory_tool_failure_output("memory_remember", &remember_error, Instant::now());
        let texts = output
            .content
            .iter()
            .filter_map(|content| match content {
                ToolContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec![
                "memory_remember did not persist memory because the configured ingestion embedder is unavailable. Do not retry this memory tool in this turn; continue using the current session context."
            ]
        );
    }

    #[tokio::test]
    async fn runtime_fast_ctx_without_embedder_remains_available_for_forget() {
        // Pins: privacy deletion can build its graph context when embedding is
        // disabled; only a vector-producing operation asks for an embedder.
        let mut config = MoaConfig::default();
        config.memory.vector.embedder.name = "disabled".to_string();
        let runtime = crate::IngestRuntime::from_config(lazy_pool(), test_kms(), &config)
            .expect("disabled embedding should not disable graph deletion");

        let fast = runtime_fast_ctx(
            &runtime,
            RlsContext::tenant(TenantId::from(Uuid::from_u128(2))),
        )
        .await
        .expect("forget context should not require an embedding provider");

        assert!(fast.embedder.is_none());
        assert!(matches!(
            require_configured_embedder(fast.embedder.as_ref()),
            Err(FastError::ConfiguredEmbedderUnavailable)
        ));
    }

    #[test]
    fn requested_contact_scope_without_contact_detects_invalid_contact_write() {
        // Pins: no-contact sessions must not silently widen contact memory to tenant memory.
        let session = SessionMeta {
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            contact: None,
            ..SessionMeta::default()
        };

        assert!(requested_contact_scope_without_contact(
            &session,
            Some("contact")
        ));
        assert_eq!(requested_write_scope(None), "tenant");
    }

    #[test]
    fn requested_contact_scope_without_contact_allows_contact_session() {
        // Pins: actual contact sessions keep their contact-owned memory boundary.
        let tenant_id = TenantId::from(Uuid::from_u128(1));
        let session = SessionMeta {
            tenant_id,
            contact: Some(ContactRef {
                contact_id: ContactId(Uuid::from_u128(2)),
                tenant_id,
                state: ContactVerificationState::Verified,
                canonical_contact_id: None,
                linked_contact_ids: Vec::new(),
                scopes: Vec::new(),
                permissions: json!({}),
                agent_ids: Vec::new(),
                session_ids: Vec::new(),
                verified_contact_point_ids: Vec::new(),
            }),
            ..SessionMeta::default()
        };

        assert!(!requested_contact_scope_without_contact(
            &session,
            Some("contact")
        ));
        assert_eq!(requested_write_scope(Some("contact")), "contact");
    }

    #[test]
    fn memory_backend_failure_returns_recoverable_tool_error() {
        // Pins: unavailable graph/vector memory should not fail the whole turn.
        let output = memory_tool_failure_output(
            "memory_remember",
            &FastError::Timeout("remember"),
            Instant::now(),
        );

        assert!(output.is_error);
        assert!(
            output.content.iter().any(
                |content| matches!(content, ToolContent::Text { text } if text.contains("continue using the current session context"))
            )
        );
        assert!(
            output.content.iter().any(
                |content| matches!(content, ToolContent::Text { text } if text.contains("Do not retry this memory tool in this turn"))
            )
        );
    }

    #[test]
    fn batch_remember_output_reports_each_item_and_flags_all_rejected() {
        // Pins: a batched remember result reports one entry per item with its
        // outcome; partial success stays a success, and an all-rejected batch is
        // an error carrying the "do not retry this turn" guidance so the turn
        // loop does not re-run the tool (batch-remember pacing fix, 2026-07-18).
        let partial = batch_remember_output(
            &[
                RememberOutcome::Stored(Uuid::from_u128(1)),
                RememberOutcome::Rejected("empty text".to_string()),
            ],
            Instant::now(),
        );
        assert!(!partial.is_error, "partial success is not an error");
        let data = partial.structured.expect("structured payload");
        assert_eq!(data["stored"], 1);
        assert_eq!(data["rejected"], 1);
        assert_eq!(data["results"][0]["status"], "stored");
        assert_eq!(data["results"][0]["index"], 0);
        assert_eq!(
            data["results"][0]["uid"],
            json!(Uuid::from_u128(1).to_string())
        );
        assert_eq!(data["results"][1]["status"], "rejected");
        assert_eq!(data["results"][1]["index"], 1);
        assert_eq!(data["results"][1]["reason"], "empty text");

        let all_rejected = batch_remember_output(
            &[
                RememberOutcome::Rejected("a".to_string()),
                RememberOutcome::Rejected("b".to_string()),
            ],
            Instant::now(),
        );
        assert!(all_rejected.is_error, "an all-rejected batch is an error");
        assert!(
            all_rejected
                .to_text()
                .contains("Do not retry this memory tool in this turn")
        );
        assert_eq!(all_rejected.structured.expect("payload")["stored"], 0);
    }

    #[tokio::test]
    async fn execute_remember_batch_rejects_each_bad_item_without_aborting() {
        // Pins: one invalid item never aborts the rest — every item is processed
        // and reported independently. Uses items that fail before any runtime is
        // needed (unknown label, contact scope without a contact) so the fan-in
        // aggregation is exercised offline.
        let session = SessionMeta {
            tenant_id: TenantId::from(Uuid::from_u128(1)),
            contact: None,
            ..SessionMeta::default()
        };
        let input = json!({
            "items": [
                { "text": "first", "label": "NotALabel" },
                { "text": "second", "scope": "contact" },
                { "text": "third", "label": "AlsoNotALabel" }
            ]
        });

        let output = execute_remember_tool(&test_runtime(), &session, &input, Instant::now())
            .await
            .expect("batch execution should return per-item results, not error out");

        assert!(
            output.is_error,
            "every item rejected marks the batch failed"
        );
        let data = output.structured.expect("structured payload");
        assert_eq!(data["stored"], 0);
        assert_eq!(data["rejected"], 3);
        let results = data["results"].as_array().expect("results array");
        assert_eq!(results.len(), 3, "all items are reported, none aborted");
        assert_eq!(results[0]["status"], "rejected");
        assert_eq!(results[1]["status"], "rejected");
        assert_eq!(results[1]["reason"], CONTACT_SCOPE_WITHOUT_CONTACT_REASON);
        assert_eq!(results[2]["status"], "rejected");
    }

    #[tokio::test]
    async fn execute_remember_batch_rejects_empty_and_oversized() {
        // Pins: batch size is bounded — an empty item list and a list over the
        // per-call cap are both rejected before any write is attempted.
        let session = SessionMeta::default();

        let runtime = test_runtime();
        let empty =
            execute_remember_tool(&runtime, &session, &json!({ "items": [] }), Instant::now())
                .await;
        assert!(matches!(empty, Err(FastError::Invalid(_))), "empty batch");

        let oversized_items: Vec<Value> = (0..=MAX_REMEMBER_BATCH)
            .map(|index| json!({ "text": format!("fact {index}") }))
            .collect();
        let oversized = execute_remember_tool(
            &runtime,
            &session,
            &json!({ "items": oversized_items }),
            Instant::now(),
        )
        .await;
        assert!(
            matches!(oversized, Err(FastError::Invalid(_))),
            "batch over the cap of {MAX_REMEMBER_BATCH} is rejected"
        );
    }
}
