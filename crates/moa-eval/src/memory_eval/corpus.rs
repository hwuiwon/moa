//! Versioned memory-evaluation corpus schema and JSONL helpers.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use moa_core::types::security::SensitivityClass;
use moa_core::{
    types::identifiers::SessionId, types::identifiers::StoragePartitionId,
    types::identifiers::UserId,
};
use moa_memory_types::ScopeTier;
use serde::{Deserialize, Serialize};

use moa_eval_core::{EvalError, Result};

use super::io::{
    ensure_non_empty, ensure_parent_dir, invalid_config, io_error, read_jsonl, write_jsonl,
};

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
    /// Marker-heavy transcripts optimized for the heuristic extractor.
    #[default]
    Marked,
    /// Conversational transcripts with no fact or scope markers.
    Natural,
}

/// A ledger-first fact that probes should be able to retrieve or suppress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerFact {
    /// Storage partition whose graph scope contains this fact.
    pub storage_partition_id: StoragePartitionId,
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
    pub supersedes: Vec<String>,
    /// Canonical fact id restated verbatim by this fact.
    pub restates: Option<String>,
    /// Synthetic prior retrieval uses for quality-score seeding.
    pub prior_uses: Option<u32>,
    /// Synthetic prior successful retrieval uses for quality-score seeding.
    pub prior_successes: Option<u32>,
    /// Synthetic session containing the source turn for this fact.
    pub source_session_id: SessionId,
    /// Synthetic source turn sequence inside the session.
    pub source_turn_seq: u64,
    /// Privacy class expected after ingestion.
    pub pii_class: SensitivityClass,
    /// Whether answer material should be redacted before scoring or display.
    pub expected_redacted: bool,
}

impl LedgerFact {
    /// Validates field-level invariants for one ledger fact.
    pub fn validate(&self) -> Result<()> {
        ensure_non_empty(
            "ledger fact storage_partition_id",
            self.storage_partition_id.as_str(),
        )?;
        ensure_non_empty("ledger fact user_id", self.user_id.as_str())?;
        ensure_non_empty("ledger fact fact_id", &self.fact_id)?;
        ensure_non_empty("ledger fact subject", &self.subject)?;
        ensure_non_empty("ledger fact predicate", &self.predicate)?;
        ensure_non_empty("ledger fact object", &self.object)?;
        ensure_non_empty("ledger fact answer", &self.answer)?;
        for superseded in &self.supersedes {
            ensure_non_empty("ledger fact supersedes", superseded)?;
        }
        if let Some(restates) = &self.restates {
            ensure_non_empty("ledger fact restates", restates)?;
        }
        if let Some(successes) = self.prior_successes {
            let Some(uses) = self.prior_uses else {
                return invalid_config(format!(
                    "ledger fact {} has prior_successes without prior_uses",
                    self.fact_id
                ));
            };
            if successes > uses {
                return invalid_config(format!(
                    "ledger fact {} has prior_successes {} greater than prior_uses {}",
                    self.fact_id, successes, uses
                ));
            }
        }
        Ok(())
    }
}

/// A synthetic session rendered from ledger facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntheticSession {
    /// Session identifier used by source provenance fields.
    pub session_id: SessionId,
    /// Storage partition that owns this session.
    pub storage_partition_id: StoragePartitionId,
    /// User that produced this session.
    pub user_id: UserId,
    /// Ordered synthetic turns in the session.
    pub turns: Vec<SyntheticTurn>,
}

impl SyntheticSession {
    /// Validates the session and its turns.
    pub fn validate(&self) -> Result<()> {
        ensure_non_empty(
            "synthetic session storage_partition_id",
            self.storage_partition_id.as_str(),
        )?;
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
    /// Storage partition to query.
    pub storage_partition_id: StoragePartitionId,
    /// User asking the query.
    pub user_id: UserId,
    /// Natural-language query passed into retrieval.
    pub query: String,
    /// Deterministic rewritten retrieval query for rewrite-policy A/B runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite_query: Option<String>,
    /// Expected gated rewrite decision for this probe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_rewrite: Option<bool>,
    /// Query class label used by rewrite-policy metrics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_class: Option<String>,
    /// Gold answer expected from a faithful answerer.
    pub answer: String,
    /// Facts that should support a successful answer.
    #[serde(default)]
    pub expected_fact_ids: Vec<String>,
    /// Graded 0-3 relevance per expected fact for graded ranking metrics.
    ///
    /// Absent entries default to the maximum grade, so binary-labeled golden
    /// sets stay valid while graded sets can rank partially relevant memories.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub expected_fact_grades: std::collections::BTreeMap<String, u8>,
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
        ensure_non_empty(
            "probe storage_partition_id",
            self.storage_partition_id.as_str(),
        )?;
        ensure_non_empty("probe user_id", self.user_id.as_str())?;
        ensure_non_empty("probe query", &self.query)?;
        if let Some(rewrite_query) = &self.rewrite_query {
            ensure_non_empty("probe rewrite_query", rewrite_query)?;
        }
        if let Some(query_class) = &self.query_class {
            ensure_non_empty("probe query_class", query_class)?;
        }
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
    /// Recall tenant-shared memory for a tenant member.
    TenantSharedFact,
    /// Combine multiple retrieved facts.
    MultiHop,
    /// Retrieve the fact valid at a requested historical instant.
    TemporalAsOf,
    /// Apply a user preference to a task.
    PreferenceApplication,
    /// Verify PII-bearing facts are redacted.
    PiiRedaction,
}

impl ProbeType {
    /// Returns the stable snake_case slice key matching the serde wire form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PointRecall => "point_recall",
            Self::LatestValueAfterUpdate => "latest_value_after_update",
            Self::Abstention => "abstention",
            Self::CrossUserIsolation => "cross_user_isolation",
            Self::TenantSharedFact => "tenant_shared_fact",
            Self::MultiHop => "multi_hop",
            Self::TemporalAsOf => "temporal_as_of",
            Self::PreferenceApplication => "preference_application",
            Self::PiiRedaction => "pii_redaction",
        }
    }
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
    let facts_by_id = facts
        .iter()
        .map(|fact| (fact.fact_id.as_str(), fact))
        .collect::<HashMap<_, _>>();
    for fact in facts {
        let Some(canonical_id) = fact.restates.as_deref() else {
            continue;
        };
        if canonical_id == fact.fact_id {
            return invalid_config(format!(
                "ledger fact {} cannot restate itself",
                fact.fact_id
            ));
        }
        let canonical = facts_by_id.get(canonical_id).ok_or_else(|| {
            EvalError::InvalidConfig(format!(
                "ledger fact {} restates missing canonical fact_id {}",
                fact.fact_id, canonical_id
            ))
        })?;
        if fact.subject != canonical.subject
            || fact.predicate != canonical.predicate
            || fact.object != canonical.object
            || fact.answer != canonical.answer
        {
            return invalid_config(format!(
                "ledger fact {} restates {} with mismatched subject/predicate/object/answer",
                fact.fact_id, canonical_id
            ));
        }
        if fact.source_session_id == canonical.source_session_id {
            return invalid_config(format!(
                "ledger fact {} restates {} in the same source session",
                fact.fact_id, canonical_id
            ));
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
                && fact.scope == ScopeTier::Contact
                && fact.user_id == probe.user_id
            {
                return invalid_config(format!(
                    "cross-user isolation probe {} asks as owning user {} for fact_id {}",
                    probe.probe_id, probe.user_id, fact_id
                ));
            }
            if fact.restates.is_some() {
                return invalid_config(format!(
                    "probe {} references restating fact_id {}; probes must target the canonical fact",
                    probe.probe_id, fact_id
                ));
            }
        }
    }

    Ok(())
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

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use moa_core::types::security::SensitivityClass;
    use moa_core::{
        types::identifiers::SessionId, types::identifiers::StoragePartitionId,
        types::identifiers::UserId,
    };
    use moa_memory_types::ScopeTier;
    use uuid::Uuid;

    use super::{
        CORPUS_SCHEMA_VERSION, CorpusManifest, CorpusProfile, LedgerFact, TranscriptStyle,
        validate_corpus,
    };

    #[test]
    fn validate_corpus_rejects_restatement_with_mismatched_spo() {
        // Pins: restatement pairs must be exact structured duplicates of their canonical fact.
        let manifest = CorpusManifest {
            version: CORPUS_SCHEMA_VERSION,
            corpus_id: "restatement-validation".to_string(),
            profile: CorpusProfile::Pr,
            description: "test".to_string(),
            seeds: vec![1, 2, 3],
            transcript_style: TranscriptStyle::Marked,
        };
        let canonical = fact("canonical", "repo/control-plane", None, 1);
        let restating = fact(
            "restating",
            "repo/different",
            Some("canonical".to_string()),
            2,
        );

        let error = validate_corpus(&manifest, &[canonical, restating], &[], &[])
            .expect_err("mismatched restatement should be rejected");

        assert!(error.to_string().contains("mismatched"));
    }

    #[test]
    fn validate_ledger_rejects_prior_successes_above_uses() {
        // Pins: synthetic quality priors cannot encode impossible success counts.
        let mut fact = fact("fact-prior", "repo/control-plane", None, 1);
        fact.prior_uses = Some(2);
        fact.prior_successes = Some(3);

        let error = fact
            .validate()
            .expect_err("successes above uses should fail");

        assert!(error.to_string().contains("greater than prior_uses"));
    }

    fn fact(
        fact_id: &str,
        object: &str,
        restates: Option<String>,
        session_suffix: u128,
    ) -> LedgerFact {
        LedgerFact {
            storage_partition_id: StoragePartitionId::new("tenant-a"),
            user_id: UserId::new("user-a"),
            scope: ScopeTier::Contact,
            fact_id: fact_id.to_string(),
            valid_from: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            valid_to: None,
            subject: "user-a".to_string(),
            predicate: "private_repository".to_string(),
            object: object.to_string(),
            answer: format!("user-a uses {object}."),
            supersedes: Vec::new(),
            restates,
            prior_uses: None,
            prior_successes: None,
            source_session_id: SessionId(Uuid::from_u128(session_suffix)),
            source_turn_seq: 1,
            pii_class: SensitivityClass::None,
            expected_redacted: false,
        }
    }
}
