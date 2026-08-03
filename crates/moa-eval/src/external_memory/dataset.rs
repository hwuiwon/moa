//! Versioned backend-neutral dataset and package contracts.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use moa_core::canonical_json::canonical_json_bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ExternalMemoryError, Result};
use crate::kernel::contamination::{
    DEFAULT_ANSWER_CONTAINMENT, DEFAULT_NEAR_DUPLICATE_JACCARD,
    DEFAULT_QUESTION_RESTATEMENT_CONTAINMENT, containment, jaccard, normalize, shingles,
};

/// Schema version accepted for common external-memory cases.
pub const EXTERNAL_MEMORY_CASE_SCHEMA_VERSION: u32 = 1;
/// Schema version accepted for dataset package manifests.
pub const DATASET_PACKAGE_SCHEMA_VERSION: u32 = 1;
/// Schema version accepted for verified fetch summaries.
pub const VERIFIED_FETCH_SUMMARY_SCHEMA_VERSION: u32 = 1;
const DATASET_PACKAGE_HASH_DOMAIN: &[u8] = b"moa.external-memory.package.v1\0";

/// One backend-neutral external-memory benchmark case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalMemoryCaseV1 {
    /// Version of this case contract.
    pub schema_version: u32,
    /// Hard isolation boundary for this case.
    pub isolation_key: String,
    /// Timestamped conversation sessions to form into memory.
    pub sessions: Vec<ExternalMemorySession>,
    /// Question answered after formation and retrieval.
    pub question: String,
    /// Optional answer choices.
    #[serde(default)]
    pub options: Vec<String>,
    /// Dataset-owned reference answer.
    pub answer: String,
    /// Dataset-owned category or slice.
    pub category: String,
    /// Independent session- and turn-level evidence labels.
    #[serde(default)]
    pub evidence_labels: EvidenceLabels,
}

/// One timestamped session occurrence in an external-memory case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalMemorySession {
    /// Stable occurrence-level source identifier.
    pub source_id: String,
    /// Session timestamp used for chronological ordering.
    pub occurred_at: DateTime<Utc>,
    /// Timestamped turns in original dataset order.
    pub turns: Vec<ExternalMemoryTurn>,
}

/// One timestamped turn occurrence in an external-memory case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalMemoryTurn {
    /// Stable occurrence-level source identifier.
    pub source_id: String,
    /// Turn timestamp used for chronological ordering.
    pub occurred_at: DateTime<Utc>,
    /// Dataset-neutral speaker label.
    pub role: String,
    /// Turn text passed to the backend's production ingestion path.
    pub text: String,
}

/// Independent optional evidence labels supplied by a dataset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceLabels {
    /// Gold session occurrence IDs, when the dataset provides them.
    #[serde(default)]
    pub session_source_ids: Option<Vec<String>>,
    /// Gold turn occurrence IDs, when the dataset provides them.
    #[serde(default)]
    pub turn_source_ids: Option<Vec<String>>,
}

/// One validated turn in deterministic chronological ingest order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChronologicalTurn {
    /// Stable session occurrence ID.
    pub session_source_id: String,
    /// Stable turn occurrence ID.
    pub turn_source_id: String,
    /// Timestamp used for chronological ingestion.
    pub occurred_at: DateTime<Utc>,
    /// Original session occurrence index used as a tie-breaker.
    pub session_source_order: usize,
    /// Original turn occurrence index used as a tie-breaker.
    pub turn_source_order: usize,
    /// Dataset-neutral speaker label.
    pub role: String,
    /// Turn text passed to the backend.
    pub text: String,
}

/// Validated case with a deterministic chronological ingest projection.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedExternalMemoryCase {
    /// Original versioned case.
    pub case: ExternalMemoryCaseV1,
    /// Stable chronological ingest order.
    pub chronological_turns: Vec<ChronologicalTurn>,
}

impl PreparedExternalMemoryCase {
    /// Returns a conservative token estimate for the complete source context.
    #[must_use]
    pub fn full_context_token_estimate(&self) -> usize {
        self.chronological_turns
            .iter()
            .map(|turn| estimate_tokens(&turn.text))
            .sum()
    }
}

/// Validates a versioned case and derives its chronological ingest order.
pub fn validate_case(case: ExternalMemoryCaseV1) -> Result<PreparedExternalMemoryCase> {
    if case.schema_version != EXTERNAL_MEMORY_CASE_SCHEMA_VERSION {
        return Err(ExternalMemoryError::InvalidDataset(format!(
            "unsupported case schema version {}",
            case.schema_version
        )));
    }
    for (name, value) in [
        ("isolation_key", case.isolation_key.as_str()),
        ("question", case.question.as_str()),
        ("answer", case.answer.as_str()),
        ("category", case.category.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(ExternalMemoryError::InvalidDataset(format!(
                "{name} must not be blank"
            )));
        }
    }
    if case.sessions.is_empty() {
        return Err(ExternalMemoryError::InvalidDataset(
            "case must contain at least one session".to_string(),
        ));
    }

    let mut source_ids = HashSet::new();
    let mut session_ids = HashSet::new();
    let mut turn_ids = HashSet::new();
    let mut chronological_turns = Vec::new();
    for (session_source_order, session) in case.sessions.iter().enumerate() {
        validate_source_id(&session.source_id, &mut source_ids)?;
        session_ids.insert(session.source_id.as_str());
        if session.turns.is_empty() {
            return Err(ExternalMemoryError::InvalidDataset(format!(
                "session {} has no turns",
                session.source_id
            )));
        }
        for (turn_source_order, turn) in session.turns.iter().enumerate() {
            validate_source_id(&turn.source_id, &mut source_ids)?;
            turn_ids.insert(turn.source_id.as_str());
            if turn.role.trim().is_empty() || turn.text.trim().is_empty() {
                return Err(ExternalMemoryError::InvalidDataset(format!(
                    "turn {} must have a role and text",
                    turn.source_id
                )));
            }
            chronological_turns.push(ChronologicalTurn {
                session_source_id: session.source_id.clone(),
                turn_source_id: turn.source_id.clone(),
                occurred_at: turn.occurred_at,
                session_source_order,
                turn_source_order,
                role: turn.role.clone(),
                text: turn.text.clone(),
            });
        }
    }
    validate_label_references(
        "session",
        case.evidence_labels.session_source_ids.as_deref(),
        &session_ids,
    )?;
    validate_label_references(
        "turn",
        case.evidence_labels.turn_source_ids.as_deref(),
        &turn_ids,
    )?;
    chronological_turns.sort_by_key(|turn| {
        (
            turn.occurred_at,
            turn.session_source_order,
            turn.turn_source_order,
        )
    });
    Ok(PreparedExternalMemoryCase {
        case,
        chronological_turns,
    })
}

fn validate_source_id<'a>(source_id: &'a str, seen: &mut HashSet<&'a str>) -> Result<()> {
    if source_id.trim().is_empty() {
        return Err(ExternalMemoryError::InvalidDataset(
            "stable source id must not be blank".to_string(),
        ));
    }
    if !seen.insert(source_id) {
        return Err(ExternalMemoryError::InvalidDataset(format!(
            "duplicate stable source id `{source_id}`"
        )));
    }
    Ok(())
}

fn validate_label_references(
    level: &str,
    labels: Option<&[String]>,
    known: &HashSet<&str>,
) -> Result<()> {
    let Some(labels) = labels else {
        return Ok(());
    };
    let mut seen = HashSet::new();
    for source_id in labels {
        if !known.contains(source_id.as_str()) {
            return Err(ExternalMemoryError::InvalidDataset(format!(
                "unknown {level} evidence source id `{source_id}`"
            )));
        }
        if !seen.insert(source_id) {
            return Err(ExternalMemoryError::InvalidDataset(format!(
                "duplicate {level} evidence source id `{source_id}`"
            )));
        }
    }
    Ok(())
}

fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Provenance for one file in a verified dataset package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetFileProvenance {
    /// Package-relative path.
    pub path: String,
    /// Exact byte length.
    pub size_bytes: u64,
    /// Lowercase SHA-256 digest.
    pub sha256: String,
}

/// Immutable upstream repository provenance for a dataset package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetPackageSourceV1 {
    /// Upstream repository identifier.
    pub repository: String,
    /// Immutable upstream revision.
    pub revision: String,
}

/// Versioned provenance manifest for one dataset package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetPackageManifestV1 {
    /// Manifest schema version.
    pub schema_version: u32,
    /// Registry dataset identifier.
    pub dataset: String,
    /// Immutable upstream source.
    pub source: DatasetPackageSourceV1,
    /// Every byte-bearing file used by the loader.
    pub files: Vec<DatasetFileProvenance>,
}

impl DatasetPackageManifestV1 {
    /// Validates version, revision, file paths, lengths, and digests.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != DATASET_PACKAGE_SCHEMA_VERSION {
            return Err(ExternalMemoryError::InvalidDataset(format!(
                "unsupported package schema version {}",
                self.schema_version
            )));
        }
        if self.dataset.trim().is_empty()
            || self.source.repository.trim().is_empty()
            || self.source.revision.trim().is_empty()
        {
            return Err(ExternalMemoryError::InvalidDataset(
                "dataset, source repository, and source revision are required".to_string(),
            ));
        }
        if self.files.is_empty() {
            return Err(ExternalMemoryError::InvalidDataset(
                "package manifest must pin at least one file".to_string(),
            ));
        }
        let mut paths = HashSet::new();
        let mut previous_path: Option<&str> = None;
        for file in &self.files {
            let path = Path::new(&file.path);
            if file.path == "package.json"
                || path.is_absolute()
                || path.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir
                            | std::path::Component::CurDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
                || path.components().next().is_none()
            {
                return Err(ExternalMemoryError::InvalidDataset(format!(
                    "package file path must be relative: {}",
                    file.path
                )));
            }
            if previous_path.is_some_and(|previous| previous >= file.path.as_str()) {
                return Err(ExternalMemoryError::InvalidDataset(
                    "package files must be sorted by path".to_string(),
                ));
            }
            previous_path = Some(&file.path);
            if !paths.insert(file.path.as_str()) {
                return Err(ExternalMemoryError::InvalidDataset(format!(
                    "duplicate package file path: {}",
                    file.path
                )));
            }
            validate_sha256("file sha256", &file.sha256)?;
        }
        Ok(())
    }

    /// Hashes this manifest using the versioned canonical package contract.
    pub fn canonical_hash(&self) -> Result<String> {
        self.validate()?;
        let value = serde_json::to_value(self)?;
        Self::canonical_hash_value(&value)
    }

    /// Hashes a JSON manifest value with field-order-independent canonical serialization.
    pub fn canonical_hash_value(value: &serde_json::Value) -> Result<String> {
        let mut hasher = Sha256::new();
        hasher.update(DATASET_PACKAGE_HASH_DOMAIN);
        hasher.update(canonical_json_bytes(value)?);
        Ok(format!("{:x}", hasher.finalize()))
    }
}

/// Strict `package.json` wrapper for one canonical dataset manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetPackageV1 {
    /// Canonical inner manifest.
    pub manifest: DatasetPackageManifestV1,
    /// Domain-separated SHA-256 over only the canonical inner manifest.
    pub package_sha256: String,
}

/// Strict verified fetch summary shared by fetch and run commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum VerifiedFetchSummaryV1 {
    /// PersonaMem 32k package validation summary.
    PersonaMem(PersonaMemFetchSummaryV1),
    /// LongMemEval-S Cleaned package validation summary.
    LongMemEval(LongMemEvalFetchSummaryV1),
}

/// Strict PersonaMem fetch summary emitted only after package validation succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonaMemFetchSummaryV1 {
    /// Summary schema version.
    pub schema_version: u32,
    /// Dataset registry key.
    pub dataset: String,
    /// Upstream repository identifier.
    pub repository: String,
    /// Immutable upstream revision.
    pub revision: String,
    /// Canonical package digest.
    pub package_sha256: String,
    /// Validated question count.
    pub question_count: usize,
    /// Validated persona count.
    pub persona_count: usize,
    /// Validated shared-context count.
    pub context_count: usize,
    /// True only after full validation succeeds.
    pub verified: bool,
}

/// Strict LongMemEval fetch summary emitted only after package validation succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalFetchSummaryV1 {
    /// Summary schema version.
    pub schema_version: u32,
    /// Dataset registry key.
    pub dataset: String,
    /// Upstream repository identifier.
    pub repository: String,
    /// Immutable upstream revision.
    pub revision: String,
    /// Canonical package digest.
    pub package_sha256: String,
    /// Validated question count.
    pub question_count: usize,
    /// Validated abstention count.
    pub abstention_count: usize,
    /// Validated retrieval denominator.
    pub retrieval_count: usize,
    /// True only after full validation succeeds.
    pub verified: bool,
}

impl VerifiedFetchSummaryV1 {
    /// Returns the validated question count.
    #[must_use]
    pub fn question_count(&self) -> usize {
        match self {
            Self::PersonaMem(summary) => summary.question_count,
            Self::LongMemEval(summary) => summary.question_count,
        }
    }

    /// Validates shared package provenance before any runtime construction.
    pub fn validate_package(&self, package: &DatasetPackageV1) -> Result<()> {
        package.validate()?;
        let (schema_version, dataset, repository, revision, package_sha256, verified) = match self {
            Self::PersonaMem(summary) => (
                summary.schema_version,
                summary.dataset.as_str(),
                summary.repository.as_str(),
                summary.revision.as_str(),
                summary.package_sha256.as_str(),
                summary.verified,
            ),
            Self::LongMemEval(summary) => (
                summary.schema_version,
                summary.dataset.as_str(),
                summary.repository.as_str(),
                summary.revision.as_str(),
                summary.package_sha256.as_str(),
                summary.verified,
            ),
        };
        if schema_version != VERIFIED_FETCH_SUMMARY_SCHEMA_VERSION || !verified {
            return Err(ExternalMemoryError::InvalidDataset(
                "fetch summary must be verified schema version 1".to_string(),
            ));
        }
        if dataset != package.manifest.dataset
            || repository != package.manifest.source.repository
            || revision != package.manifest.source.revision
            || package_sha256 != package.package_sha256
        {
            return Err(ExternalMemoryError::InvalidDataset(
                "fetch summary provenance does not match package.json".to_string(),
            ));
        }
        Ok(())
    }
}

impl DatasetPackageV1 {
    /// Constructs a package wrapper with its canonical digest.
    pub fn new(manifest: DatasetPackageManifestV1) -> Result<Self> {
        let package_sha256 = manifest.canonical_hash()?;
        Ok(Self {
            manifest,
            package_sha256,
        })
    }

    /// Validates the strict manifest and its package digest.
    pub fn validate(&self) -> Result<()> {
        self.manifest.validate()?;
        validate_sha256("package_sha256", &self.package_sha256)?;
        let expected = self.manifest.canonical_hash()?;
        if self.package_sha256 != expected {
            return Err(ExternalMemoryError::InvalidDataset(format!(
                "package SHA-256 mismatch: expected {expected}, got {}",
                self.package_sha256
            )));
        }
        Ok(())
    }

    /// Verifies every pinned file beneath a package root.
    pub fn verify_files(&self, root: &Path) -> Result<()> {
        self.validate()?;
        for file in &self.manifest.files {
            let path = root.join(&file.path);
            let bytes = std::fs::read(&path)?;
            let size_bytes = u64::try_from(bytes.len()).map_err(|_| {
                ExternalMemoryError::InvalidDataset(format!(
                    "package file length does not fit u64: {}",
                    path.display()
                ))
            })?;
            if size_bytes != file.size_bytes {
                return Err(ExternalMemoryError::InvalidDataset(format!(
                    "package file {} length mismatch: expected {}, got {size_bytes}",
                    path.display(),
                    file.size_bytes
                )));
            }
            let sha256 = format!("{:x}", Sha256::digest(&bytes));
            if sha256 != file.sha256 {
                return Err(ExternalMemoryError::InvalidDataset(format!(
                    "package file {} SHA-256 mismatch: expected {}, got {sha256}",
                    path.display(),
                    file.sha256
                )));
            }
        }
        Ok(())
    }
}

/// Loads the Task-8 common-JSON case format from a package file.
pub fn load_common_json(path: &Path) -> Result<Vec<PreparedExternalMemoryCase>> {
    let bytes = std::fs::read(path)?;
    let cases: Vec<ExternalMemoryCaseV1> = serde_json::from_slice(&bytes)?;
    if cases.is_empty() {
        return Err(ExternalMemoryError::InvalidDataset(
            "common JSON package contains no cases".to_string(),
        ));
    }
    let mut isolation_keys = HashSet::new();
    cases
        .into_iter()
        .map(|case| {
            if !isolation_keys.insert(case.isolation_key.clone()) {
                return Err(ExternalMemoryError::InvalidDataset(format!(
                    "duplicate isolation key `{}`",
                    case.isolation_key
                )));
            }
            validate_case(case)
        })
        .collect()
}

/// Rejects a dataset package whose own content leaks its answers.
///
/// Two package-level leaks are checkable without any retrieval:
///
/// - **a duplicated case.** Two cases with the same question *and* the same
///   evidence are one case counted twice; if their answers also differ, the pair
///   is unsatisfiable. Note what is deliberately *allowed*: the same question
///   over different evidence with different answers, which is exactly how a
///   persona benchmark is built.
/// - **an in-package answer key.** A conversation turn that restates the question
///   *and* carries the gold answer hands the reader the pair directly. A turn that
///   merely contains the answer is the legitimate evidence the benchmark is built
///   from and must pass — that distinction is the whole point.
///
/// Fails closed: the first leak refuses the package rather than annotating it.
pub fn scan_package_leakage(cases: &[PreparedExternalMemoryCase]) -> Result<()> {
    let fingerprints = cases
        .iter()
        .map(|case| {
            (
                normalize(&case.case.question),
                evidence_fingerprint(case),
                normalize(&case.case.answer),
            )
        })
        .collect::<Vec<_>>();
    for (left_index, left) in cases.iter().enumerate() {
        for (right_index, right) in cases.iter().enumerate().skip(left_index + 1) {
            let (left_question, left_evidence, left_answer) = &fingerprints[left_index];
            let (right_question, right_evidence, right_answer) = &fingerprints[right_index];
            if left_evidence != right_evidence {
                continue;
            }
            if left_question == right_question {
                return Err(ExternalMemoryError::InvalidDataset(format!(
                    "cases `{}` and `{}` share the same question and the same evidence; \
                     a duplicated case scores one case against the other's labels",
                    left.case.isolation_key, right.case.isolation_key
                )));
            }
            if left_answer != right_answer {
                continue;
            }
            let similarity = jaccard(
                &shingles(&left.case.question),
                &shingles(&right.case.question),
            );
            if similarity >= DEFAULT_NEAR_DUPLICATE_JACCARD {
                return Err(ExternalMemoryError::InvalidDataset(format!(
                    "cases `{}` and `{}` are near-duplicates (question similarity \
                     {similarity:.3}) over identical evidence",
                    left.case.isolation_key, right.case.isolation_key
                )));
            }
        }
    }

    for case in cases {
        for turn in &case.chronological_turns {
            let turn_shingles = shingles(&turn.text);
            let question_containment = containment(&turn_shingles, &case.case.question);
            if question_containment < DEFAULT_QUESTION_RESTATEMENT_CONTAINMENT {
                continue;
            }
            let answer_containment = containment(&turn_shingles, &case.case.answer);
            if answer_containment >= DEFAULT_ANSWER_CONTAINMENT {
                return Err(ExternalMemoryError::InvalidDataset(format!(
                    "case `{}` turn `{}` restates the question (containment \
                     {question_containment:.3}) and carries its gold answer (containment \
                     {answer_containment:.3}); that is an answer key, not evidence",
                    case.case.isolation_key, turn.turn_source_id
                )));
            }
        }
    }
    Ok(())
}

/// Returns a normalized fingerprint of a case's evidence turns.
fn evidence_fingerprint(case: &PreparedExternalMemoryCase) -> String {
    case.chronological_turns
        .iter()
        .map(|turn| normalize(&turn.text))
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

/// Loader format selected by one package registry entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetPackageFormat {
    /// Task-8 backend-neutral JSON case array.
    CommonJson,
    /// PersonaMem v1 32k question CSV plus shared-context JSONL.
    PersonaMem32k,
    /// LongMemEval-S cleaned single-file JSON package.
    LongMemEvalSCleaned,
}

/// One named dataset loader registered independently of any backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetPackageRegistryEntry {
    /// Stable dataset identifier used by package manifests.
    pub dataset: String,
    /// Versioned loader format.
    pub format: DatasetPackageFormat,
}

/// Backend-neutral package loader registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasetPackageRegistry {
    entries: BTreeMap<String, DatasetPackageRegistryEntry>,
}

impl DatasetPackageRegistry {
    /// Creates the Task-8 registry containing only the common-JSON fixture loader.
    #[must_use]
    pub fn task_8() -> Self {
        let entry = DatasetPackageRegistryEntry {
            dataset: "common-json".to_string(),
            format: DatasetPackageFormat::CommonJson,
        };
        Self {
            entries: BTreeMap::from([(entry.dataset.clone(), entry)]),
        }
    }

    /// Creates the Task-9 registry with common JSON and pinned PersonaMem 32k loaders.
    #[must_use]
    pub fn task_9() -> Self {
        let mut registry = Self::task_8();
        let entry = DatasetPackageRegistryEntry {
            dataset: super::personamem::PERSONAMEM_DATASET.to_string(),
            format: DatasetPackageFormat::PersonaMem32k,
        };
        registry.entries.insert(entry.dataset.clone(), entry);
        registry
    }

    /// Creates the Task-10 registry with LongMemEval-S Cleaned included.
    #[must_use]
    pub fn task_10() -> Self {
        let mut registry = Self::task_9();
        let entry = DatasetPackageRegistryEntry {
            dataset: super::longmemeval::LONGMEMEVAL_DATASET.to_string(),
            format: DatasetPackageFormat::LongMemEvalSCleaned,
        };
        registry.entries.insert(entry.dataset.clone(), entry);
        registry
    }

    /// Returns one registered package loader entry.
    #[must_use]
    pub fn entry(&self, dataset: &str) -> Option<&DatasetPackageRegistryEntry> {
        self.entries.get(dataset)
    }

    /// Validates a manifest and loads its cases through the registered format.
    pub fn load(
        &self,
        package: &DatasetPackageV1,
        data_path: &Path,
    ) -> Result<Vec<PreparedExternalMemoryCase>> {
        package.validate()?;
        let manifest = &package.manifest;
        let entry = self.entry(&manifest.dataset).ok_or_else(|| {
            ExternalMemoryError::InvalidDataset(format!(
                "dataset `{}` is not registered",
                manifest.dataset
            ))
        })?;
        let cases = match entry.format {
            DatasetPackageFormat::CommonJson => load_common_json(data_path),
            DatasetPackageFormat::PersonaMem32k => {
                let dataset = super::personamem::load_full_personamem_package(package, data_path)?;
                Ok(dataset
                    .cases
                    .into_iter()
                    .map(|case| case.prepared)
                    .collect())
            }
            DatasetPackageFormat::LongMemEvalSCleaned => {
                let dataset =
                    super::longmemeval::load_full_longmemeval_package(package, data_path)?;
                Ok(dataset
                    .cases
                    .into_iter()
                    .map(|case| case.prepared)
                    .collect())
            }
        }?;
        // Package leakage is checked after loading and before any case is scored,
        // so an answer key or a duplicated question can never reach a run.
        scan_package_leakage(&cases)?;
        Ok(cases)
    }
}

fn validate_sha256(name: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ExternalMemoryError::InvalidDataset(format!(
            "{name} must be a lowercase SHA-256 digest"
        )));
    }
    Ok(())
}
