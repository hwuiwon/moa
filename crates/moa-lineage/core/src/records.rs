//! Serializable lineage records emitted by retrieval, context, and generation.

use chrono::{DateTime, Utc};
use moa_core::{MemoryScope, SessionId, UserId, WorkspaceId};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use uuid::Uuid;

use crate::ids::TurnId;

/// One append-only lineage payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case")]
pub enum LineageEvent {
    /// Retrieval fan-in and ranking lineage.
    Retrieval(RetrievalLineage),
    /// Final compiled-context lineage.
    Context(ContextLineage),
    /// LLM request/response lineage.
    Generation(GenerationLineage),
    /// Citation and verifier lineage.
    Citation(CitationLineage),
    /// Evaluation, online judge, or human score lineage.
    Eval(ScoreRecord),
    /// Compliance audit decision lineage.
    Decision(DecisionRecord),
}

/// Numeric lineage record kind stored in TimescaleDB.
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[repr(i16)]
pub enum RecordKind {
    /// Retrieval record.
    Retrieval = 1,
    /// Context record.
    Context = 2,
    /// Generation record.
    Generation = 3,
    /// Citation record.
    Citation = 4,
    /// Evaluation record.
    Eval = 5,
    /// Decision/audit record.
    Decision = 6,
}

impl RecordKind {
    /// Returns the stable database discriminant.
    #[must_use]
    pub const fn as_i16(self) -> i16 {
        self as i16
    }
}

/// Retrieval lineage for one hybrid retrieval operation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetrievalLineage {
    /// Shared agent turn identifier.
    pub turn_id: TurnId,
    /// Session identifier.
    pub session_id: SessionId,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// User identifier.
    pub user_id: UserId,
    /// Memory scope used for retrieval.
    pub scope: MemoryScope,
    /// Event timestamp.
    pub ts: DateTime<Utc>,
    /// Original query string.
    pub query_original: String,
    /// Query expansions produced by the planner.
    pub query_expansions: Vec<String>,
    /// Vector hits observed before fusion.
    pub vector_hits: Vec<VecHit>,
    /// Graph paths observed before fusion.
    pub graph_paths: Vec<GraphPath>,
    /// Fused hit scores.
    pub fusion_scores: Vec<FusedHit>,
    /// Rerank hit scores.
    pub rerank_scores: Vec<RerankHit>,
    /// Final chunk or node IDs that survived into context.
    pub top_k: Vec<Uuid>,
    /// Per-stage timings.
    pub timings: StageTimings,
    /// Backend-specific introspection.
    pub introspection: BackendIntrospection,
    /// Retrieval stage identifier.
    pub stage: RetrievalStage,
}

/// Retrieval stage marker.
#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub enum RetrievalStage {
    /// A single hybrid retrieval operation.
    Single,
    /// One sub-query from a multi-query retrieval plan.
    SubQuery {
        /// Sub-query index.
        idx: usize,
    },
}

/// One vector candidate.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VecHit {
    /// Candidate chunk or node ID.
    pub chunk_id: Uuid,
    /// Backend score.
    pub score: f32,
    /// Vector backend name.
    pub source: String,
    /// Embedder model identifier.
    pub embedder: String,
    /// Embedding dimension.
    pub embed_dim: u16,
}

/// One graph traversal path.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphPath {
    /// Start node ID.
    pub start: Uuid,
    /// End node ID.
    pub end: Uuid,
    /// Edge IDs walked.
    pub edges: Vec<Uuid>,
    /// Edge or node labels.
    pub labels: Vec<String>,
    /// Path length.
    pub length: u8,
    /// Path score.
    pub score: f32,
}

/// One fused retrieval hit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FusedHit {
    /// Candidate chunk or node ID.
    pub chunk_id: Uuid,
    /// Final fused score.
    pub fused_score: f32,
    /// Vector contribution.
    pub vector_contribution: f32,
    /// Graph contribution.
    pub graph_contribution: f32,
    /// Lexical contribution.
    pub lexical_contribution: f32,
    /// Fusion method name.
    pub fusion_method: String,
}

/// One reranked hit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RerankHit {
    /// Candidate chunk or node ID.
    pub chunk_id: Uuid,
    /// Original rank index.
    pub original_index: u16,
    /// Reranker score.
    pub relevance_score: f32,
    /// Reranker model.
    pub rerank_model: String,
}

/// Millisecond timings for retrieval stages.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StageTimings {
    /// Embedding latency.
    pub embed_ms: u32,
    /// Vector search latency.
    pub vector_search_ms: u32,
    /// Graph search latency.
    pub graph_search_ms: u32,
    /// Lexical search latency.
    pub lexical_search_ms: u32,
    /// Fusion latency.
    pub fusion_ms: u32,
    /// Rerank latency.
    pub rerank_ms: u32,
    /// End-to-end latency.
    pub total_ms: u32,
}

/// Optional backend introspection snapshots.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BackendIntrospection {
    /// pgvector details.
    pub pgvector: Option<PgvectorIntrospection>,
    /// Apache AGE details.
    pub age: Option<AgeIntrospection>,
    /// Turbopuffer details.
    pub turbopuffer: Option<TurbopufferIntrospection>,
}

/// pgvector introspection details.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PgvectorIntrospection {
    /// HNSW `ef_search` setting.
    pub ef_search: u32,
    /// Optional iterative scan mode.
    pub iterative_scan: Option<String>,
    /// Shared buffers hit count.
    pub buffers_hit: Option<u64>,
    /// Shared buffers read count.
    pub buffers_read: Option<u64>,
    /// SQL planning latency.
    pub planning_ms: Option<f32>,
    /// SQL execution latency.
    pub execution_ms: Option<f32>,
}

/// AGE introspection details.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgeIntrospection {
    /// Maximum graph path length.
    pub max_path_length: u8,
    /// Number of edges walked.
    pub edges_walked: u32,
    /// Number of paths returned.
    pub paths_returned: u32,
}

/// Turbopuffer introspection details.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurbopufferIntrospection {
    /// Turbopuffer namespace.
    pub namespace: String,
    /// Read consistency mode.
    pub consistency: String,
    /// Optional billed units.
    pub billed_units: Option<f64>,
    /// Client-observed wall-clock latency.
    pub client_wall_clock_ms: u32,
}

/// Final compiled context lineage for one turn.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextLineage {
    /// Shared agent turn identifier.
    pub turn_id: TurnId,
    /// Session identifier.
    pub session_id: SessionId,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// User identifier.
    pub user_id: UserId,
    /// Event timestamp.
    pub ts: DateTime<Utc>,
    /// Chunks placed in the provider window.
    pub chunks_in_window: Vec<ContextChunk>,
    /// Truncation decisions.
    pub truncations: Vec<TruncationEvent>,
    /// Provider prefix-cache read token count when known.
    pub prefix_cache_hit_tokens: Option<u32>,
    /// Provider prefix-cache creation token count when known.
    pub prefix_cache_miss_tokens: Option<u32>,
    /// Estimated total input tokens.
    pub total_input_tokens_estimated: u32,
}

/// One context chunk placed in the provider window.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextChunk {
    /// Context chunk identifier.
    pub chunk_id: Uuid,
    /// Source node or artifact identifier.
    pub source_uid: Uuid,
    /// Position in the compiled context.
    pub position: u16,
    /// Estimated token count.
    pub estimated_tokens: u32,
    /// Context role.
    pub role: String,
}

/// One context truncation event.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TruncationEvent {
    /// Optional dropped chunk ID.
    pub chunk_id: Option<Uuid>,
    /// Truncation reason.
    pub reason: String,
    /// Number of tokens dropped.
    pub tokens_dropped: u32,
}

/// LLM generation lineage for one provider response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GenerationLineage {
    /// Shared agent turn identifier.
    pub turn_id: TurnId,
    /// Session identifier.
    pub session_id: SessionId,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// User identifier.
    pub user_id: UserId,
    /// Event timestamp.
    pub ts: DateTime<Utc>,
    /// Provider name.
    pub provider: String,
    /// Requested model.
    pub request_model: String,
    /// Response model.
    pub response_model: String,
    /// Token usage.
    pub usage: TokenUsage,
    /// Provider finish reasons.
    pub finish_reasons: Vec<String>,
    /// Tool calls requested by the model.
    pub tool_calls: Vec<ToolCallSummary>,
    /// Estimated request cost in micros of USD.
    pub cost_micros: u64,
    /// Provider request duration.
    pub duration: Duration,
    /// OTel trace ID when available.
    pub trace_id: Option<String>,
    /// OTel span ID when available.
    pub span_id: Option<String>,
}

/// Provider token usage normalized for lineage storage.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Input tokens.
    pub input_tokens: u32,
    /// Output tokens.
    pub output_tokens: u32,
    /// Cache-read input tokens.
    pub cache_read_tokens: Option<u32>,
    /// Cache-creation input tokens.
    pub cache_creation_tokens: Option<u32>,
}

/// Summary of one model-requested tool call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallSummary {
    /// Tool name.
    pub tool_name: String,
    /// Provider or MOA tool call ID.
    pub call_id: String,
    /// Serialized argument size.
    pub argument_size_bytes: u32,
    /// Serialized result size when known.
    pub result_size_bytes: u32,
    /// Tool call duration when known.
    pub duration: Duration,
    /// Optional error string.
    pub error: Option<String>,
}

/// Citation lineage for one completed provider answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CitationLineage {
    /// Shared agent turn identifier.
    pub turn_id: TurnId,
    /// Session identifier.
    pub session_id: SessionId,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// User identifier.
    pub user_id: UserId,
    /// Event timestamp.
    pub ts: DateTime<Utc>,
    /// Full answer text that was checked.
    pub answer_text: String,
    /// Byte offsets for each answer sentence.
    pub answer_sentence_offsets: Vec<(u32, u32)>,
    /// Normalized citation records.
    pub citations: Vec<Citation>,
    /// Provider citation source when one was used.
    pub vendor_used: Option<String>,
    /// Verifier pipeline identifier.
    pub verifier_used: Option<String>,
}

/// One normalized citation from a provider or verifier.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Citation {
    /// Sentence index into `CitationLineage::answer_sentence_offsets`.
    pub answer_span: u32,
    /// Optional byte offsets within the cited sentence.
    pub answer_span_bytes: Option<(u32, u32)>,
    /// Source chunk identifier.
    pub source_chunk_id: Uuid,
    /// Source graph node identifier when known.
    pub source_node_uid: Option<Uuid>,
    /// Source text claimed by the model or verifier.
    pub cited_text: Option<String>,
    /// Vendor-supplied citation score when present.
    pub vendor_score: Option<f32>,
    /// Cascade verifier result.
    pub verifier: VerifierResult,
}

/// Citation verifier output for one citation/source pair.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerifierResult {
    /// Whether the citation is considered grounded.
    pub verified: bool,
    /// BM25-like lexical score when available.
    pub bm25_score: Option<f32>,
    /// NLI entailment score when available.
    pub nli_entailment: Option<f32>,
    /// NLI contradiction score when available.
    pub nli_contradiction: Option<f32>,
    /// Verification method used.
    pub method: String,
}

/// A normalized evaluation score for online judges, offline replay, or humans.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScoreRecord {
    /// Stable score identifier.
    pub score_id: Uuid,
    /// Score timestamp.
    pub ts: DateTime<Utc>,
    /// Entity this score evaluates.
    pub target: ScoreTarget,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// Optional user identifier.
    pub user_id: Option<UserId>,
    /// Score name, such as `grounding` or `retrieval_zero_recall`.
    pub name: String,
    /// Score value.
    pub value: ScoreValue,
    /// Score producer.
    pub source: ScoreSource,
    /// Evaluator or model name.
    pub model_or_evaluator: String,
    /// Optional replay run identifier.
    pub run_id: Option<Uuid>,
    /// Optional dataset identifier.
    pub dataset_id: Option<Uuid>,
    /// Optional score comment.
    pub comment: Option<String>,
}

/// Entity targeted by a score record.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScoreTarget {
    /// A single agent turn.
    Turn {
        /// Target turn ID.
        turn_id: TurnId,
    },
    /// A persisted session.
    Session {
        /// Target session ID.
        session_id: SessionId,
    },
    /// One item inside an offline replay run.
    DatasetRunItem {
        /// Replay run ID.
        run_id: Uuid,
        /// Dataset item ID.
        item_id: Uuid,
    },
}

/// Score value variants.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "v", rename_all = "snake_case")]
pub enum ScoreValue {
    /// Numeric score.
    Numeric(f64),
    /// Boolean score.
    Boolean(bool),
    /// Categorical score.
    Categorical(String),
}

/// Score producer category.
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScoreSource {
    /// Emitted live during a turn.
    OnlineJudge,
    /// Emitted by offline replay.
    OfflineReplay,
    /// Emitted by human review tooling.
    Human,
    /// Emitted by an external integration.
    External,
}

/// Compliance audit decision lineage for one policy boundary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionRecord {
    /// Shared agent turn identifier.
    pub turn_id: TurnId,
    /// Session identifier.
    pub session_id: SessionId,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// User identifier.
    pub user_id: UserId,
    /// Event timestamp.
    pub ts: DateTime<Utc>,
    /// Decision kind and redacted payload.
    pub kind: DecisionKind,
    /// Policy or rule-set version that produced the decision.
    pub policy_version: String,
    /// BLAKE3 hash of the canonical decision payload for cross-checking.
    pub integrity_hash: Vec<u8>,
}

/// Compliance-relevant decision categories.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum DecisionKind {
    /// PII redaction was applied before lineage capture.
    PiiRedaction(PiiRedactionDecision),
    /// ACL filtering removed or admitted a candidate.
    AclFilter(AclFilterDecision),
    /// Scope enforcement allowed or rejected a request.
    ScopeEnforcement(ScopeEnforcementDecision),
    /// Privacy export was requested or completed.
    PrivacyExport(PrivacyExportDecision),
    /// Privacy erasure or crypto-shred was requested or completed.
    PrivacyErase(PrivacyEraseDecision),
}

/// Redacted PII redaction decision details.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PiiRedactionDecision {
    /// Pseudonymized subject identifier when known.
    pub subject_pseudonym: Option<String>,
    /// Fields redacted from the source text.
    pub fields: Vec<String>,
    /// Detector or service version.
    pub detector: String,
    /// Whether the source payload was modified.
    pub redacted: bool,
}

/// ACL filtering decision details.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AclFilterDecision {
    /// Resource or candidate identifier.
    pub resource_id: String,
    /// Action evaluated.
    pub action: String,
    /// Whether access was allowed.
    pub allowed: bool,
    /// Rule or policy label that decided the request.
    pub rule: String,
}

/// Scope enforcement decision details.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScopeEnforcementDecision {
    /// Requested scope.
    pub requested_scope: String,
    /// Effective scope after enforcement.
    pub effective_scope: String,
    /// Whether enforcement admitted the request.
    pub allowed: bool,
    /// Reason label.
    pub reason: String,
}

/// Privacy export decision details.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrivacyExportDecision {
    /// Pseudonymized subject identifier.
    pub subject_pseudonym: String,
    /// Export request identifier.
    pub request_id: Uuid,
    /// Export status.
    pub status: String,
}

/// Privacy erase decision details.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PrivacyEraseDecision {
    /// Pseudonymized subject identifier.
    pub subject_pseudonym: String,
    /// Erase request identifier.
    pub request_id: Uuid,
    /// Whether subject key destruction has been scheduled or completed.
    pub key_erased: bool,
}

impl LineageEvent {
    /// Returns the turn ID when this event carries one.
    #[must_use]
    pub fn turn_id(&self) -> Option<TurnId> {
        match self {
            Self::Retrieval(record) => Some(record.turn_id),
            Self::Context(record) => Some(record.turn_id),
            Self::Generation(record) => Some(record.turn_id),
            Self::Citation(record) => Some(record.turn_id),
            Self::Eval(record) => match &record.target {
                ScoreTarget::Turn { turn_id } => Some(*turn_id),
                ScoreTarget::Session { .. } | ScoreTarget::DatasetRunItem { .. } => None,
            },
            Self::Decision(record) => Some(record.turn_id),
        }
    }

    /// Returns the stable numeric record kind.
    #[must_use]
    pub fn record_kind(&self) -> RecordKind {
        match self {
            Self::Retrieval(_) => RecordKind::Retrieval,
            Self::Context(_) => RecordKind::Context,
            Self::Generation(_) => RecordKind::Generation,
            Self::Citation(_) => RecordKind::Citation,
            Self::Eval(_) => RecordKind::Eval,
            Self::Decision(_) => RecordKind::Decision,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{MemoryScope, SessionId, UserId, WorkspaceId};

    use super::*;

    #[test]
    fn lineage_event_serializes_with_kind_and_record() {
        let workspace_id = WorkspaceId::new("workspace");
        let event = LineageEvent::Retrieval(RetrievalLineage {
            turn_id: TurnId::new_v7(),
            session_id: SessionId::new(),
            workspace_id: workspace_id.clone(),
            user_id: UserId::new("user"),
            scope: MemoryScope::Workspace { workspace_id },
            ts: Utc::now(),
            query_original: "query".to_string(),
            query_expansions: Vec::new(),
            vector_hits: Vec::new(),
            graph_paths: Vec::new(),
            fusion_scores: Vec::new(),
            rerank_scores: Vec::new(),
            top_k: Vec::new(),
            timings: StageTimings::default(),
            introspection: BackendIntrospection::default(),
            stage: RetrievalStage::Single,
        });

        let value = serde_json::to_value(event).expect("serialize lineage event");

        assert_eq!(value["kind"], "retrieval");
        assert_eq!(value["record"]["query_original"], "query");
    }

    #[test]
    fn score_event_serializes_with_typed_target_and_value() {
        let turn_id = TurnId::new_v7();
        let event = LineageEvent::Eval(ScoreRecord {
            score_id: Uuid::now_v7(),
            ts: Utc::now(),
            target: ScoreTarget::Turn { turn_id },
            workspace_id: WorkspaceId::new("workspace"),
            user_id: Some(UserId::new("user")),
            name: "citation_verified".to_string(),
            value: ScoreValue::Boolean(true),
            source: ScoreSource::OnlineJudge,
            model_or_evaluator: "cascade-bm25-hhem".to_string(),
            run_id: None,
            dataset_id: None,
            comment: None,
        });

        let value = serde_json::to_value(event).expect("serialize score event");

        assert_eq!(value["kind"], "eval");
        assert_eq!(value["record"]["target"]["kind"], "turn");
        assert_eq!(value["record"]["value"]["type"], "boolean");
        assert_eq!(value["record"]["source"], "online_judge");
    }
}
