//! Deterministic DTOs and extract/chunk helpers for graph-memory ingestion.

use chrono::{DateTime, Utc};
use moa_core::{SessionId, UserId, WorkspaceId};
use moa_memory_graph::PiiClass;
use moa_memory_pii::PiiSpan;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{IngestError, Result};
/// Maximum chunk length accepted by the checked deterministic extractor.
pub const MAX_EXTRACT_CHUNK_CHARS: usize = 32_768;

/// Finalized session turn payload sent to the slow-path ingestion VO.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTurn {
    /// Workspace that owns the session.
    pub workspace_id: WorkspaceId,
    /// User that produced the session turn.
    pub user_id: UserId,
    /// Session identifier.
    pub session_id: SessionId,
    /// Durable turn sequence, normally the persisted `BrainResponse` event sequence number.
    pub turn_seq: u64,
    /// Transcript text to extract graph facts from.
    pub transcript: String,
    /// Best-known dominant PII class before extraction.
    pub dominant_pii_class: String,
    /// Timestamp at which the turn was finalized.
    pub finalized_at: DateTime<Utc>,
}

/// A transcript chunk processed by extraction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnChunk {
    /// Zero-based chunk index inside the turn.
    pub index: usize,
    /// Chunk text.
    pub text: String,
    /// Approximate token count used for routing and tests.
    pub token_estimate: usize,
}

/// Scope hint emitted by extraction for one fact candidate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractedFactScopeHint {
    /// The fact belongs to the workspace-bound user who produced the turn.
    #[default]
    User,
    /// The fact is intentionally shared with the whole workspace.
    Workspace,
}

/// One fact candidate emitted by extraction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractedFact {
    /// Stable fact identifier derived from the fact hash.
    pub uid: Uuid,
    /// Subject text.
    pub subject: String,
    /// Predicate text.
    pub predicate: String,
    /// Object text.
    pub object: String,
    /// Concise fact summary.
    pub summary: String,
    /// Source chunk index.
    pub source_chunk: usize,
    /// Scope hint used by slow-path ingestion when writing graph rows.
    #[serde(default)]
    pub scope_hint: ExtractedFactScopeHint,
    /// Optional model-provided confidence for this extracted fact.
    #[serde(default)]
    pub confidence: Option<f64>,
}

/// A fact after PII classification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifiedFact {
    /// Extracted fact payload.
    pub fact: ExtractedFact,
    /// Aggregate PII class for the fact summary.
    pub pii_class: PiiClass,
    /// PII spans returned by the classifier.
    pub pii_spans: Vec<PiiSpan>,
}

/// A classified fact after embedding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddedFact {
    /// Classified fact payload.
    pub classified: ClassifiedFact,
    /// Optional embedding vector. Missing when no embedder is configured.
    pub embedding: Option<Vec<f32>>,
    /// Optional embedding model name.
    pub embedding_model: Option<String>,
    /// Optional embedding model version.
    pub embedding_model_version: Option<i32>,
}

/// Contradiction decision made before writing a fact to the graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IngestDecision {
    /// Insert the fact as a new graph node.
    Insert {
        /// Fact to insert.
        fact: EmbeddedFact,
    },
    /// Supersede an existing fact node with this replacement.
    Supersede {
        /// Existing node uid to close.
        old_uid: Uuid,
        /// Replacement fact.
        fact: EmbeddedFact,
    },
    /// Skip because the fact is already represented.
    SkipDuplicate {
        /// Fact uid that was considered duplicate.
        fact_uid: Uuid,
    },
}

/// Summary returned after applying one turn's decisions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestApplyReport {
    /// Number of facts inserted as new nodes.
    pub inserted: usize,
    /// Number of facts that superseded an older node.
    pub superseded: usize,
    /// Number of facts skipped by idempotency or duplicate checks.
    pub skipped: usize,
    /// Number of facts written to the dead-letter queue.
    pub failed: usize,
}

/// Chunks a finalized turn transcript without splitting fenced code blocks.
pub fn chunk_turn(
    turn: &SessionTurn,
    target_tokens: usize,
    overlap_tokens: usize,
) -> Result<Vec<TurnChunk>> {
    crate::chunking::chunk_turn(turn, target_tokens, overlap_tokens)
}

/// Extracts deterministic fact candidates from chunks.
///
/// This is the local deterministic scaffold for M10. Production LLM extraction can replace this
/// helper behind the same DTOs without changing the Restate journal shape.
#[must_use]
pub fn extract_facts(chunks: &[TurnChunk]) -> Vec<ExtractedFact> {
    chunks
        .iter()
        .flat_map(|chunk| {
            candidate_fact_summaries(&chunk.text)
                .into_iter()
                .map(move |summary| extracted_fact_from_summary(chunk.index, summary))
        })
        .collect()
}

/// Extracts deterministic fact candidates after validating chunk size.
///
/// The local extractor keeps size validation separate from [`extract_facts`] so existing Restate
/// journals that call the original helper stay byte-compatible while tests and future production
/// callers can fail with a typed error before oversized chunks reach model-backed extractors.
pub fn extract_facts_checked(chunks: &[TurnChunk]) -> Result<Vec<ExtractedFact>> {
    for chunk in chunks {
        let actual_chars = chunk.text.chars().count();
        if actual_chars > MAX_EXTRACT_CHUNK_CHARS {
            return Err(IngestError::ChunkTooLarge {
                index: chunk.index,
                actual_chars,
                max_chars: MAX_EXTRACT_CHUNK_CHARS,
            });
        }
    }
    Ok(extract_facts(chunks))
}

/// Returns a deterministic confidence hint for one extracted fact summary.
#[must_use]
pub fn extraction_confidence_hint(summary: &str) -> f64 {
    let lower = summary.to_ascii_lowercase();
    if ["probably", "maybe", "might", "likely", "appears to"]
        .into_iter()
        .any(|marker| lower.contains(marker))
    {
        0.45
    } else {
        0.70
    }
}

/// Returns the canonical fact hash bytes used by `moa.ingest_dedup`.
pub fn fact_hash(fact: &ExtractedFact) -> Result<Vec<u8>> {
    Ok(fact_hash_parts(
        &fact.subject,
        &fact.predicate,
        &fact.object,
        &fact.summary,
    ))
}

/// Returns a stable UUID derived from a fact hash.
#[must_use]
pub fn fact_uid_from_hash(hash: &[u8]) -> Uuid {
    let mut bytes = [0_u8; 16];
    let copy_len = bytes.len().min(hash.len());
    bytes[..copy_len].copy_from_slice(&hash[..copy_len]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// Returns the deterministic graph node UUID for one fact in one finalized turn.
#[must_use]
pub fn scoped_fact_uid(
    workspace_id: &WorkspaceId,
    session_id: &SessionId,
    turn_seq: u64,
    fact_hash: &[u8],
) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(workspace_id.to_string().as_bytes());
    hasher.update([0]);
    hasher.update(session_id.0.as_bytes());
    hasher.update(turn_seq.to_be_bytes());
    hasher.update(fact_hash);
    fact_uid_from_hash(&hasher.finalize())
}

/// Returns whether a degraded workspace should ingest this turn.
#[must_use]
pub fn should_ingest_degraded(turn: &SessionTurn) -> bool {
    if turn.dominant_pii_class != "none" {
        return true;
    }
    let mut hasher = Sha256::new();
    hasher.update(turn.workspace_id.to_string().as_bytes());
    hasher.update(turn.session_id.0.as_bytes());
    hasher.update(turn.turn_seq.to_be_bytes());
    let digest = hasher.finalize();
    digest[0] < 128
}

fn candidate_fact_summaries(text: &str) -> Vec<String> {
    let explicit = text
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            line.strip_prefix("Fact:")
                .or_else(|| line.strip_prefix("- Fact:"))
                .or_else(|| line.strip_prefix("* Fact:"))
                .map(str::trim)
                .filter(|summary| !summary.is_empty())
                .map(ToOwned::to_owned)
        })
        .collect::<Vec<_>>();
    if !explicit.is_empty() {
        return explicit;
    }

    text.split(['.', '!', '?'])
        .map(str::trim)
        .filter(|sentence| !is_non_declarative(sentence))
        .filter(|sentence| sentence.split_whitespace().count() >= 4)
        .map(ToOwned::to_owned)
        .collect()
}

fn is_non_declarative(sentence: &str) -> bool {
    let lower = sentence.trim().to_ascii_lowercase();
    [
        "should ", "could ", "would ", "can ", "please ", "review ", "do ",
    ]
    .into_iter()
    .any(|prefix| lower.starts_with(prefix))
}

fn extracted_fact_from_summary(source_chunk: usize, summary: String) -> ExtractedFact {
    let (scope_hint, summary) = scope_hint_from_summary(&summary);
    let (subject, predicate, object) = split_summary(&summary);
    let hash = fact_hash_parts(&subject, &predicate, &object, &summary);
    ExtractedFact {
        uid: fact_uid_from_hash(&hash),
        subject,
        predicate,
        object,
        summary,
        source_chunk,
        scope_hint,
        confidence: None,
    }
}

fn scope_hint_from_summary(summary: &str) -> (ExtractedFactScopeHint, String) {
    let scope_hint = if contains_scope_marker(summary, "workspace shared") {
        ExtractedFactScopeHint::Workspace
    } else {
        ExtractedFactScopeHint::User
    };
    let summary = strip_scope_marker(summary, "workspace shared");
    let summary = strip_scope_marker(&summary, "user private");
    (scope_hint, normalize_stripped_scope_summary(&summary))
}

fn contains_scope_marker(summary: &str, marker: &str) -> bool {
    summary.to_ascii_lowercase().contains(marker)
}

fn strip_scope_marker(summary: &str, marker: &str) -> String {
    let mut stripped = summary.to_string();
    loop {
        let lower = stripped.to_ascii_lowercase();
        let Some(start) = lower.find(marker) else {
            break;
        };
        stripped.replace_range(start..start + marker.len(), " ");
    }
    stripped
}

fn normalize_stripped_scope_summary(summary: &str) -> String {
    summary
        .trim_matches(|ch: char| {
            ch.is_whitespace() || matches!(ch, ':' | '-' | ',' | ';' | '[' | ']' | '(' | ')')
        })
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn fact_hash_parts(subject: &str, predicate: &str, object: &str, summary: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(subject.as_bytes());
    hasher.update([0]);
    hasher.update(predicate.as_bytes());
    hasher.update([0]);
    hasher.update(object.as_bytes());
    hasher.update([0]);
    hasher.update(summary.as_bytes());
    hasher.finalize().to_vec()
}

fn split_summary(summary: &str) -> (String, String, String) {
    let words = summary.split_whitespace().collect::<Vec<_>>();
    match words.as_slice() {
        [] => ("fact".to_string(), "states".to_string(), String::new()),
        [only] => ((*only).to_string(), "states".to_string(), String::new()),
        [subject, predicate, rest @ ..] => (
            (*subject).trim_matches(':').to_string(),
            (*predicate).trim_matches(':').to_string(),
            object_words(rest).join(" "),
        ),
    }
}

fn object_words<'a>(words: &'a [&'a str]) -> &'a [&'a str] {
    if let [first, rest @ ..] = words
        && first.eq_ignore_ascii_case("is")
    {
        rest
    } else {
        words
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{SessionId, UserId, WorkspaceId};

    use super::*;

    fn turn(transcript: &str) -> SessionTurn {
        SessionTurn {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            session_id: SessionId::new(),
            turn_seq: 7,
            transcript: transcript.to_string(),
            dominant_pii_class: "none".to_string(),
            finalized_at: Utc::now(),
        }
    }

    #[test]
    fn extracts_explicit_fact_lines() {
        let chunks = chunk_turn(
            &turn("Fact: auth service uses JWT\nFact: API owns billing"),
            700,
            100,
        )
        .expect("chunk explicit facts");
        let facts = extract_facts(&chunks);
        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].summary, "auth service uses JWT");
    }

    #[test]
    fn extraction_defaults_unmarked_facts_to_user_scope() {
        let chunks =
            chunk_turn(&turn("Fact: auth service uses JWT"), 700, 100).expect("chunk one fact");
        let facts = extract_facts(&chunks);

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].scope_hint, ExtractedFactScopeHint::User);
    }

    #[test]
    fn extraction_strips_scope_markers_before_parsing() {
        let chunks = chunk_turn(
            &turn("Fact: workspace shared API runs_on_port 3000\nFact: user private theme uses dark_mode"),
            700,
            100,
        )
        .expect("chunk marked facts");
        let facts = extract_facts(&chunks);

        assert_eq!(facts.len(), 2);
        assert_eq!(facts[0].scope_hint, ExtractedFactScopeHint::Workspace);
        assert_eq!(facts[0].summary, "API runs_on_port 3000");
        assert_eq!(facts[0].subject, "API");
        assert_eq!(facts[1].scope_hint, ExtractedFactScopeHint::User);
        assert_eq!(facts[1].summary, "theme uses dark_mode");
        assert_eq!(facts[1].subject, "theme");
    }

    #[test]
    fn fact_uid_is_stable_for_same_fact() {
        let chunks =
            chunk_turn(&turn("Fact: auth service uses JWT"), 700, 100).expect("chunk one fact");
        let facts = extract_facts(&chunks);
        let hash = fact_hash(&facts[0]).expect("hash fact");
        assert_eq!(facts[0].uid, fact_uid_from_hash(&hash));
    }

    #[test]
    fn extracted_fact_deserializes_without_confidence_field() {
        // Pins: old Restate journal entries without model confidence keep deserializing.
        let json = serde_json::json!({
            "uid": uuid::Uuid::now_v7(),
            "subject": "auth",
            "predicate": "uses",
            "object": "JWT",
            "summary": "auth uses JWT",
            "source_chunk": 0,
            "scope_hint": "workspace"
        });

        let fact: ExtractedFact =
            serde_json::from_value(json).expect("old extracted fact JSON should deserialize");

        assert_eq!(fact.confidence, None);
        assert_eq!(fact.scope_hint, ExtractedFactScopeHint::Workspace);
    }

    #[test]
    fn scoped_fact_uid_differs_by_workspace() {
        let chunks =
            chunk_turn(&turn("Fact: auth service uses JWT"), 700, 100).expect("chunk one fact");
        let facts = extract_facts(&chunks);
        let hash = fact_hash(&facts[0]).expect("hash fact");
        let session_id = SessionId::new();

        let first = scoped_fact_uid(&WorkspaceId::new("workspace-a"), &session_id, 7, &hash);
        let second = scoped_fact_uid(&WorkspaceId::new("workspace-b"), &session_id, 7, &hash);

        assert_ne!(first, second);
    }

    #[test]
    fn degraded_sampling_is_deterministic() {
        let turn = turn("Fact: auth service uses JWT");
        let first = should_ingest_degraded(&turn);
        let second = should_ingest_degraded(&turn);
        assert_eq!(first, second);
    }
}
