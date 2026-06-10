//! Gold-node resolution for memory evaluation corpora.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use moa_core::ScopeContext;
use moa_memory_graph::{AgeGraphStore, NodeIndexRow, NodeLabel, PiiClass};
use moa_memory_ingest::{
    IngestApplyReport, IngestCtx, SessionTurn, chunk_turn, extract_facts, fact_hash,
    ingest_turn_direct_with_ctx,
};
use moa_memory_vector::PgvectorStore;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

use super::{LedgerFact, SyntheticSession, SyntheticTurn, validate_ledger, validate_sessions};
use crate::{EvalError, Result};

const CHUNK_TARGET_TOKENS: usize = 700;
const CHUNK_OVERLAP_TOKENS: usize = 100;

/// Full result from ingesting a corpus and resolving ledger facts to graph nodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldResolutionReport {
    /// Per-turn ingestion summaries, in the deterministic order the turns were ingested.
    pub ingest_reports: Vec<GoldIngestTurnReport>,
    /// Per-ledger-fact resolution records suitable for writing to `gold_nodes.jsonl`.
    pub records: Vec<GoldNodeRecord>,
}

impl GoldResolutionReport {
    /// Returns the fraction of ledger facts that resolved to at least one stored graph node.
    #[must_use]
    pub fn ingestion_coverage(&self) -> f64 {
        if self.records.is_empty() {
            return 0.0;
        }
        let resolved = self
            .records
            .iter()
            .filter(|record| record.resolution_status != GoldResolutionStatus::Unresolved)
            .count();
        resolved as f64 / self.records.len() as f64
    }

    /// Returns unresolved ledger fact identifiers in output order.
    #[must_use]
    pub fn unresolved_facts(&self) -> Vec<&str> {
        self.records
            .iter()
            .filter(|record| record.resolution_status == GoldResolutionStatus::Unresolved)
            .map(|record| record.fact_id.as_str())
            .collect()
    }

    /// Returns records that matched more than one graph node.
    #[must_use]
    pub fn duplicate_resolutions(&self) -> Vec<&GoldNodeRecord> {
        self.records
            .iter()
            .filter(|record| record.resolution_status == GoldResolutionStatus::Duplicate)
            .collect()
    }
}

/// Ingestion summary for one synthetic turn processed during gold resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldIngestTurnReport {
    /// Synthetic session identifier.
    pub session_id: String,
    /// Synthetic turn sequence.
    pub turn_seq: u64,
    /// Ledger fact ids planted by this turn.
    pub fact_ids: Vec<String>,
    /// Number of facts inserted by slow-path ingestion.
    pub inserted: usize,
    /// Number of facts superseded by slow-path ingestion.
    pub superseded: usize,
    /// Number of facts skipped by slow-path ingestion.
    pub skipped: usize,
    /// Number of facts that failed and were dead-lettered.
    pub failed: usize,
}

impl GoldIngestTurnReport {
    fn from_report(turn: &SessionTurn, fact_ids: &[String], report: IngestApplyReport) -> Self {
        Self {
            session_id: turn.session_id.to_string(),
            turn_seq: turn.turn_seq,
            fact_ids: sorted_strings(fact_ids),
            inserted: report.inserted,
            superseded: report.superseded,
            skipped: report.skipped,
            failed: report.failed,
        }
    }
}

/// One `gold_nodes.jsonl` record mapping a ledger fact to stored node rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldNodeRecord {
    /// Stable synthetic fact identifier.
    pub fact_id: String,
    /// Resolved `moa.node_index.uid` values, sorted for byte-stable output.
    pub node_uids: Vec<Uuid>,
    /// Actual stored `moa.node_index.scope` for the resolved node, or `mixed` for scope drift.
    pub scope: Option<String>,
    /// Whether any resolved node is currently active.
    pub active: bool,
    /// Actual stored `valid_from` for the first resolved node in deterministic order.
    pub valid_from: Option<DateTime<Utc>>,
    /// Actual stored `valid_to` for the first resolved node in deterministic order.
    pub valid_to: Option<DateTime<Utc>>,
    /// Resolution outcome for this ledger fact.
    pub resolution_status: GoldResolutionStatus,
    /// Expected ledger scope, retained beside the actual stored scope for diagnostics.
    pub expected_scope: String,
    /// Expected ledger validity start.
    pub expected_valid_from: DateTime<Utc>,
    /// Expected ledger validity end.
    pub expected_valid_to: Option<DateTime<Utc>>,
    /// PII redaction status observed on the resolved stored node content.
    pub pii_status: GoldPiiStatus,
    /// Stored PII classes for resolved nodes, sorted and deduplicated.
    pub stored_pii_classes: Vec<String>,
    /// Ledger facts this fact supersedes.
    pub supersedes: Vec<String>,
    /// Ledger facts that supersede this fact.
    pub superseded_by: Vec<String>,
    /// Ordered ledger supersession chain touching this fact.
    pub supersession_chain: Vec<String>,
    /// Per-node details for duplicate and scope-drift diagnosis.
    pub nodes: Vec<GoldNodeSnapshot>,
}

/// Resolution outcome for a ledger fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoldResolutionStatus {
    /// Exactly one graph node resolved.
    Resolved,
    /// No graph node resolved.
    Unresolved,
    /// More than one graph node resolved.
    Duplicate,
}

/// PII redaction outcome for a resolved ledger fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoldPiiStatus {
    /// The ledger fact did not expect redaction.
    NotExpected,
    /// The fact expected redaction but did not resolve to a stored node.
    NotResolved,
    /// Resolved stored content appears redacted.
    Redacted,
    /// Resolved stored content still contains the sensitive ledger object.
    Unredacted,
    /// Duplicate resolutions disagree on redaction state.
    Mixed,
}

/// Per-node stored metadata captured in each gold-node record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GoldNodeSnapshot {
    /// Stored `moa.node_index.uid`.
    pub uid: Uuid,
    /// Stored `moa.node_index.scope`.
    pub scope: String,
    /// Whether this node is active.
    pub active: bool,
    /// Stored validity start.
    pub valid_from: DateTime<Utc>,
    /// Stored validity end.
    pub valid_to: Option<DateTime<Utc>>,
    /// Stored PII class.
    pub pii_class: String,
}

/// Ingests synthetic sessions and resolves every ledger fact to stored graph nodes.
pub async fn resolve_gold_nodes(
    ctx: IngestCtx,
    ledger: &[LedgerFact],
    sessions: &[SyntheticSession],
) -> Result<GoldResolutionReport> {
    validate_ledger(ledger)?;
    validate_sessions(sessions)?;
    let facts_by_id = facts_by_id(ledger)?;
    let sources = sources_by_fact_id(&facts_by_id, sessions)?;
    let turn_order = deterministic_turn_order(&facts_by_id, sessions)?;

    let mut ingest_reports = Vec::with_capacity(turn_order.len());
    for turn_source in &turn_order {
        let turn = session_turn(turn_source, &facts_by_id)?;
        let turn_ctx = ingest_ctx_for_turn(&ctx, &turn);
        let report = ingest_turn_direct_with_ctx(turn_ctx, turn.clone())
            .await
            .map_err(|error| {
                EvalError::InvalidConfig(format!(
                    "gold ingestion failed for session {} turn {}: {error:?}",
                    turn.session_id, turn.turn_seq
                ))
            })?;
        ingest_reports.push(GoldIngestTurnReport::from_report(
            &turn,
            &turn_source.turn.fact_ids,
            report,
        ));
    }

    let superseded_by = superseded_by(ledger);
    let mut records = Vec::with_capacity(ledger.len());
    for fact in sorted_facts(ledger) {
        let source = sources.get(&fact.fact_id).ok_or_else(|| {
            EvalError::InvalidConfig(format!(
                "ledger fact {} has no matching synthetic source turn",
                fact.fact_id
            ))
        })?;
        let nodes = resolve_fact_nodes(&ctx, fact, source).await?;
        records.push(record_for_fact(fact, &nodes, &superseded_by));
    }

    Ok(GoldResolutionReport {
        ingest_reports,
        records,
    })
}

/// Writes `gold_nodes.jsonl` from a resolved gold report.
pub async fn write_gold_nodes_jsonl(path: &Path, records: &[GoldNodeRecord]) -> Result<()> {
    write_jsonl(path, records).await
}

/// Reads `gold_nodes.jsonl` records.
pub async fn read_gold_nodes_jsonl(path: &Path) -> Result<Vec<GoldNodeRecord>> {
    read_jsonl(path).await
}

async fn resolve_fact_nodes(
    ctx: &IngestCtx,
    fact: &LedgerFact,
    source: &FactSource<'_>,
) -> Result<Vec<NodeIndexRow>> {
    let source_candidates = fetch_source_candidates(ctx, fact).await?;
    let expected_hashes = expected_fact_hashes(fact, source)?;
    let hash_matches = match_by_hash(&source_candidates, &expected_hashes);
    if !hash_matches.is_empty() {
        return Ok(hash_matches);
    }

    if source.turn.fact_ids.len() == 1 && source_candidates.len() == 1 {
        return Ok(source_candidates);
    }

    let structured_source_matches = match_by_structured_fact(&source_candidates, fact);
    if !structured_source_matches.is_empty() {
        return Ok(structured_source_matches);
    }

    let broad_candidates = fetch_workspace_fact_candidates(ctx, fact).await?;
    Ok(match_by_structured_fact(&broad_candidates, fact))
}

async fn fetch_source_candidates(ctx: &IngestCtx, fact: &LedgerFact) -> Result<Vec<NodeIndexRow>> {
    sqlx::query_as::<_, NodeIndexRow>(
        r#"
        SELECT uid, label, workspace_id, user_id, scope, name, pii_class,
               valid_to, valid_from, properties_summary, last_accessed_at
        FROM moa.node_index
        WHERE label = 'Fact'
          AND workspace_id = $1
          AND properties_summary->>'source_session_id' = $2
          AND properties_summary->>'source_turn_seq' = $3
        ORDER BY uid
        "#,
    )
    .bind(fact.workspace_id.to_string())
    .bind(fact.source_session_id.to_string())
    .bind(fact.source_turn_seq.to_string())
    .fetch_all(&ctx.pool)
    .await
    .map_err(EvalError::from)
}

async fn fetch_workspace_fact_candidates(
    ctx: &IngestCtx,
    fact: &LedgerFact,
) -> Result<Vec<NodeIndexRow>> {
    sqlx::query_as::<_, NodeIndexRow>(
        r#"
        SELECT uid, label, workspace_id, user_id, scope, name, pii_class,
               valid_to, valid_from, properties_summary, last_accessed_at
        FROM moa.node_index
        WHERE label = 'Fact'
          AND workspace_id = $1
        ORDER BY uid
        "#,
    )
    .bind(fact.workspace_id.to_string())
    .fetch_all(&ctx.pool)
    .await
    .map_err(EvalError::from)
}

fn match_by_hash(candidates: &[NodeIndexRow], hashes: &BTreeSet<String>) -> Vec<NodeIndexRow> {
    if hashes.is_empty() {
        return Vec::new();
    }
    candidates
        .iter()
        .filter(|candidate| {
            property_text(candidate.properties_summary.as_ref(), "fact_hash")
                .is_some_and(|hash| hashes.contains(&hash))
        })
        .cloned()
        .collect()
}

fn match_by_structured_fact(candidates: &[NodeIndexRow], fact: &LedgerFact) -> Vec<NodeIndexRow> {
    candidates
        .iter()
        .filter(|candidate| structured_fact_matches(candidate.properties_summary.as_ref(), fact))
        .cloned()
        .collect()
}

fn structured_fact_matches(properties: Option<&Value>, fact: &LedgerFact) -> bool {
    let Some(properties) = properties else {
        return false;
    };

    let expected_subject = normalize_match_text(&fact.subject);
    let expected_predicate = normalize_match_text(&fact.predicate);
    let expected_object = normalize_match_text(&fact.object);

    let exact_fields = property_text(Some(properties), "subject")
        .is_some_and(|subject| subject == expected_subject)
        && property_text(Some(properties), "predicate")
            .is_some_and(|predicate| predicate == expected_predicate)
        && property_text(Some(properties), "object")
            .is_some_and(|object| object == expected_object);
    if exact_fields {
        return true;
    }

    let expected_phrase = normalize_match_text(&format!(
        "{} {} {}",
        fact.subject, fact.predicate, fact.object
    ));
    property_text(Some(properties), "summary")
        .is_some_and(|summary| summary.contains(&expected_phrase))
}

fn expected_fact_hashes(fact: &LedgerFact, source: &FactSource<'_>) -> Result<BTreeSet<String>> {
    let turn = SessionTurn {
        workspace_id: source.session.workspace_id.clone(),
        user_id: source.session.user_id.clone(),
        session_id: source.session.session_id,
        turn_seq: source.turn.turn_seq,
        transcript: source.turn.transcript.clone(),
        dominant_pii_class: fact.pii_class.as_str().to_string(),
        finalized_at: fact.valid_from,
    };
    let chunks = chunk_turn(&turn, CHUNK_TARGET_TOKENS, CHUNK_OVERLAP_TOKENS).map_err(|error| {
        EvalError::InvalidConfig(format!(
            "failed to chunk source turn for fact {}: {error}",
            fact.fact_id
        ))
    })?;
    let extracted = extract_facts(&chunks);
    let mut hashes = BTreeSet::new();
    for candidate in extracted {
        let candidate_matches = source.turn.fact_ids.len() == 1
            || extracted_fact_matches_ledger(
                &candidate.subject,
                &candidate.predicate,
                &candidate.object,
                fact,
            )
            || normalize_match_text(&candidate.summary).contains(&normalize_match_text(&format!(
                "{} {} {}",
                fact.subject, fact.predicate, fact.object
            )));
        if candidate_matches {
            let hash = fact_hash(&candidate).map_err(|error| {
                EvalError::InvalidConfig(format!(
                    "failed to hash extracted source fact {}: {error}",
                    fact.fact_id
                ))
            })?;
            hashes.insert(hex_bytes(&hash));
        }
    }
    Ok(hashes)
}

fn extracted_fact_matches_ledger(
    subject: &str,
    predicate: &str,
    object: &str,
    fact: &LedgerFact,
) -> bool {
    normalize_match_text(subject) == normalize_match_text(&fact.subject)
        && normalize_match_text(predicate) == normalize_match_text(&fact.predicate)
        && normalize_match_text(object) == normalize_match_text(&fact.object)
}

fn record_for_fact(
    fact: &LedgerFact,
    nodes: &[NodeIndexRow],
    superseded_by: &BTreeMap<String, Vec<String>>,
) -> GoldNodeRecord {
    let snapshots = node_snapshots(nodes);
    let node_uids = snapshots.iter().map(|node| node.uid).collect::<Vec<_>>();
    let status = match node_uids.len() {
        0 => GoldResolutionStatus::Unresolved,
        1 => GoldResolutionStatus::Resolved,
        _ => GoldResolutionStatus::Duplicate,
    };
    let active = snapshots.iter().any(|node| node.active);
    let scope = record_scope(&snapshots);
    let valid_from = snapshots.first().map(|node| node.valid_from);
    let valid_to = snapshots.first().and_then(|node| node.valid_to);
    let stored_pii_classes = snapshots
        .iter()
        .map(|node| node.pii_class.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let superseded_by = superseded_by
        .get(&fact.fact_id)
        .map_or_else(Vec::new, Clone::clone);
    let supersession_chain = supersession_chain(fact, &superseded_by);
    let pii_status = pii_status(fact, nodes);

    GoldNodeRecord {
        fact_id: fact.fact_id.clone(),
        node_uids,
        scope,
        active,
        valid_from,
        valid_to,
        resolution_status: status,
        expected_scope: scope_tier_str(fact.scope).to_string(),
        expected_valid_from: fact.valid_from,
        expected_valid_to: fact.valid_to,
        pii_status,
        stored_pii_classes,
        supersedes: sorted_strings(&fact.supersedes),
        superseded_by,
        supersession_chain,
        nodes: snapshots,
    }
}

fn ingest_ctx_for_turn(base: &IngestCtx, turn: &SessionTurn) -> IngestCtx {
    let scope = ScopeContext::workspace(turn.workspace_id.clone());
    let vector = Arc::new(PgvectorStore::new_for_app_role(
        base.pool.clone(),
        scope.clone(),
    ));
    let graph = Arc::new(
        AgeGraphStore::scoped_for_app_role(base.pool.clone(), scope)
            .with_vector_store(vector.clone()),
    );
    IngestCtx::new(
        base.pool.clone(),
        graph,
        vector,
        base.embedder.clone(),
        base.pii.clone(),
        base.contradict.clone(),
    )
}

fn pii_status(fact: &LedgerFact, nodes: &[NodeIndexRow]) -> GoldPiiStatus {
    if !fact.expected_redacted {
        return GoldPiiStatus::NotExpected;
    }
    if nodes.is_empty() {
        return GoldPiiStatus::NotResolved;
    }

    let statuses = nodes
        .iter()
        .map(|node| {
            let sensitive_object = normalize_match_text(&fact.object);
            let mut stored_text_parts = String::new();
            if let Some(summary) = property_text(node.properties_summary.as_ref(), "summary") {
                stored_text_parts.push_str(&summary);
            }
            stored_text_parts.push(' ');
            if let Some(object) = property_text(node.properties_summary.as_ref(), "object") {
                stored_text_parts.push_str(&object);
            }
            let stored_text = normalize_match_text(&stored_text_parts);
            let class_redacted = node.pii_class != PiiClass::None;
            let object_absent =
                sensitive_object.is_empty() || !stored_text.contains(&sensitive_object);
            if class_redacted && object_absent {
                GoldPiiStatus::Redacted
            } else {
                GoldPiiStatus::Unredacted
            }
        })
        .collect::<BTreeSet<_>>();

    if statuses.len() == 1 {
        match statuses.into_iter().next() {
            Some(status) => status,
            None => GoldPiiStatus::Unredacted,
        }
    } else {
        GoldPiiStatus::Mixed
    }
}

fn node_snapshots(nodes: &[NodeIndexRow]) -> Vec<GoldNodeSnapshot> {
    let mut snapshots = nodes
        .iter()
        .filter(|node| node.label == NodeLabel::Fact)
        .map(|node| GoldNodeSnapshot {
            uid: node.uid,
            scope: node.scope.clone(),
            active: node.valid_to.is_none(),
            valid_from: node.valid_from,
            valid_to: node.valid_to,
            pii_class: node.pii_class.as_str().to_string(),
        })
        .collect::<Vec<_>>();
    snapshots.sort_by_key(|node| node.uid);
    snapshots
}

fn record_scope(nodes: &[GoldNodeSnapshot]) -> Option<String> {
    let scopes = nodes
        .iter()
        .map(|node| node.scope.as_str())
        .collect::<BTreeSet<_>>();
    match scopes.len() {
        0 => None,
        1 => scopes.into_iter().next().map(ToOwned::to_owned),
        _ => Some("mixed".to_string()),
    }
}

fn superseded_by(ledger: &[LedgerFact]) -> BTreeMap<String, Vec<String>> {
    let mut by_fact = BTreeMap::<String, Vec<String>>::new();
    for fact in ledger {
        for superseded in &fact.supersedes {
            by_fact
                .entry(superseded.clone())
                .or_default()
                .push(fact.fact_id.clone());
        }
    }
    for facts in by_fact.values_mut() {
        facts.sort();
        facts.dedup();
    }
    by_fact
}

fn supersession_chain(fact: &LedgerFact, superseded_by: &[String]) -> Vec<String> {
    let mut chain = BTreeSet::new();
    chain.insert(fact.fact_id.clone());
    chain.extend(fact.supersedes.iter().cloned());
    chain.extend(superseded_by.iter().cloned());
    chain.into_iter().collect()
}

fn deterministic_turn_order<'a>(
    facts: &HashMap<&'a str, &'a LedgerFact>,
    sessions: &'a [SyntheticSession],
) -> Result<Vec<FactSource<'a>>> {
    let mut turns = Vec::new();
    for session in sessions {
        for turn in &session.turns {
            turns.push(FactSource { session, turn });
        }
    }
    for source in &turns {
        turn_finalized_at(source.turn, facts)?;
    }
    turns.sort_by(|left, right| {
        let left_time = turn_finalized_at(left.turn, facts);
        let right_time = turn_finalized_at(right.turn, facts);
        match (left_time, right_time) {
            (Ok(left_time), Ok(right_time)) => (
                left_time,
                left.session.session_id.to_string(),
                left.turn.turn_seq,
            )
                .cmp(&(
                    right_time,
                    right.session.session_id.to_string(),
                    right.turn.turn_seq,
                )),
            _ => std::cmp::Ordering::Equal,
        }
    });
    Ok(turns)
}

fn sources_by_fact_id<'a>(
    facts: &HashMap<&'a str, &'a LedgerFact>,
    sessions: &'a [SyntheticSession],
) -> Result<HashMap<String, FactSource<'a>>> {
    let mut sources = HashMap::new();
    for session in sessions {
        for turn in &session.turns {
            for fact_id in &turn.fact_ids {
                let Some(fact) = facts.get(fact_id.as_str()) else {
                    return Err(EvalError::InvalidConfig(format!(
                        "synthetic session {} turn {} references missing fact_id {}",
                        session.session_id, turn.turn_seq, fact_id
                    )));
                };
                if fact.source_session_id != session.session_id
                    || fact.source_turn_seq != turn.turn_seq
                {
                    return Err(EvalError::InvalidConfig(format!(
                        "ledger fact {} source {}:{} does not match synthetic turn {}:{}",
                        fact.fact_id,
                        fact.source_session_id,
                        fact.source_turn_seq,
                        session.session_id,
                        turn.turn_seq
                    )));
                }
                if sources
                    .insert(fact_id.clone(), FactSource { session, turn })
                    .is_some()
                {
                    return Err(EvalError::InvalidConfig(format!(
                        "ledger fact {fact_id} appears in multiple synthetic turns"
                    )));
                }
            }
        }
    }

    for fact in facts.values() {
        if !sources.contains_key(&fact.fact_id) {
            return Err(EvalError::InvalidConfig(format!(
                "ledger fact {} has no matching synthetic turn",
                fact.fact_id
            )));
        }
    }
    Ok(sources)
}

fn session_turn(
    source: &FactSource<'_>,
    facts: &HashMap<&str, &LedgerFact>,
) -> Result<SessionTurn> {
    Ok(SessionTurn {
        workspace_id: source.session.workspace_id.clone(),
        user_id: source.session.user_id.clone(),
        session_id: source.session.session_id,
        turn_seq: source.turn.turn_seq,
        transcript: source.turn.transcript.clone(),
        dominant_pii_class: dominant_pii_class(source.turn, facts)?,
        finalized_at: turn_finalized_at(source.turn, facts)?,
    })
}

fn dominant_pii_class(turn: &SyntheticTurn, facts: &HashMap<&str, &LedgerFact>) -> Result<String> {
    let mut rank = 0_u8;
    let mut class = "none";
    for fact_id in &turn.fact_ids {
        let fact = facts.get(fact_id.as_str()).ok_or_else(|| {
            EvalError::InvalidConfig(format!(
                "synthetic turn {} references missing fact_id {}",
                turn.turn_seq, fact_id
            ))
        })?;
        let candidate_rank = pii_rank(fact.pii_class);
        if candidate_rank > rank {
            rank = candidate_rank;
            class = fact.pii_class.as_str();
        }
    }
    Ok(class.to_string())
}

fn pii_rank(class: PiiClass) -> u8 {
    match class {
        PiiClass::None => 0,
        PiiClass::Pii => 1,
        PiiClass::Phi => 2,
        PiiClass::Restricted => 3,
    }
}

fn turn_finalized_at(
    turn: &SyntheticTurn,
    facts: &HashMap<&str, &LedgerFact>,
) -> Result<DateTime<Utc>> {
    let mut timestamps = Vec::new();
    for fact_id in &turn.fact_ids {
        let fact = facts.get(fact_id.as_str()).ok_or_else(|| {
            EvalError::InvalidConfig(format!(
                "synthetic turn {} references missing fact_id {}",
                turn.turn_seq, fact_id
            ))
        })?;
        timestamps.push(fact.valid_from);
    }
    timestamps.into_iter().min().ok_or_else(|| {
        EvalError::InvalidConfig(format!(
            "synthetic turn {} must reference at least one fact for gold resolution",
            turn.turn_seq
        ))
    })
}

fn facts_by_id(ledger: &[LedgerFact]) -> Result<HashMap<&str, &LedgerFact>> {
    let mut facts = HashMap::new();
    for fact in ledger {
        if facts.insert(fact.fact_id.as_str(), fact).is_some() {
            return Err(EvalError::InvalidConfig(format!(
                "duplicate ledger fact_id {}",
                fact.fact_id
            )));
        }
    }
    Ok(facts)
}

fn sorted_facts(ledger: &[LedgerFact]) -> Vec<&LedgerFact> {
    let mut facts = ledger.iter().collect::<Vec<_>>();
    facts.sort_by(|left, right| left.fact_id.cmp(&right.fact_id));
    facts
}

fn property_text(properties: Option<&Value>, key: &str) -> Option<String> {
    properties
        .and_then(|properties| properties.get(key))
        .and_then(Value::as_str)
        .map(normalize_match_text)
}

fn normalize_match_text(text: &str) -> String {
    let mut normalized = String::new();
    let mut previous_was_space = true;
    for character in text.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            normalized.push(character);
            previous_was_space = false;
        } else if !previous_was_space {
            normalized.push(' ');
            previous_was_space = true;
        }
    }
    normalized.trim().to_string()
}

fn scope_tier_str(scope: moa_core::ScopeTier) -> &'static str {
    match scope {
        moa_core::ScopeTier::Global => "global",
        moa_core::ScopeTier::Workspace => "workspace",
        moa_core::ScopeTier::User => "user",
    }
}

fn sorted_strings(values: &[String]) -> Vec<String> {
    let mut values = values.to_vec();
    values.sort();
    values.dedup();
    values
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn read_jsonl<T>(path: &Path) -> Result<Vec<T>>
where
    T: DeserializeOwned,
{
    let file = File::open(path)
        .await
        .map_err(|source| io_error(path, source))?;
    let mut lines = BufReader::new(file).lines();
    let mut records = Vec::new();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|source| io_error(path, source))?
    {
        if line.trim().is_empty() {
            continue;
        }
        records.push(serde_json::from_str(&line)?);
    }
    Ok(records)
}

async fn write_jsonl<T>(path: &Path, records: &[T]) -> Result<()>
where
    T: Serialize,
{
    ensure_parent_dir(path).await?;
    let mut file = File::create(path)
        .await
        .map_err(|source| io_error(path, source))?;
    for record in records {
        let line = serde_json::to_vec(record)?;
        file.write_all(&line)
            .await
            .map_err(|source| io_error(path, source))?;
        file.write_all(b"\n")
            .await
            .map_err(|source| io_error(path, source))?;
    }
    file.flush().await.map_err(|source| io_error(path, source))
}

async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| io_error(parent, source))?;
    }
    Ok(())
}

fn io_error(path: &Path, source: std::io::Error) -> EvalError {
    EvalError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Debug, Clone, Copy)]
struct FactSource<'a> {
    session: &'a SyntheticSession,
    turn: &'a SyntheticTurn,
}
