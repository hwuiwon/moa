//! Low-latency graph-memory ingestion for explicit remember, forget, and supersede commands.

use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use moa_core::RlsContext;
use moa_core::{
    ContactId, MemoryToolExecutor, MoaError, SessionActorRef, SessionMeta, StoragePartitionId,
    TenantId, ToolOutput, traits::EmbeddingProvider,
};
use moa_db::ScopedConn;
use moa_memory_graph::{
    GraphError, GraphStore, NodeLabel, NodeWriteIntent, PiiClass, PostgresGraphStore,
};
use moa_memory_pii::{
    OpenAiPrivacyFilterClassifier, PiiClassifier, PiiError, PiiResult, redact_text,
};
use moa_memory_vector::{Error as VectorError, PgvectorStore, VECTOR_DIMENSION, VectorStore};
use moa_providers::CohereV4Embedder;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::time::timeout;
use tracing::warn;
use uuid::Uuid;

use crate::{Conflict, ContradictionContext, ContradictionDetector, IngestError, current_runtime};

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
    vector: Arc<dyn VectorStore>,
    embedder: Arc<dyn EmbeddingProvider>,
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
        Self {
            pool,
            scope,
            graph,
            vector,
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

    fn contradiction_context(&self) -> ContradictionContext {
        if self.assume_app_role {
            ContradictionContext::for_app_role(
                self.pool.clone(),
                self.scope.clone(),
                self.vector.clone(),
            )
        } else {
            ContradictionContext::new(self.pool.clone(), self.scope.clone(), self.vector.clone())
        }
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
    Ingest(#[from] IngestError),
}

/// Returns whether a tool name is handled by the graph-backed fast memory path.
#[must_use]
pub fn is_fast_memory_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "memory_remember" | "memory_forget" | "memory_supersede"
    )
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

/// Executes a memory tool request using the installed orchestrator runtime.
pub async fn execute_memory_tool(
    session: &SessionMeta,
    tool_name: &str,
    input: &Value,
) -> moa_core::Result<ToolOutput> {
    let started = Instant::now();
    let output = match tool_name {
        "memory_remember" => execute_remember_tool(session, input, started).await,
        "memory_forget" => execute_forget_tool(session, input, started).await,
        "memory_supersede" => execute_supersede_tool(session, input, started).await,
        _ => Err(FastError::Invalid(format!(
            "unknown fast memory tool {tool_name}"
        ))),
    };
    output.map_err(|error| MoaError::ToolError(error.to_string()))
}

/// Runtime adapter that lets `moa-hands` execute graph-memory tools without depending on ingest.
#[derive(Debug, Default, Clone, Copy)]
pub struct FastMemoryToolExecutor;

#[async_trait]
impl MemoryToolExecutor for FastMemoryToolExecutor {
    async fn execute_memory_tool(
        &self,
        session: &SessionMeta,
        tool_name: &str,
        input: &Value,
    ) -> moa_core::Result<ToolOutput> {
        execute_memory_tool(session, tool_name, input).await
    }
}

async fn fast_remember_inner(
    req: FastRememberRequest,
    ctx: &FastPathCtx,
) -> Result<Uuid, FastError> {
    validate_remember_request(&req)?;

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
    let embedding = ctx
        .embedder
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

    let conflict = if let Some(old_uid) = req.supersedes_specific {
        Conflict::Supersede(old_uid)
    } else {
        let contradiction_ctx = ctx.contradiction_context();
        match timeout(
            JUDGE_TIMEOUT,
            ctx.contradict.check_one_fast(
                &redacted_text,
                &embedding,
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
            Err(_) => {
                metrics::counter!("moa_fast_remember_indeterminate_total").increment(1);
                parse_supersedes_marker(&redacted_text)
                    .map(Conflict::Supersede)
                    .unwrap_or(Conflict::Indeterminate)
            }
        }
    };

    if let Conflict::Duplicate(existing_uid) = conflict {
        return Ok(existing_uid);
    }

    let confidence = if matches!(conflict, Conflict::Indeterminate) {
        0.5
    } else {
        0.9
    };
    let intent = build_intent(
        &req,
        &embedding,
        pii.class,
        confidence,
        ctx.embedder.model_name(),
        ctx.embedder.model_version(),
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
    pii_class: PiiClass,
    confidence: f64,
    embedding_model: &str,
    embedding_model_version: i32,
    redacted_text: &str,
) -> NodeWriteIntent {
    NodeWriteIntent {
        uid: Uuid::now_v7(),
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
    let mut conn = ScopedConn::begin(&ctx.pool, &ctx.scope).await?;
    if ctx.assume_app_role {
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(conn.as_mut())
            .await?;
    }
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

#[derive(Debug, Deserialize, Serialize)]
struct RememberToolInput {
    text: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    supersedes_specific: Option<Uuid>,
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
    session: &SessionMeta,
    input: &Value,
    started: Instant,
) -> Result<ToolOutput, FastError> {
    let params: RememberToolInput = serde_json::from_value(input.clone())?;
    let label = parse_node_label(params.label.as_deref())?;
    let scope = params.scope.unwrap_or_else(|| "tenant".to_string());
    let (ctx, tenant_id, contact_id) = runtime_ctx_for_scope(session, &scope)?;
    let actor_id = actor_id_from_session(session);
    let uid = fast_remember(
        FastRememberRequest {
            tenant_id,
            contact_id,
            scope,
            text: params.text,
            label,
            supersedes_specific: params.supersedes_specific,
            actor_id,
            actor_kind: "user".to_string(),
        },
        &ctx,
    )
    .await?;
    Ok(ToolOutput::json(
        format!("Remembered graph memory node {uid}"),
        json!({ "uid": uid, "operation": "remember" }),
        started.elapsed(),
    ))
}

async fn execute_forget_tool(
    session: &SessionMeta,
    input: &Value,
    started: Instant,
) -> Result<ToolOutput, FastError> {
    let params: ForgetToolInput = serde_json::from_value(input.clone())?;
    let count = match (params.uid, params.name, params.soft_all_contact_id) {
        (Some(uid), None, None) => {
            let ctx = runtime_ctx_for_visible_session_scope(session)?;
            fast_forget(ForgetPattern::Uid(uid), &ctx).await?
        }
        (None, Some(name), None) => {
            let ctx = runtime_ctx_for_visible_session_scope(session)?;
            fast_forget(ForgetPattern::NameMatch(name), &ctx).await?
        }
        (None, None, Some(contact_id)) => {
            let ctx = runtime_ctx_for_contact(session, contact_id)?;
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
    session: &SessionMeta,
    input: &Value,
    started: Instant,
) -> Result<ToolOutput, FastError> {
    let params: SupersedeToolInput = serde_json::from_value(input.clone())?;
    let remember = RememberToolInput {
        text: params.new_text,
        label: params.label,
        scope: params.scope,
        supersedes_specific: Some(params.old_uid),
    };
    execute_remember_tool(session, &serde_json::to_value(remember)?, started).await
}

fn runtime_ctx_for_scope(
    session: &SessionMeta,
    scope: &str,
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
    Ok((runtime_fast_ctx(scope_ctx)?, tenant_id, contact_id))
}

fn runtime_ctx_for_contact(
    session: &SessionMeta,
    contact_id: Uuid,
) -> Result<FastPathCtx, FastError> {
    let tenant_id = tenant_uuid(session);
    let scope_ctx = RlsContext::contact(TenantId::from(tenant_id), ContactId(contact_id));
    runtime_fast_ctx(scope_ctx)
}

fn runtime_ctx_for_visible_session_scope(session: &SessionMeta) -> Result<FastPathCtx, FastError> {
    if let Some(contact) = &session.contact {
        runtime_ctx_for_contact(session, contact.contact_id.0)
    } else {
        let (ctx, _, _) = runtime_ctx_for_scope(session, "tenant")?;
        Ok(ctx)
    }
}

fn tenant_uuid(session: &SessionMeta) -> Uuid {
    session.tenant_id.0
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

fn runtime_fast_ctx(scope: RlsContext) -> Result<FastPathCtx, FastError> {
    let runtime = current_runtime()?;
    let pool = runtime.pool().clone();
    let vector: Arc<dyn VectorStore> = Arc::new(PgvectorStore::new(pool.clone(), scope.clone()));
    let graph = Arc::new(
        PostgresGraphStore::scoped(pool.clone(), scope.clone()).with_vector_store(vector.clone()),
    );
    let embedder = Arc::new(
        CohereV4Embedder::new(cohere_api_key(runtime.cohere_api_key())?)
            .map_err(FastError::from)?,
    );
    let pii: Arc<dyn PiiClassifier> = match runtime.pii_service_url() {
        Some(url) => Arc::new(OpenAiPrivacyFilterClassifier::new(url)?),
        None => Arc::new(FailClosedClassifier),
    };
    let contradict = runtime.contradiction_detector();

    Ok(FastPathCtx::new(
        pool, scope, graph, vector, embedder, pii, contradict,
    ))
}

fn cohere_api_key(api_key: &str) -> Result<String, FastError> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(FastError::Invalid(
            "MOA_COHERE_API_KEY is required for fast memory embedding".to_string(),
        ));
    }
    Ok(api_key.to_string())
}

fn parse_node_label(value: Option<&str>) -> Result<NodeLabel, FastError> {
    match value {
        Some(label) => {
            NodeLabel::from_str(label).map_err(|error| FastError::Invalid(error.to_string()))
        }
        None => Ok(NodeLabel::Fact),
    }
}

#[derive(Debug, Clone)]
struct FailClosedClassifier;

#[async_trait]
impl PiiClassifier for FailClosedClassifier {
    async fn classify(&self, _text: &str) -> Result<PiiResult, PiiError> {
        Ok(PiiResult::fail_closed("fast-path-no-pii-service"))
    }
}
