//! Strict PersonaMem v1 32k loading, projection, scoring, and reporting.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{BufRead, BufReader};
use std::path::Path;

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::answer::{AnswerScore, AnswerScoreOutcome, AnswerScorer, ReaderResponse, SupportStatus};
use super::dataset::{
    DatasetFileProvenance, DatasetPackage, DatasetPackageManifest, DatasetPackageSource,
    EvidenceLabels, ExternalMemoryCase, ExternalMemorySession, ExternalMemoryTurn,
    PreparedExternalMemoryCase, validate_case,
};
use super::{ExternalMemoryError, Result};
use crate::kernel::{
    BootstrapConfig, ClusterBootstrapReport, ClusterObservation, cluster_bootstrap_mean_by_user,
};

/// Stable registry identifier for the PersonaMem v1 32k lane.
pub const PERSONAMEM_DATASET: &str = "personamem-32k";
/// Pinned upstream dataset repository.
pub const PERSONAMEM_REPOSITORY: &str = "bowen-upenn/PersonaMem-v1";
/// Pinned immutable upstream revision.
pub const PERSONAMEM_REVISION: &str = "73dfd752d477d0c466cd441f1669397f5726d7ab";
/// Official question file name.
pub const PERSONAMEM_QUESTIONS_FILE: &str = "questions_32k.csv";
/// Official question file byte length.
pub const PERSONAMEM_QUESTIONS_SIZE_BYTES: u64 = 1_305_366;
/// Official question file SHA-256.
pub const PERSONAMEM_QUESTIONS_SHA256: &str =
    "cccd34cf53e0bc4d9536c04cff5ca045156d9a4e227e83327112482840bbc93c";
/// Official shared-context file name.
pub const PERSONAMEM_SHARED_CONTEXTS_FILE: &str = "shared_contexts_32k.jsonl";
/// Official shared-context file byte length.
pub const PERSONAMEM_SHARED_CONTEXTS_SIZE_BYTES: u64 = 5_613_210;
/// Official shared-context file SHA-256.
pub const PERSONAMEM_SHARED_CONTEXTS_SHA256: &str =
    "217247ebfec9e8442fc53570c795ab69f21aad08745f7de78d9beab51b122d4a";
/// Official full package SHA-256.
pub const PERSONAMEM_PACKAGE_SHA256: &str =
    "f4baf9ffa83a8452b5a026564eb439caa94334020d49be84510d392a88fe94ac";
/// Exact official question count.
pub const PERSONAMEM_QUESTION_COUNT: usize = 589;
/// Exact official persona count.
pub const PERSONAMEM_PERSONA_COUNT: usize = 20;
/// Exact official shared-context count.
pub const PERSONAMEM_CONTEXT_COUNT: usize = 37;

const PERSONAMEM_HEADERS: [&str; 15] = [
    "persona_id",
    "question_id",
    "question_type",
    "topic",
    "context_length_in_tokens",
    "context_length_in_letters",
    "distance_to_ref_in_blocks",
    "distance_to_ref_in_tokens",
    "num_irrelevant_tokens",
    "distance_to_ref_proportion_in_context",
    "user_question_or_message",
    "correct_answer",
    "all_options",
    "shared_context_id",
    "end_index_in_shared_context",
];

/// One parsed PersonaMem answer option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaMemOption {
    /// Canonical parenthesized option label.
    pub label: String,
    /// Option text excluding its label prefix.
    pub text: String,
}

/// Typed source metadata retained for one PersonaMem question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonaMemQuestionMetadata {
    /// Persona cluster identifier.
    pub persona_id: u32,
    /// Stable question identifier.
    pub question_id: String,
    /// Raw upstream question type.
    pub question_type: String,
    /// Raw upstream topic.
    pub topic: String,
    /// Source context token count.
    pub context_length_in_tokens: u64,
    /// Source context letter count.
    pub context_length_in_letters: u64,
    /// Exact source distance bucket.
    pub distance_to_ref_in_blocks: u32,
    /// Source token distance to the reference.
    pub distance_to_ref_in_tokens: u64,
    /// Source irrelevant-token count.
    pub num_irrelevant_tokens: u64,
    /// Source percentage text for proportional reference distance.
    pub distance_to_ref_proportion_in_context: String,
    /// Joined shared-context identifier.
    pub shared_context_id: String,
    /// End-exclusive source message index.
    pub end_index_in_shared_context: usize,
}

/// One source message occurrence in its projected logical turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaMemProjectedOccurrence {
    /// Original zero-based index before system records are excluded.
    pub original_index: usize,
    /// Logical turn index within the projected session.
    pub logical_turn_index: usize,
    /// Original non-system role.
    pub role: String,
    /// Original message content.
    pub content: String,
    /// Fixed-epoch occurrence timestamp derived from `original_index`.
    pub occurred_at: DateTime<Utc>,
}

/// One system-delimited PersonaMem session projection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaMemProjectedSession {
    /// Source session index among non-empty projected sessions.
    pub session_index: usize,
    /// Lossless non-system occurrences in source order.
    pub occurrences: Vec<PersonaMemProjectedOccurrence>,
}

/// Lossless logical projection retained alongside the generic ingest DTO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaMemHistoryProjection {
    /// Non-empty sessions split at excluded system records.
    pub sessions: Vec<PersonaMemProjectedSession>,
}

/// One prepared PersonaMem case with typed benchmark metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedPersonaMemCase {
    /// Generic backend-neutral ingest and query contract.
    pub prepared: PreparedExternalMemoryCase,
    /// Typed source question metadata.
    pub metadata: PersonaMemQuestionMetadata,
    /// Four validated ordered options.
    pub options: Vec<PersonaMemOption>,
    /// Logical grouping retained without collapsing message occurrences.
    pub history: PersonaMemHistoryProjection,
}

/// One strictly loaded PersonaMem package.
#[derive(Debug, Clone, PartialEq)]
pub struct PersonaMemDataset {
    /// Prepared question cases.
    pub cases: Vec<PreparedPersonaMemCase>,
    /// Number of unique joined context records in the source file.
    pub context_count: usize,
}

impl PersonaMemDataset {
    /// Returns the number of distinct persona clusters.
    #[must_use]
    pub fn persona_count(&self) -> usize {
        self.cases
            .iter()
            .map(|case| case.metadata.persona_id)
            .collect::<BTreeSet<_>>()
            .len()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonaMemQuestionRow {
    persona_id: u32,
    question_id: String,
    question_type: String,
    topic: String,
    context_length_in_tokens: u64,
    context_length_in_letters: u64,
    distance_to_ref_in_blocks: u32,
    distance_to_ref_in_tokens: u64,
    num_irrelevant_tokens: u64,
    distance_to_ref_proportion_in_context: String,
    user_question_or_message: String,
    correct_answer: String,
    all_options: String,
    shared_context_id: String,
    end_index_in_shared_context: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonaMemMessage {
    role: String,
    content: String,
}

/// Returns the exact pinned full-release manifest.
#[must_use]
pub fn official_personamem_manifest() -> DatasetPackageManifest {
    DatasetPackageManifest {
        schema_version: 1,
        dataset: PERSONAMEM_DATASET.to_string(),
        source: DatasetPackageSource {
            repository: PERSONAMEM_REPOSITORY.to_string(),
            revision: PERSONAMEM_REVISION.to_string(),
        },
        files: vec![
            DatasetFileProvenance {
                path: PERSONAMEM_QUESTIONS_FILE.to_string(),
                size_bytes: PERSONAMEM_QUESTIONS_SIZE_BYTES,
                sha256: PERSONAMEM_QUESTIONS_SHA256.to_string(),
            },
            DatasetFileProvenance {
                path: PERSONAMEM_SHARED_CONTEXTS_FILE.to_string(),
                size_bytes: PERSONAMEM_SHARED_CONTEXTS_SIZE_BYTES,
                sha256: PERSONAMEM_SHARED_CONTEXTS_SHA256.to_string(),
            },
        ],
    }
}

/// Strictly loads PersonaMem question CSV and shared-context JSONL files.
pub fn load_personamem_files(
    questions_path: &Path,
    contexts_path: &Path,
) -> Result<PersonaMemDataset> {
    let contexts = load_contexts(contexts_path)?;
    let mut reader = csv::ReaderBuilder::new()
        .flexible(false)
        .from_path(questions_path)
        .map_err(csv_error)?;
    let headers = reader.headers().map_err(csv_error)?;
    if headers.iter().ne(PERSONAMEM_HEADERS) {
        return Err(ExternalMemoryError::InvalidDataset(format!(
            "PersonaMem CSV header must be exactly {}",
            PERSONAMEM_HEADERS.join(",")
        )));
    }

    let mut question_ids = HashSet::new();
    let mut cases = Vec::new();
    for row in reader.deserialize::<PersonaMemQuestionRow>() {
        let row = row.map_err(csv_error)?;
        validate_question_row(&row, &mut question_ids)?;
        let options = parse_options(&row.all_options)?;
        if !options
            .iter()
            .any(|option| option.label == row.correct_answer)
        {
            return Err(ExternalMemoryError::InvalidDataset(format!(
                "question {} gold label {} is not one of its parsed options",
                row.question_id, row.correct_answer
            )));
        }
        let context = contexts.get(&row.shared_context_id).ok_or_else(|| {
            ExternalMemoryError::InvalidDataset(format!(
                "question {} references missing shared context {}",
                row.question_id, row.shared_context_id
            ))
        })?;
        if row.end_index_in_shared_context == 0 || row.end_index_in_shared_context > context.len() {
            return Err(ExternalMemoryError::InvalidDataset(format!(
                "question {} end_index_in_shared_context {} is outside 1..={}",
                row.question_id,
                row.end_index_in_shared_context,
                context.len()
            )));
        }

        let history = project_history(
            &row.shared_context_id,
            &context[..row.end_index_in_shared_context],
        )?;
        let external_sessions = external_sessions(&row.shared_context_id, &history);
        let external_case = ExternalMemoryCase {
            schema_version: 1,
            isolation_key: format!(
                "{PERSONAMEM_DATASET}/{PERSONAMEM_REVISION}/{}",
                row.question_id
            ),
            sessions: external_sessions,
            question: row.user_question_or_message.clone(),
            options: options
                .iter()
                .map(|option| format!("{} {}", option.label, option.text))
                .collect(),
            answer: row.correct_answer.clone(),
            category: row.question_type.clone(),
            evidence_labels: EvidenceLabels::default(),
        };
        let prepared = validate_case(external_case)?;
        cases.push(PreparedPersonaMemCase {
            prepared,
            metadata: PersonaMemQuestionMetadata {
                persona_id: row.persona_id,
                question_id: row.question_id,
                question_type: row.question_type,
                topic: row.topic,
                context_length_in_tokens: row.context_length_in_tokens,
                context_length_in_letters: row.context_length_in_letters,
                distance_to_ref_in_blocks: row.distance_to_ref_in_blocks,
                distance_to_ref_in_tokens: row.distance_to_ref_in_tokens,
                num_irrelevant_tokens: row.num_irrelevant_tokens,
                distance_to_ref_proportion_in_context: row.distance_to_ref_proportion_in_context,
                shared_context_id: row.shared_context_id,
                end_index_in_shared_context: row.end_index_in_shared_context,
            },
            options,
            history,
        });
    }
    if cases.is_empty() {
        return Err(ExternalMemoryError::InvalidDataset(
            "PersonaMem CSV contains no questions".to_string(),
        ));
    }
    Ok(PersonaMemDataset {
        cases,
        context_count: contexts.len(),
    })
}

/// Validates and loads the pinned complete PersonaMem 32k package.
pub fn load_full_personamem_package(
    package: &DatasetPackage,
    root: &Path,
) -> Result<PersonaMemDataset> {
    let expected_manifest = official_personamem_manifest();
    if package.manifest != expected_manifest || package.package_sha256 != PERSONAMEM_PACKAGE_SHA256
    {
        return Err(ExternalMemoryError::InvalidDataset(
            "PersonaMem package provenance does not match the pinned 32k release".to_string(),
        ));
    }
    package.verify_files(root)?;
    let dataset = load_personamem_files(
        &root.join(PERSONAMEM_QUESTIONS_FILE),
        &root.join(PERSONAMEM_SHARED_CONTEXTS_FILE),
    )?;
    let persona_ids = dataset
        .cases
        .iter()
        .map(|case| case.metadata.persona_id)
        .collect::<BTreeSet<_>>();
    let persona_count = u32::try_from(PERSONAMEM_PERSONA_COUNT).map_err(|_| {
        ExternalMemoryError::InvalidDataset("PersonaMem persona count does not fit u32".to_string())
    })?;
    let expected_personas = (0..persona_count).collect::<BTreeSet<_>>();
    if dataset.cases.len() != PERSONAMEM_QUESTION_COUNT
        || dataset.context_count != PERSONAMEM_CONTEXT_COUNT
        || persona_ids != expected_personas
    {
        return Err(ExternalMemoryError::InvalidDataset(format!(
            "PersonaMem release counts must be {PERSONAMEM_QUESTION_COUNT} questions / {PERSONAMEM_PERSONA_COUNT} personas / {PERSONAMEM_CONTEXT_COUNT} contexts; got {} / {} / {}",
            dataset.cases.len(),
            persona_ids.len(),
            dataset.context_count
        )));
    }
    Ok(dataset)
}

fn validate_question_row(
    row: &PersonaMemQuestionRow,
    question_ids: &mut HashSet<String>,
) -> Result<()> {
    for (name, value) in [
        ("question_id", row.question_id.as_str()),
        ("question_type", row.question_type.as_str()),
        ("topic", row.topic.as_str()),
        (
            "user_question_or_message",
            row.user_question_or_message.as_str(),
        ),
        ("correct_answer", row.correct_answer.as_str()),
        ("shared_context_id", row.shared_context_id.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(ExternalMemoryError::InvalidDataset(format!(
                "PersonaMem {name} must not be blank"
            )));
        }
    }
    if row.persona_id >= PERSONAMEM_PERSONA_COUNT as u32 {
        return Err(ExternalMemoryError::InvalidDataset(format!(
            "PersonaMem persona_id {} is outside 0..19",
            row.persona_id
        )));
    }
    if !(1..=7).contains(&row.distance_to_ref_in_blocks) {
        return Err(ExternalMemoryError::InvalidDataset(format!(
            "PersonaMem distance_to_ref_in_blocks {} is outside 1..=7",
            row.distance_to_ref_in_blocks
        )));
    }
    let Some(proportion) = row
        .distance_to_ref_proportion_in_context
        .strip_suffix('%')
        .and_then(|value| value.parse::<f64>().ok())
    else {
        return Err(ExternalMemoryError::InvalidDataset(
            "PersonaMem distance proportion must be typed percentage text".to_string(),
        ));
    };
    if !proportion.is_finite() || !(0.0..=100.0).contains(&proportion) {
        return Err(ExternalMemoryError::InvalidDataset(
            "PersonaMem distance proportion must be finite and within 0%..=100%".to_string(),
        ));
    }
    if !question_ids.insert(row.question_id.clone()) {
        return Err(ExternalMemoryError::InvalidDataset(format!(
            "duplicate PersonaMem question_id {}",
            row.question_id
        )));
    }
    Ok(())
}

fn load_contexts(path: &Path) -> Result<BTreeMap<String, Vec<PersonaMemMessage>>> {
    let file = std::fs::File::open(path)?;
    let mut contexts = BTreeMap::new();
    for (line_index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            return Err(ExternalMemoryError::InvalidDataset(format!(
                "PersonaMem context line {} must not be blank",
                line_index + 1
            )));
        }
        let record: BTreeMap<String, Vec<PersonaMemMessage>> = serde_json::from_str(&line)?;
        if record.len() != 1 {
            return Err(ExternalMemoryError::InvalidDataset(format!(
                "PersonaMem context line {} must be a one-key object",
                line_index + 1
            )));
        }
        let (context_id, messages) = record.into_iter().next().ok_or_else(|| {
            ExternalMemoryError::InvalidDataset("context record unexpectedly empty".to_string())
        })?;
        if context_id.trim().is_empty() || messages.is_empty() {
            return Err(ExternalMemoryError::InvalidDataset(
                "PersonaMem context ID and messages must not be empty".to_string(),
            ));
        }
        for message in &messages {
            if !matches!(message.role.as_str(), "system" | "user" | "assistant")
                || message.content.trim().is_empty()
            {
                return Err(ExternalMemoryError::InvalidDataset(format!(
                    "PersonaMem context {context_id} has an invalid role or blank content"
                )));
            }
        }
        if contexts.insert(context_id.clone(), messages).is_some() {
            return Err(ExternalMemoryError::InvalidDataset(format!(
                "duplicate PersonaMem shared context {context_id}"
            )));
        }
    }
    if contexts.is_empty() {
        return Err(ExternalMemoryError::InvalidDataset(
            "PersonaMem context JSONL contains no records".to_string(),
        ));
    }
    Ok(contexts)
}

fn project_history(
    context_id: &str,
    messages: &[PersonaMemMessage],
) -> Result<PersonaMemHistoryProjection> {
    let epoch = Utc
        .with_ymd_and_hms(2000, 1, 1, 0, 0, 0)
        .single()
        .ok_or_else(|| ExternalMemoryError::InvalidDataset("invalid fixed epoch".to_string()))?;
    let mut sessions = Vec::new();
    let mut current = Vec::new();
    let mut logical_turn_index = 0_usize;
    let mut has_logical_turn = false;
    for (original_index, message) in messages.iter().enumerate() {
        if message.role == "system" {
            push_projected_session(&mut sessions, &mut current);
            logical_turn_index = 0;
            has_logical_turn = false;
            continue;
        }
        if message.role == "user" {
            if has_logical_turn {
                logical_turn_index = logical_turn_index.saturating_add(1);
            } else {
                has_logical_turn = true;
            }
        } else if !has_logical_turn {
            has_logical_turn = true;
        }
        let seconds = i64::try_from(original_index).map_err(|_| {
            ExternalMemoryError::InvalidDataset(format!(
                "PersonaMem context {context_id} index does not fit i64"
            ))
        })?;
        current.push(PersonaMemProjectedOccurrence {
            original_index,
            logical_turn_index,
            role: message.role.clone(),
            content: message.content.clone(),
            occurred_at: epoch + Duration::seconds(seconds),
        });
    }
    push_projected_session(&mut sessions, &mut current);
    if sessions.is_empty() {
        return Err(ExternalMemoryError::InvalidDataset(format!(
            "PersonaMem context {context_id} slice contains no non-system messages"
        )));
    }
    Ok(PersonaMemHistoryProjection { sessions })
}

fn push_projected_session(
    sessions: &mut Vec<PersonaMemProjectedSession>,
    current: &mut Vec<PersonaMemProjectedOccurrence>,
) {
    if current.is_empty() {
        return;
    }
    sessions.push(PersonaMemProjectedSession {
        session_index: sessions.len(),
        occurrences: std::mem::take(current),
    });
}

fn external_sessions(
    context_id: &str,
    history: &PersonaMemHistoryProjection,
) -> Vec<ExternalMemorySession> {
    history
        .sessions
        .iter()
        .map(|session| ExternalMemorySession {
            source_id: format!("{context_id}/session-{:03}", session.session_index),
            occurred_at: session.occurrences[0].occurred_at,
            turns: session
                .occurrences
                .iter()
                .map(|occurrence| ExternalMemoryTurn {
                    source_id: format!("{context_id}/occurrence-{:06}", occurrence.original_index),
                    occurred_at: occurrence.occurred_at,
                    role: occurrence.role.clone(),
                    text: occurrence.content.clone(),
                })
                .collect(),
        })
        .collect()
}

fn parse_options(raw: &str) -> Result<Vec<PersonaMemOption>> {
    let values = serde_json::from_str::<Vec<String>>(raw)
        .or_else(|_| parse_python_string_list(raw).map_err(serde_json::Error::io))
        .map_err(|error| {
            ExternalMemoryError::InvalidDataset(format!(
                "PersonaMem all_options must be a quoted Python/JSON list: {error}"
            ))
        })?;
    if values.len() != 4 {
        return Err(ExternalMemoryError::InvalidDataset(
            "PersonaMem all_options must contain exactly four entries".to_string(),
        ));
    }
    let mut seen = HashSet::new();
    let mut seen_text = HashSet::new();
    let mut options = Vec::with_capacity(4);
    for (index, value) in values.into_iter().enumerate() {
        let expected_label = ["(a)", "(b)", "(c)", "(d)"][index].to_string();
        let Some(text) = value.strip_prefix(&expected_label) else {
            return Err(ExternalMemoryError::InvalidDataset(format!(
                "PersonaMem option {} must start with {expected_label}",
                index + 1
            )));
        };
        let text = text.trim();
        if text.is_empty() || !seen.insert(value.clone()) || !seen_text.insert(text.to_string()) {
            return Err(ExternalMemoryError::InvalidDataset(
                "PersonaMem options must be non-empty and unique".to_string(),
            ));
        }
        options.push(PersonaMemOption {
            label: expected_label,
            text: text.to_string(),
        });
    }
    Ok(options)
}

fn parse_python_string_list(raw: &str) -> std::io::Result<Vec<String>> {
    let bytes = raw.as_bytes();
    let mut index = 0_usize;
    skip_ascii_whitespace(bytes, &mut index);
    expect_byte(bytes, &mut index, b'[')?;
    let mut values = Vec::new();
    loop {
        skip_ascii_whitespace(bytes, &mut index);
        if consume_byte(bytes, &mut index, b']') {
            break;
        }
        let quote = *bytes.get(index).ok_or_else(invalid_python_list)?;
        if quote != b'\'' && quote != b'"' {
            return Err(invalid_python_list());
        }
        index += 1;
        let mut value = String::new();
        let mut closed = false;
        while let Some(byte) = bytes.get(index).copied() {
            index += 1;
            if byte == quote {
                closed = true;
                break;
            }
            if byte == b'\\' {
                let escaped = bytes.get(index).copied().ok_or_else(invalid_python_list)?;
                index += 1;
                value.push(match escaped {
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    b'\\' => '\\',
                    b'\'' => '\'',
                    b'"' => '"',
                    _ => return Err(invalid_python_list()),
                });
            } else if byte.is_ascii() {
                value.push(char::from(byte));
            } else {
                let start = index - 1;
                let character = raw[start..]
                    .chars()
                    .next()
                    .ok_or_else(invalid_python_list)?;
                value.push(character);
                index = start + character.len_utf8();
            }
        }
        if !closed {
            return Err(invalid_python_list());
        }
        values.push(value);
        skip_ascii_whitespace(bytes, &mut index);
        if consume_byte(bytes, &mut index, b',') {
            continue;
        }
        expect_byte(bytes, &mut index, b']')?;
        break;
    }
    skip_ascii_whitespace(bytes, &mut index);
    if index != bytes.len() {
        return Err(invalid_python_list());
    }
    Ok(values)
}

fn skip_ascii_whitespace(bytes: &[u8], index: &mut usize) {
    while bytes.get(*index).is_some_and(u8::is_ascii_whitespace) {
        *index += 1;
    }
}

fn consume_byte(bytes: &[u8], index: &mut usize, expected: u8) -> bool {
    if bytes.get(*index) == Some(&expected) {
        *index += 1;
        true
    } else {
        false
    }
}

fn expect_byte(bytes: &[u8], index: &mut usize, expected: u8) -> std::io::Result<()> {
    if consume_byte(bytes, index, expected) {
        Ok(())
    } else {
        Err(invalid_python_list())
    }
}

fn invalid_python_list() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid restricted Python string list",
    )
}

fn csv_error(error: csv::Error) -> ExternalMemoryError {
    ExternalMemoryError::InvalidDataset(format!("PersonaMem CSV: {error}"))
}

/// Versioned deterministic PersonaMem label-only scorer.
#[derive(Debug, Clone, Copy, Default)]
pub struct PersonaMemLabelScorer;

impl PersonaMemLabelScorer {
    /// Scores candidate text only when its distinct parenthesized label set is the gold singleton.
    #[must_use]
    pub fn score_text(gold_label: &str, candidate: &str) -> f64 {
        let lowered = candidate.to_ascii_lowercase();
        let bytes = lowered.as_bytes();
        let labels = bytes
            .windows(3)
            .filter(|window| {
                window[0] == b'(' && (b'a'..=b'd').contains(&window[1]) && window[2] == b')'
            })
            .map(|window| format!("({})", char::from(window[1])))
            .collect::<BTreeSet<_>>();
        f64::from(labels.len() == 1 && labels.contains(gold_label))
    }
}

impl AnswerScorer for PersonaMemLabelScorer {
    fn score(
        &self,
        case: &ExternalMemoryCase,
        answer: &ReaderResponse,
    ) -> std::result::Result<AnswerScoreOutcome, String> {
        Ok(AnswerScoreOutcome::Supported(AnswerScore {
            metric: "personamem_label_accuracy_v1".to_string(),
            value: Self::score_text(&case.answer, &answer.answer),
            denominator: 1,
        }))
    }
}

/// Retained terminal reader outcome for one PersonaMem question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "answer")]
pub enum PersonaMemAnswerOutcome {
    /// Reader text was available for deterministic label scoring.
    Answer(String),
    /// Provider invocation failed.
    ProviderFailure,
    /// Provider output could not be parsed by the caller.
    ParseFailure,
}

/// Exact numerator, denominator, and persona-cluster count for one slice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonaMemSliceReport {
    /// Correct answers in the slice.
    pub numerator: usize,
    /// All questions in the slice, including failures.
    pub denominator: usize,
    /// Distinct persona clusters represented by the slice.
    pub cluster_count: usize,
}

/// Versioned PersonaMem accuracy and slice report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonaMemAccuracyReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Versioned headline metric name.
    pub metric: String,
    /// Correct answers across all observations.
    pub numerator: usize,
    /// All questions, including ambiguous and failed outcomes.
    pub denominator: usize,
    /// Distinct persona clusters.
    pub cluster_count: usize,
    /// Persona-cluster bootstrap interval and deterministic configuration.
    pub bootstrap: ClusterBootstrapReport,
    /// Exact raw question-type slices.
    pub question_type_slices: BTreeMap<String, PersonaMemSliceReport>,
    /// Exact integer distance buckets.
    pub distance_slices: BTreeMap<u32, PersonaMemSliceReport>,
    /// Explicit retrieval-recall support status.
    pub retrieval_recall: SupportStatus,
}

/// Builds a full-denominator PersonaMem accuracy report clustered by persona.
pub fn build_accuracy_report(
    cases: &[PreparedPersonaMemCase],
    outcomes: &BTreeMap<String, PersonaMemAnswerOutcome>,
) -> Result<PersonaMemAccuracyReport> {
    if cases.is_empty() {
        return Err(ExternalMemoryError::InvalidDataset(
            "PersonaMem accuracy requires at least one case".to_string(),
        ));
    }
    let mut observations = Vec::with_capacity(cases.len());
    let mut numerator = 0_usize;
    let mut type_members = BTreeMap::<String, Vec<(u32, bool)>>::new();
    let mut distance_members = BTreeMap::<u32, Vec<(u32, bool)>>::new();
    for case in cases {
        let correct = matches!(
            outcomes.get(&case.metadata.question_id),
            Some(PersonaMemAnswerOutcome::Answer(answer))
                if PersonaMemLabelScorer::score_text(&case.prepared.case.answer, answer) == 1.0
        );
        numerator += usize::from(correct);
        observations.push(ClusterObservation {
            user_id: case.metadata.persona_id.to_string(),
            probe_id: case.metadata.question_id.clone(),
            value: f64::from(correct),
        });
        type_members
            .entry(case.metadata.question_type.clone())
            .or_default()
            .push((case.metadata.persona_id, correct));
        distance_members
            .entry(case.metadata.distance_to_ref_in_blocks)
            .or_default()
            .push((case.metadata.persona_id, correct));
    }
    let cluster_count = observations
        .iter()
        .map(|observation| observation.user_id.as_str())
        .collect::<HashSet<_>>()
        .len();
    Ok(PersonaMemAccuracyReport {
        schema_version: 1,
        metric: "personamem_label_accuracy_v1".to_string(),
        numerator,
        denominator: cases.len(),
        cluster_count,
        bootstrap: cluster_bootstrap_mean_by_user(
            "personamem_label_accuracy_v1",
            &observations,
            BootstrapConfig::default(),
        ),
        question_type_slices: type_members
            .into_iter()
            .map(|(name, members)| (name, slice_report(&members)))
            .collect(),
        distance_slices: distance_members
            .into_iter()
            .map(|(distance, members)| (distance, slice_report(&members)))
            .collect(),
        retrieval_recall: SupportStatus::Unsupported {
            reason: "PersonaMem v1 has no reliable evidence-reference labels".to_string(),
        },
    })
}

fn slice_report(members: &[(u32, bool)]) -> PersonaMemSliceReport {
    PersonaMemSliceReport {
        numerator: members.iter().filter(|(_, correct)| *correct).count(),
        denominator: members.len(),
        cluster_count: members
            .iter()
            .map(|(persona_id, _)| *persona_id)
            .collect::<HashSet<_>>()
            .len(),
    }
}

/// Strict provenance for counts in the tiny contract fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonaMemFixtureCounts {
    /// Fixture question count.
    pub questions: usize,
    /// Fixture persona count.
    pub personas: usize,
    /// Fixture context count.
    pub contexts: usize,
}

/// Strict provenance manifest for the tiny PersonaMem contract fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonaMemFixtureManifest {
    /// Fixture manifest schema version.
    pub schema_version: u32,
    /// Distinct fixture dataset identifier.
    pub dataset: String,
    /// Official source repository and revision.
    pub source: DatasetPackageSource,
    /// Official source file provenance.
    pub source_files: Vec<DatasetFileProvenance>,
    /// Tiny fixture file provenance.
    pub fixture_files: Vec<DatasetFileProvenance>,
    /// Synthetic fixture question IDs.
    pub selected_question_ids: Vec<String>,
    /// Why those rows exist.
    pub selection_rationale: String,
    /// Explicit derived-versus-synthetic status.
    pub content_origin: String,
    /// Exact tiny fixture counts.
    pub counts: PersonaMemFixtureCounts,
}

impl PersonaMemFixtureManifest {
    /// Validates official provenance, fixture bytes, selected IDs, and counts.
    pub fn validate(&self, root: &Path) -> Result<()> {
        if self.schema_version != 1
            || self.dataset != "personamem-32k-tiny"
            || self.source.repository != PERSONAMEM_REPOSITORY
            || self.source.revision != PERSONAMEM_REVISION
            || self.content_origin != "synthetic_contract_fixture"
            || self.selection_rationale.trim().is_empty()
        {
            return Err(ExternalMemoryError::InvalidDataset(
                "invalid PersonaMem tiny fixture provenance".to_string(),
            ));
        }
        if self.source_files != official_personamem_manifest().files {
            return Err(ExternalMemoryError::InvalidDataset(
                "PersonaMem fixture must pin the official source files".to_string(),
            ));
        }
        verify_provenance_files(root, &self.fixture_files)?;
        let dataset = load_personamem_files(
            &root.join("questions_32k_tiny.csv"),
            &root.join("shared_contexts_32k_tiny.jsonl"),
        )?;
        let question_ids = dataset
            .cases
            .iter()
            .map(|case| case.metadata.question_id.clone())
            .collect::<Vec<_>>();
        if question_ids != self.selected_question_ids
            || dataset.cases.len() != self.counts.questions
            || dataset.persona_count() != self.counts.personas
            || dataset.context_count != self.counts.contexts
        {
            return Err(ExternalMemoryError::InvalidDataset(
                "PersonaMem fixture selected IDs or counts do not match its files".to_string(),
            ));
        }
        Ok(())
    }
}

fn verify_provenance_files(root: &Path, files: &[DatasetFileProvenance]) -> Result<()> {
    for file in files {
        let bytes = std::fs::read(root.join(&file.path))?;
        let size_bytes = u64::try_from(bytes.len()).map_err(|_| {
            ExternalMemoryError::InvalidDataset("fixture size does not fit u64".to_string())
        })?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        if size_bytes != file.size_bytes || sha256 != file.sha256 {
            return Err(ExternalMemoryError::InvalidDataset(format!(
                "PersonaMem fixture file {} provenance mismatch",
                file.path
            )));
        }
    }
    Ok(())
}
