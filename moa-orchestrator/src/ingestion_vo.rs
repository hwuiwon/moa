//! Restate virtual object for slow-path graph-memory ingestion.

use std::env;
use std::sync::Arc;
use std::time::Duration;

use moa_core::{ScopeContext, ScopedConn};
use moa_memory_graph::{
    AgeGraphStore, GraphStore, NodeLabel, NodeWriteIntent, PiiClass as GraphPiiClass,
};
use moa_memory_ingest::{
    ClassifiedFact, EmbeddedFact, ExtractedFact, IngestApplyReport, IngestDecision, SessionTurn,
    chunk_turn, extract_facts, fact_hash, scoped_fact_uid,
};
use moa_memory_pii::{
    OpenAiPrivacyFilterClassifier, PiiCategory, PiiClass as ClassifierPiiClass, PiiClassifier,
    PiiResult, PiiSpan,
};
use moa_memory_vector::{CohereV4Embedder, Embedder, PgvectorStore};
use restate_sdk::prelude::*;
use secrecy::SecretString;
use serde_json::json;
use sqlx::PgPool;

use crate::OrchestratorCtx;
use crate::observability::annotate_restate_handler_span;

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
        annotate_restate_handler_span("IngestionVO", "ingest_turn");
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

        let degraded = workspace_degraded(&turn).await?;
        if degraded && !moa_memory_ingest::should_ingest_degraded(&turn) {
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
        let extracted = ctx
            .run(|| async move { Ok(Json::from(extract_facts(&extract_chunks))) })
            .name("extract")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let classify_facts_input = extracted.clone();
        let classified = ctx
            .run(|| async move { classify_facts(&classify_facts_input).await.map(Json::from) })
            .name("classify_pii")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let embed_input = classified.clone();
        let embedded = ctx
            .run(|| async move { embed_batch(&embed_input).await.map(Json::from) })
            .name("embed")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let contradiction_input = embedded.clone();
        let decisions = ctx
            .run(|| async move {
                detect_contradictions(&contradiction_input)
                    .await
                    .map(Json::from)
            })
            .name("contradict")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let upsert_turn = turn.clone();
        let report = ctx
            .run(|| async move {
                apply_decisions(&upsert_turn, &decisions)
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

async fn classify_facts(facts: &[ExtractedFact]) -> Result<Vec<ClassifiedFact>, HandlerError> {
    let classifier = classifier_from_env()?;
    let mut classified = Vec::with_capacity(facts.len());
    for fact in facts {
        let result = classify_fact(&classifier, &fact.summary).await?;
        classified.push(ClassifiedFact {
            fact: fact.clone(),
            pii_class: result.class,
            pii_spans: result.spans,
        });
    }
    Ok(classified)
}

async fn classify_fact(
    classifier: &ClassifierBackend,
    text: &str,
) -> Result<PiiResult, HandlerError> {
    match classifier {
        ClassifierBackend::Sidecar(classifier) => {
            classifier.classify(text).await.map_err(HandlerError::from)
        }
        ClassifierBackend::Heuristic => Ok(heuristic_classify(text)),
    }
}

fn classifier_from_env() -> Result<ClassifierBackend, HandlerError> {
    if let Ok(url) = env::var("MOA_PII_URL").or_else(|_| env::var("MOA_PII_SERVICE_URL")) {
        return Ok(ClassifierBackend::Sidecar(
            OpenAiPrivacyFilterClassifier::new(url).map_err(HandlerError::from)?,
        ));
    }
    Ok(ClassifierBackend::Heuristic)
}

enum ClassifierBackend {
    Sidecar(OpenAiPrivacyFilterClassifier),
    Heuristic,
}

fn heuristic_classify(text: &str) -> PiiResult {
    let mut spans = Vec::new();
    for token in text.split_whitespace() {
        if token.contains('@') {
            push_span(text, token, PiiCategory::Email, 0.80, &mut spans);
        } else if token.contains("sk-") || token.to_ascii_lowercase().contains("secret") {
            push_span(text, token, PiiCategory::Secret, 0.80, &mut spans);
        } else if looks_like_ssn(token) {
            push_span(text, token, PiiCategory::Ssn, 0.90, &mut spans);
        }
    }
    let class = if spans
        .iter()
        .any(|span| matches!(span.category, PiiCategory::Secret))
    {
        ClassifierPiiClass::Restricted
    } else if spans
        .iter()
        .any(|span| matches!(span.category, PiiCategory::Ssn))
    {
        ClassifierPiiClass::Phi
    } else if spans.is_empty() {
        ClassifierPiiClass::None
    } else {
        ClassifierPiiClass::Pii
    };
    PiiResult {
        class,
        spans,
        model_version: "moa-heuristic:v1".to_string(),
        abstained: false,
    }
}

fn push_span(
    text: &str,
    token: &str,
    category: PiiCategory,
    confidence: f32,
    spans: &mut Vec<PiiSpan>,
) {
    if let Some(start) = text.find(token) {
        spans.push(PiiSpan {
            start,
            end: start + token.len(),
            category,
            confidence,
        });
    }
}

fn looks_like_ssn(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() == 11
        && bytes[3] == b'-'
        && bytes[6] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 3 || index == 6 || byte.is_ascii_digit())
}

async fn embed_batch(facts: &[ClassifiedFact]) -> Result<Vec<EmbeddedFact>, HandlerError> {
    let Some(api_key) = env::var("COHERE_API_KEY")
        .ok()
        .or_else(|| env::var("MOA_COHERE_API_KEY").ok())
    else {
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

async fn detect_contradictions(
    embedded: &[EmbeddedFact],
) -> Result<Vec<IngestDecision>, HandlerError> {
    Ok(embedded
        .iter()
        .cloned()
        .map(|fact| IngestDecision::Insert { fact })
        .collect())
}

async fn apply_decisions(
    turn: &SessionTurn,
    decisions: &[IngestDecision],
) -> Result<IngestApplyReport, HandlerError> {
    let runtime = OrchestratorCtx::current();
    let pool = runtime.session_store.pool().clone();
    let scope = ScopeContext::workspace(turn.workspace_id.clone());
    let mut report = IngestApplyReport::default();

    for decision in decisions {
        match apply_one_decision(&pool, &scope, turn, decision).await {
            Ok(ApplyOutcome::Inserted) => report.inserted += 1,
            Ok(ApplyOutcome::Superseded) => report.superseded += 1,
            Ok(ApplyOutcome::Skipped) => report.skipped += 1,
            Err(error) => {
                report.failed += 1;
                let error_message = format!("{error:?}");
                write_dlq(&pool, &scope, turn, decision, &error_message).await?;
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

    let graph = graph_store(pool.clone(), scope.clone(), fact);
    let fact_uid = scoped_fact_uid(&turn.workspace_id, &turn.session_id, turn.turn_seq, &hash);
    match decision {
        IngestDecision::Insert { fact } => {
            let uid = graph
                .create_node(node_intent(turn, fact, &hash, fact_uid))
                .await
                .map_err(HandlerError::from)?;
            insert_dedup(pool, scope, turn, &hash, uid).await?;
            Ok(ApplyOutcome::Inserted)
        }
        IngestDecision::Supersede { old_uid, fact } => {
            let uid = graph
                .supersede_node(*old_uid, node_intent(turn, fact, &hash, fact_uid))
                .await
                .map_err(HandlerError::from)?;
            insert_dedup(pool, scope, turn, &hash, uid).await?;
            Ok(ApplyOutcome::Superseded)
        }
        IngestDecision::SkipDuplicate { .. } => Ok(ApplyOutcome::Skipped),
    }
}

fn graph_store(pool: PgPool, scope: ScopeContext, fact: &EmbeddedFact) -> AgeGraphStore {
    let store = AgeGraphStore::scoped(pool.clone(), scope.clone());
    if fact.embedding.is_some() {
        store.with_vector_store(Arc::new(PgvectorStore::new(pool, scope)))
    } else {
        store
    }
}

fn node_intent(
    turn: &SessionTurn,
    fact: &EmbeddedFact,
    hash: &[u8],
    fact_uid: uuid::Uuid,
) -> NodeWriteIntent {
    let extracted = &fact.classified.fact;
    NodeWriteIntent {
        uid: fact_uid,
        label: NodeLabel::Fact,
        workspace_id: Some(turn.workspace_id.to_string()),
        user_id: None,
        scope: "workspace".to_string(),
        name: extracted.subject.clone(),
        properties: json!({
            "uid": fact_uid.to_string(),
            "extracted_uid": extracted.uid.to_string(),
            "workspace_id": turn.workspace_id.to_string(),
            "scope": "workspace",
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
        pii_class: graph_pii_class(fact.classified.pii_class),
        confidence: Some(0.70),
        valid_from: turn.finalized_at,
        embedding: fact.embedding.clone(),
        embedding_model: fact.embedding_model.clone(),
        embedding_model_version: fact.embedding_model_version,
        actor_id: turn.user_id.to_string(),
        actor_kind: "user".to_string(),
    }
}

async fn workspace_degraded(turn: &SessionTurn) -> Result<bool, HandlerError> {
    let runtime = OrchestratorCtx::current();
    let scope = ScopeContext::workspace(turn.workspace_id.clone());
    let mut conn = ScopedConn::begin(runtime.session_store.pool(), &scope)
        .await
        .map_err(HandlerError::from)?;
    let degraded = sqlx::query_scalar::<_, bool>(
        "SELECT slow_path_degraded FROM moa.workspace_state WHERE workspace_id = $1",
    )
    .bind(turn.workspace_id.to_string())
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
    let uid = sqlx::query_scalar::<_, uuid::Uuid>(
        r#"
        SELECT fact_uid
        FROM moa.ingest_dedup
        WHERE workspace_id = $1
          AND session_id = $2
          AND turn_seq = $3
          AND fact_hash = $4
        "#,
    )
    .bind(turn.workspace_id.to_string())
    .bind(turn.session_id.0)
    .bind(turn_seq)
    .bind(hash)
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
    sqlx::query(
        r#"
        INSERT INTO moa.ingest_dedup
            (workspace_id, session_id, turn_seq, fact_hash, fact_uid)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (workspace_id, session_id, turn_seq, fact_hash) DO NOTHING
        "#,
    )
    .bind(turn.workspace_id.to_string())
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
    sqlx::query(
        r#"
        INSERT INTO moa.ingest_dlq
            (workspace_id, session_id, turn_seq, payload, error, next_retry_at)
        VALUES ($1, $2, $3, $4, $5, now() + INTERVAL '5 minutes')
        "#,
    )
    .bind(turn.workspace_id.to_string())
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

fn graph_pii_class(class: ClassifierPiiClass) -> GraphPiiClass {
    match class {
        ClassifierPiiClass::None => GraphPiiClass::None,
        ClassifierPiiClass::Pii => GraphPiiClass::Pii,
        ClassifierPiiClass::Phi => GraphPiiClass::Phi,
        ClassifierPiiClass::Restricted => GraphPiiClass::Restricted,
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
    use moa_core::ContextMessage;
    use moa_memory_pii::{PiiCategory, PiiClass};

    use super::{heuristic_classify, turn_transcript};

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
    fn heuristic_classifier_marks_secrets_restricted() {
        let result = heuristic_classify("The API secret is sk-test-123.");

        assert_eq!(result.class, PiiClass::Restricted);
        assert!(
            result
                .spans
                .iter()
                .any(|span| matches!(span.category, PiiCategory::Secret))
        );
    }
}
