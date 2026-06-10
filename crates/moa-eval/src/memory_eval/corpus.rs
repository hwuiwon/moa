//! Versioned memory-evaluation corpus schema and JSONL helpers.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use moa_core::{ScopeTier, SessionId, UserId, WorkspaceId};
use moa_memory_graph::PiiClass;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{EvalError, Result};

/// Current schema version for memory evaluation corpus files.
pub const CORPUS_SCHEMA_VERSION: u32 = 1;

/// Manifest for a memory evaluation corpus directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusManifest {
    /// Corpus schema version.
    pub version: u32,
    /// Stable corpus identifier.
    pub corpus_id: String,
    /// Size and generation profile.
    pub profile: CorpusProfile,
    /// Human-readable description of the corpus.
    pub description: String,
    /// Deterministic generation seeds that produced this corpus.
    pub seeds: Vec<u64>,
    /// Transcript rendering style used by synthetic source sessions.
    #[serde(default)]
    pub transcript_style: TranscriptStyle,
}

impl CorpusManifest {
    /// Validates manifest fields that are independent of corpus records.
    pub fn validate(&self) -> Result<()> {
        if self.version != CORPUS_SCHEMA_VERSION {
            return invalid_config(format!(
                "memory eval corpus version {} is not supported; expected {}",
                self.version, CORPUS_SCHEMA_VERSION
            ));
        }
        ensure_non_empty("corpus_id", &self.corpus_id)
    }
}

/// Supported memory-evaluation corpus sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorpusProfile {
    /// PR-sized hermetic corpus for routine development and CI checks.
    Pr,
    /// Larger corpus for nightly or manual runs.
    Full,
}

/// Rendering style for synthetic source transcripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptStyle {
    /// Legacy marker-heavy transcripts optimized for the heuristic extractor.
    #[default]
    Marked,
    /// Conversational transcripts with no fact or scope markers.
    Natural,
}

/// A ledger-first fact that probes should be able to retrieve or suppress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerFact {
    /// Workspace whose graph scope contains this fact.
    pub workspace_id: WorkspaceId,
    /// User who owns the fact for user scope, or authored it for broader scopes.
    pub user_id: UserId,
    /// Graph-memory scope tier for this fact.
    pub scope: ScopeTier,
    /// Stable synthetic fact identifier.
    pub fact_id: String,
    /// Start of the fact's valid-time interval.
    pub valid_from: DateTime<Utc>,
    /// End of the fact's valid-time interval, when superseded or expired.
    pub valid_to: Option<DateTime<Utc>>,
    /// Fact subject.
    pub subject: String,
    /// Fact predicate.
    pub predicate: String,
    /// Fact object.
    pub object: String,
    /// Gold answer text expected when this fact satisfies a probe.
    pub answer: String,
    /// Prior facts superseded by this fact.
    #[serde(default)]
    pub supersedes: Vec<String>,
    /// Synthetic session containing the source turn for this fact.
    pub source_session_id: SessionId,
    /// Synthetic source turn sequence inside the session.
    pub source_turn_seq: u64,
    /// Privacy class expected after ingestion.
    pub pii_class: PiiClass,
    /// Whether answer material should be redacted before scoring or display.
    pub expected_redacted: bool,
}

impl LedgerFact {
    /// Validates field-level invariants for one ledger fact.
    pub fn validate(&self) -> Result<()> {
        ensure_non_empty("ledger fact workspace_id", self.workspace_id.as_str())?;
        ensure_non_empty("ledger fact user_id", self.user_id.as_str())?;
        ensure_non_empty("ledger fact fact_id", &self.fact_id)?;
        ensure_non_empty("ledger fact subject", &self.subject)?;
        ensure_non_empty("ledger fact predicate", &self.predicate)?;
        ensure_non_empty("ledger fact object", &self.object)?;
        ensure_non_empty("ledger fact answer", &self.answer)?;
        for superseded in &self.supersedes {
            ensure_non_empty("ledger fact supersedes", superseded)?;
        }
        Ok(())
    }
}

/// A synthetic session rendered from ledger facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntheticSession {
    /// Session identifier used by source provenance fields.
    pub session_id: SessionId,
    /// Workspace that owns this session.
    pub workspace_id: WorkspaceId,
    /// User that produced this session.
    pub user_id: UserId,
    /// Ordered synthetic turns in the session.
    pub turns: Vec<SyntheticTurn>,
}

impl SyntheticSession {
    /// Validates the session and its turns.
    pub fn validate(&self) -> Result<()> {
        ensure_non_empty("synthetic session workspace_id", self.workspace_id.as_str())?;
        ensure_non_empty("synthetic session user_id", self.user_id.as_str())?;
        if self.turns.is_empty() {
            return invalid_config(format!(
                "synthetic session {} must contain at least one turn",
                self.session_id
            ));
        }

        let mut turn_sequences = HashSet::new();
        for turn in &self.turns {
            turn.validate()?;
            if !turn_sequences.insert(turn.turn_seq) {
                return invalid_config(format!(
                    "synthetic session {} has duplicate turn_seq {}",
                    self.session_id, turn.turn_seq
                ));
            }
        }
        Ok(())
    }
}

/// One synthetic source turn used to drive memory ingestion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntheticTurn {
    /// Durable turn sequence used for source attribution.
    pub turn_seq: u64,
    /// Transcript text to feed to memory ingestion.
    pub transcript: String,
    /// Ledger facts intentionally planted by this turn.
    #[serde(default)]
    pub fact_ids: Vec<String>,
}

impl SyntheticTurn {
    /// Validates the synthetic turn fields.
    pub fn validate(&self) -> Result<()> {
        ensure_non_empty("synthetic turn transcript", &self.transcript)?;
        for fact_id in &self.fact_ids {
            ensure_non_empty("synthetic turn fact_id", fact_id)?;
        }
        Ok(())
    }
}

/// One retrieval or answer probe for the memory evaluation corpus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    /// Stable probe identifier.
    pub probe_id: String,
    /// Probe behavior class.
    pub probe_type: ProbeType,
    /// Workspace to query.
    pub workspace_id: WorkspaceId,
    /// User asking the query.
    pub user_id: UserId,
    /// Natural-language query passed into retrieval.
    pub query: String,
    /// Gold answer expected from a faithful answerer.
    pub answer: String,
    /// Facts that should support a successful answer.
    #[serde(default)]
    pub expected_fact_ids: Vec<String>,
    /// Facts that must not be returned or exposed for this probe.
    #[serde(default)]
    pub blocked_fact_ids: Vec<String>,
    /// Optional valid-time instant for temporal retrieval probes.
    pub as_of: Option<DateTime<Utc>>,
    /// Whether the expected answer should be redacted.
    pub expected_redacted: bool,
}

impl Probe {
    /// Validates fields that do not require access to the ledger.
    pub fn validate(&self) -> Result<()> {
        ensure_non_empty("probe probe_id", &self.probe_id)?;
        ensure_non_empty("probe workspace_id", self.workspace_id.as_str())?;
        ensure_non_empty("probe user_id", self.user_id.as_str())?;
        ensure_non_empty("probe query", &self.query)?;
        ensure_non_empty("probe answer", &self.answer)?;
        for fact_id in self.referenced_fact_ids() {
            ensure_non_empty("probe referenced fact_id", fact_id)?;
        }
        Ok(())
    }

    /// Returns all fact identifiers referenced by this probe.
    pub fn referenced_fact_ids(&self) -> impl Iterator<Item = &str> {
        self.expected_fact_ids
            .iter()
            .chain(self.blocked_fact_ids.iter())
            .map(String::as_str)
    }
}

/// Probe categories supported by memory retrieval evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeType {
    /// Recall one known fact.
    PointRecall,
    /// Resolve an updated fact to the latest value.
    LatestValueAfterUpdate,
    /// Abstain when no scoped memory should answer the query.
    Abstention,
    /// Verify user-private memory does not leak across users.
    CrossUserIsolation,
    /// Recall workspace-shared memory for a workspace member.
    WorkspaceSharedFact,
    /// Combine multiple retrieved facts.
    MultiHop,
    /// Retrieve the fact valid at a requested historical instant.
    TemporalAsOf,
    /// Apply a user preference to a task.
    PreferenceApplication,
    /// Verify PII-bearing facts are redacted.
    PiiRedaction,
}

/// Reads and validates `manifest.json`.
pub async fn read_manifest_json(path: &Path) -> Result<CorpusManifest> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .map_err(|source| io_error(path, source))?;
    let manifest: CorpusManifest = serde_json::from_str(&raw)?;
    manifest.validate()?;
    Ok(manifest)
}

/// Writes and validates `manifest.json`.
pub async fn write_manifest_json(path: &Path, manifest: &CorpusManifest) -> Result<()> {
    manifest.validate()?;
    write_json_file(path, manifest).await
}

/// Reads and validates `ledger.jsonl`.
pub async fn read_ledger_jsonl(path: &Path) -> Result<Vec<LedgerFact>> {
    let facts = read_jsonl(path).await?;
    validate_ledger(&facts)?;
    Ok(facts)
}

/// Writes and validates `ledger.jsonl`.
pub async fn write_ledger_jsonl(path: &Path, facts: &[LedgerFact]) -> Result<()> {
    validate_ledger(facts)?;
    write_jsonl(path, facts).await
}

/// Reads and validates `sessions.jsonl`.
pub async fn read_sessions_jsonl(path: &Path) -> Result<Vec<SyntheticSession>> {
    let sessions = read_jsonl(path).await?;
    validate_sessions(&sessions)?;
    Ok(sessions)
}

/// Writes and validates `sessions.jsonl`.
pub async fn write_sessions_jsonl(path: &Path, sessions: &[SyntheticSession]) -> Result<()> {
    validate_sessions(sessions)?;
    write_jsonl(path, sessions).await
}

/// Reads and validates `probes.jsonl` against the ledger facts.
pub async fn read_probes_jsonl(path: &Path, facts: &[LedgerFact]) -> Result<Vec<Probe>> {
    let probes = read_jsonl(path).await?;
    validate_probes(&probes, facts)?;
    Ok(probes)
}

/// Writes and validates `probes.jsonl` against the ledger facts.
pub async fn write_probes_jsonl(path: &Path, probes: &[Probe], facts: &[LedgerFact]) -> Result<()> {
    validate_probes(probes, facts)?;
    write_jsonl(path, probes).await
}

/// Validates a full memory evaluation corpus after loading all files.
pub fn validate_corpus(
    manifest: &CorpusManifest,
    facts: &[LedgerFact],
    sessions: &[SyntheticSession],
    probes: &[Probe],
) -> Result<()> {
    manifest.validate()?;
    validate_ledger(facts)?;
    validate_sessions(sessions)?;
    validate_probes(probes, facts)
}

/// Validates ledger fact records.
pub fn validate_ledger(facts: &[LedgerFact]) -> Result<()> {
    let mut fact_ids = HashSet::new();
    for fact in facts {
        fact.validate()?;
        if !fact_ids.insert(fact.fact_id.as_str()) {
            return invalid_config(format!("duplicate ledger fact_id {}", fact.fact_id));
        }
    }
    Ok(())
}

/// Validates synthetic session records.
pub fn validate_sessions(sessions: &[SyntheticSession]) -> Result<()> {
    let mut session_ids = HashSet::new();
    for session in sessions {
        session.validate()?;
        if !session_ids.insert(session.session_id) {
            return invalid_config(format!(
                "duplicate synthetic session_id {}",
                session.session_id
            ));
        }
    }
    Ok(())
}

/// Validates probes against the ledger they reference.
pub fn validate_probes(probes: &[Probe], facts: &[LedgerFact]) -> Result<()> {
    let facts_by_id: HashMap<&str, &LedgerFact> = facts
        .iter()
        .map(|fact| (fact.fact_id.as_str(), fact))
        .collect();
    let mut probe_ids = HashSet::new();

    for probe in probes {
        probe.validate()?;
        if !probe_ids.insert(probe.probe_id.as_str()) {
            return invalid_config(format!("duplicate probe_id {}", probe.probe_id));
        }

        for fact_id in probe.referenced_fact_ids() {
            let Some(fact) = facts_by_id.get(fact_id) else {
                return invalid_config(format!(
                    "probe {} references missing fact_id {}",
                    probe.probe_id, fact_id
                ));
            };

            if probe.probe_type == ProbeType::CrossUserIsolation
                && fact.scope == ScopeTier::User
                && fact.user_id == probe.user_id
            {
                return invalid_config(format!(
                    "cross-user isolation probe {} asks as owning user {} for fact_id {}",
                    probe.probe_id, probe.user_id, fact_id
                ));
            }
        }
    }

    Ok(())
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

async fn write_json_file<T>(path: &Path, document: &T) -> Result<()>
where
    T: Serialize,
{
    ensure_parent_dir(path).await?;
    let bytes = serde_json::to_vec_pretty(document)?;
    tokio::fs::write(path, bytes)
        .await
        .map_err(|source| io_error(path, source))
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

fn ensure_non_empty(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return invalid_config(format!("{label} must not be empty"));
    }
    Ok(())
}

fn invalid_config<T>(message: String) -> Result<T> {
    Err(EvalError::InvalidConfig(message))
}

fn io_error(path: &Path, source: std::io::Error) -> EvalError {
    EvalError::Io {
        path: path.to_path_buf(),
        source,
    }
}
