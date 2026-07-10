//! Strict LongMemEval-S cleaned loading, provenance, retrieval metrics, and judge rubrics.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::Path;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Number;
use sha2::{Digest, Sha256};

use super::answer::{AnswerScoreOutcome, AnswerScorer, ReaderResponse};
use super::dataset::{
    DatasetFileProvenance, DatasetPackageManifestV1, DatasetPackageSourceV1, DatasetPackageV1,
    EvidenceLabels, ExternalMemoryCaseV1, ExternalMemorySession, ExternalMemoryTurn,
    PreparedExternalMemoryCase, validate_case,
};
use super::{ExternalMemoryError, Result};
use crate::kernel::MetricSummary;

/// Stable registry identifier for the LongMemEval-S cleaned lane.
pub const LONGMEMEVAL_DATASET: &str = "longmemeval-s-cleaned";
/// Pinned Hugging Face dataset repository.
pub const LONGMEMEVAL_REPOSITORY: &str = "xiaowu0162/longmemeval-cleaned";
/// Pinned immutable Hugging Face dataset revision.
pub const LONGMEMEVAL_REVISION: &str = "98d7416c24c778c2fee6e6f3006e7a073259d48f";
/// Official LongMemEval-S cleaned source file name.
pub const LONGMEMEVAL_FILE: &str = "longmemeval_s_cleaned.json";
/// Official source file byte length.
pub const LONGMEMEVAL_FILE_SIZE_BYTES: u64 = 277_383_467;
/// Official source file SHA-256.
pub const LONGMEMEVAL_FILE_SHA256: &str =
    "d6f21ea9d60a0d56f34a05b609c79c88a451d2ae03597821ea3d5a9678c3a442";
/// Official package SHA-256 under `DatasetPackageManifestV1`.
pub const LONGMEMEVAL_PACKAGE_SHA256: &str =
    "620a9833c81011f8f29aa689f8bbf5242669f53eac5853a797330d3dafdedfff";
/// Exact official question count.
pub const LONGMEMEVAL_QUESTION_COUNT: usize = 500;
/// Exact official abstention count.
pub const LONGMEMEVAL_ABSTENTION_COUNT: usize = 30;
/// Exact official retrieval denominator.
pub const LONGMEMEVAL_RETRIEVAL_COUNT: usize = 470;
/// Pinned upstream evaluator repository.
pub const LONGMEMEVAL_EVALUATOR_REPOSITORY: &str = "xiaowu0162/LongMemEval";
/// Pinned upstream evaluator commit.
pub const LONGMEMEVAL_EVALUATOR_COMMIT: &str = "9e0b455f4ef0e2ab8f2e582289761153549043fc";
/// Pinned upstream evaluator source path.
pub const LONGMEMEVAL_EVALUATOR_SOURCE_PATH: &str = "src/evaluation/evaluate_qa.py";
/// SHA-256 of the pinned upstream evaluator source.
pub const LONGMEMEVAL_EVALUATOR_SOURCE_SHA256: &str =
    "ecce9c4c79dc89d99534ac17b383a5cbb5b9f0c69ee98adaf0684742e3d95251";
/// SHA-256 of the pinned upstream retrieval metric helper.
pub const LONGMEMEVAL_RETRIEVAL_UTILS_SHA256: &str =
    "c98b8d1096877a15aa755c9de44fe33c195298466a2eb6f3c0f9f6bde8c72349";
/// SHA-256 of the pinned upstream retrieval runner.
pub const LONGMEMEVAL_RETRIEVAL_RUNNER_SHA256: &str =
    "efd7fc5969a904717741fadca3c7dc73611ddbb2aaf3ef33117ebb6943b3e346";
/// Version of the vendored LongMemEval absolute-judge rubric bundle.
pub const LONGMEMEVAL_RUBRIC_VERSION: &str = "longmemeval-upstream-rubrics-v1";
/// Domain-separated SHA-256 of the exact five-rubric bundle.
pub const LONGMEMEVAL_RUBRIC_BUNDLE_SHA256: &str =
    "afad47661d42ee6baa8d9cbab24b514ce61dc825eadb0e4184080b479ce11f67";
/// Reason returned when deterministic answer scoring is requested for LongMemEval.
pub const LONGMEMEVAL_UNSUPPORTED_ANSWER_SCORE_REASON: &str = "longmemeval-requires-absolute-judge";

/// LongMemEval scorer marker that delegates correctness to the absolute judge.
#[derive(Debug, Clone, Copy, Default)]
pub struct LongMemEvalAnswerScorerV1;

impl AnswerScorer for LongMemEvalAnswerScorerV1 {
    fn score(
        &self,
        _case: &ExternalMemoryCaseV1,
        _answer: &ReaderResponse,
    ) -> std::result::Result<AnswerScoreOutcome, String> {
        Ok(AnswerScoreOutcome::Unsupported {
            reason: LONGMEMEVAL_UNSUPPORTED_ANSWER_SCORE_REASON.to_string(),
        })
    }
}

const DATE_FORMAT: &str = "%Y/%m/%d (%a) %H:%M";
const RUBRIC_DOMAIN: &[u8] = b"moa.longmemeval.rubrics.v1\0";

const GENERAL_RUBRIC: &str = "I will give you a question, a correct answer, and a response from a model. Please answer yes if the response contains the correct answer. Otherwise, answer no. If the response is equivalent to the correct answer or contains all the intermediate steps to get the correct answer, you should also answer yes. If the response only contains a subset of the information required by the answer, answer no. \n\nQuestion: {}\n\nCorrect Answer: {}\n\nModel Response: {}\n\nIs the model response correct? Answer yes or no only.";
const TEMPORAL_REASONING_RUBRIC: &str = "I will give you a question, a correct answer, and a response from a model. Please answer yes if the response contains the correct answer. Otherwise, answer no. If the response is equivalent to the correct answer or contains all the intermediate steps to get the correct answer, you should also answer yes. If the response only contains a subset of the information required by the answer, answer no. In addition, do not penalize off-by-one errors for the number of days. If the question asks for the number of days/weeks/months, etc., and the model makes off-by-one errors (e.g., predicting 19 days when the answer is 18), the model's response is still correct. \n\nQuestion: {}\n\nCorrect Answer: {}\n\nModel Response: {}\n\nIs the model response correct? Answer yes or no only.";
const KNOWLEDGE_UPDATE_RUBRIC: &str = "I will give you a question, a correct answer, and a response from a model. Please answer yes if the response contains the correct answer. Otherwise, answer no. If the response contains some previous information along with an updated answer, the response should be considered as correct as long as the updated answer is the required answer.\n\nQuestion: {}\n\nCorrect Answer: {}\n\nModel Response: {}\n\nIs the model response correct? Answer yes or no only.";
const SINGLE_SESSION_PREFERENCE_RUBRIC: &str = "I will give you a question, a rubric for desired personalized response, and a response from a model. Please answer yes if the response satisfies the desired response. Otherwise, answer no. The model does not need to reflect all the points in the rubric. The response is correct as long as it recalls and utilizes the user's personal information correctly.\n\nQuestion: {}\n\nRubric: {}\n\nModel Response: {}\n\nIs the model response correct? Answer yes or no only.";
const ABSTENTION_RUBRIC: &str = "I will give you an unanswerable question, an explanation, and a response from a model. Please answer yes if the model correctly identifies the question as unanswerable. The model could say that the information is incomplete, or some other information is given but the asked information is not.\n\nQuestion: {}\n\nExplanation: {}\n\nModel Response: {}\n\nDoes the model correctly identify the question as unanswerable? Answer yes or no only.";

/// One of the six official LongMemEval question categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum LongMemEvalQuestionType {
    /// A newer fact replaces an earlier fact.
    #[serde(rename = "knowledge-update")]
    KnowledgeUpdate,
    /// Evidence spans more than one session.
    #[serde(rename = "multi-session")]
    MultiSession,
    /// The relevant statement came from the assistant.
    #[serde(rename = "single-session-assistant")]
    SingleSessionAssistant,
    /// The answer is a personalized response rubric.
    #[serde(rename = "single-session-preference")]
    SingleSessionPreference,
    /// The relevant statement came from the user.
    #[serde(rename = "single-session-user")]
    SingleSessionUser,
    /// The answer requires temporal reasoning.
    #[serde(rename = "temporal-reasoning")]
    TemporalReasoning,
}

impl LongMemEvalQuestionType {
    /// All official categories in the published report order.
    pub const ALL: [Self; 6] = [
        Self::KnowledgeUpdate,
        Self::MultiSession,
        Self::SingleSessionAssistant,
        Self::SingleSessionPreference,
        Self::SingleSessionUser,
        Self::TemporalReasoning,
    ];

    /// Returns the exact upstream category spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::KnowledgeUpdate => "knowledge-update",
            Self::MultiSession => "multi-session",
            Self::SingleSessionAssistant => "single-session-assistant",
            Self::SingleSessionPreference => "single-session-preference",
            Self::SingleSessionUser => "single-session-user",
            Self::TemporalReasoning => "temporal-reasoning",
        }
    }
}

impl fmt::Display for LongMemEvalQuestionType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Typed metadata retained for one LongMemEval question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongMemEvalQuestionMetadata {
    /// Stable upstream question identifier.
    pub question_id: String,
    /// Official question category.
    pub question_type: LongMemEvalQuestionType,
    /// Deterministic UTC interpretation of the timezone-free question date.
    pub question_date: DateTime<Utc>,
}

/// Provenance for one source-session occurrence after chronological sorting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongMemEvalSessionProvenance {
    /// Stable occurrence-level source ID passed through MOA.
    pub source_id: String,
    /// Raw upstream session ID, which is not required to be unique.
    pub raw_session_id: String,
    /// Original source-array position before chronological sorting.
    pub original_session_index: usize,
    /// Deterministic UTC interpretation of the timezone-free session date.
    pub occurred_at: DateTime<Utc>,
}

/// Provenance for one source-turn occurrence after chronological sorting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongMemEvalTurnProvenance {
    /// Stable occurrence-level turn source ID.
    pub source_id: String,
    /// Stable containing session occurrence ID.
    pub session_source_id: String,
    /// Raw upstream session ID retained as metadata.
    pub raw_session_id: String,
    /// Original session position before chronological sorting.
    pub original_session_index: usize,
    /// Original turn position inside its source session.
    pub original_turn_index: usize,
    /// Whether the source explicitly marks this turn as answer evidence.
    pub has_answer: bool,
}

/// One prepared LongMemEval case with lossless source occurrence metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedLongMemEvalCase {
    /// Generic backend-neutral ingest and query contract.
    pub prepared: PreparedExternalMemoryCase,
    /// Typed question metadata.
    pub metadata: LongMemEvalQuestionMetadata,
    /// Chronologically sorted session occurrence provenance.
    pub session_provenance: Vec<LongMemEvalSessionProvenance>,
    /// Chronologically sorted turn occurrence provenance.
    pub turn_provenance: Vec<LongMemEvalTurnProvenance>,
    /// Whether `_abs` makes this an answer-only abstention case.
    pub is_abstention: bool,
}

/// One strictly loaded LongMemEval-S cleaned dataset.
#[derive(Debug, Clone, PartialEq)]
pub struct LongMemEvalDataset {
    /// Prepared question cases in upstream question order.
    pub cases: Vec<PreparedLongMemEvalCase>,
}

impl LongMemEvalDataset {
    /// Returns the case with the exact upstream question ID.
    #[must_use]
    pub fn case(&self, question_id: &str) -> Option<&PreparedLongMemEvalCase> {
        self.cases
            .iter()
            .find(|case| case.metadata.question_id == question_id)
    }

    /// Returns the number of `_abs` answer-only cases.
    #[must_use]
    pub fn abstention_count(&self) -> usize {
        self.cases.iter().filter(|case| case.is_abstention).count()
    }

    /// Returns the number of cases contributing retrieval metrics.
    #[must_use]
    pub fn retrieval_count(&self) -> usize {
        self.cases.len().saturating_sub(self.abstention_count())
    }

    /// Counts cases by their official question category.
    #[must_use]
    pub fn question_type_counts(&self) -> BTreeMap<LongMemEvalQuestionType, usize> {
        let mut counts = BTreeMap::new();
        for case in &self.cases {
            *counts.entry(case.metadata.question_type).or_default() += 1;
        }
        counts
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceQuestion {
    question_id: String,
    question_type: LongMemEvalQuestionType,
    question: String,
    question_date: String,
    answer: SourceAnswer,
    answer_session_ids: Vec<String>,
    haystack_dates: Vec<String>,
    haystack_session_ids: Vec<String>,
    haystack_sessions: Vec<Vec<SourceTurn>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SourceAnswer {
    String(String),
    Number(Number),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceTurn {
    role: SourceRole,
    content: String,
    #[serde(default)]
    has_answer: bool,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SourceRole {
    User,
    Assistant,
}

impl SourceRole {
    const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

struct SessionProjection {
    session: ExternalMemorySession,
    provenance: LongMemEvalSessionProvenance,
    turns: Vec<LongMemEvalTurnProvenance>,
}

/// Returns the pinned official LongMemEval-S package manifest.
#[must_use]
pub fn official_longmemeval_manifest() -> DatasetPackageManifestV1 {
    DatasetPackageManifestV1 {
        schema_version: 1,
        dataset: LONGMEMEVAL_DATASET.to_string(),
        source: DatasetPackageSourceV1 {
            repository: LONGMEMEVAL_REPOSITORY.to_string(),
            revision: LONGMEMEVAL_REVISION.to_string(),
        },
        files: vec![DatasetFileProvenance {
            path: LONGMEMEVAL_FILE.to_string(),
            size_bytes: LONGMEMEVAL_FILE_SIZE_BYTES,
            sha256: LONGMEMEVAL_FILE_SHA256.to_string(),
        }],
    }
}

/// Strictly loads one LongMemEval-S cleaned source JSON file.
pub fn load_longmemeval_file(path: &Path) -> Result<LongMemEvalDataset> {
    let bytes = std::fs::read(path)?;
    let questions: Vec<SourceQuestion> = serde_json::from_slice(&bytes)?;
    if questions.is_empty() {
        return Err(invalid_dataset("LongMemEval source contains no questions"));
    }
    let mut question_ids = HashSet::new();
    let mut cases = Vec::with_capacity(questions.len());
    for question in questions {
        validate_nonblank("question_id", &question.question_id)?;
        if !question_ids.insert(question.question_id.clone()) {
            return Err(invalid_dataset(format!(
                "duplicate question_id `{}`",
                question.question_id
            )));
        }
        cases.push(project_question(question)?);
    }
    Ok(LongMemEvalDataset { cases })
}

/// Validates and loads the pinned complete LongMemEval-S cleaned package.
pub fn load_full_longmemeval_package(
    package: &DatasetPackageV1,
    root: &Path,
) -> Result<LongMemEvalDataset> {
    if package.manifest != official_longmemeval_manifest()
        || package.package_sha256 != LONGMEMEVAL_PACKAGE_SHA256
    {
        return Err(invalid_dataset(
            "LongMemEval package provenance does not match the pinned S-cleaned release",
        ));
    }
    package.verify_files(root)?;
    let dataset = load_longmemeval_file(&root.join(LONGMEMEVAL_FILE))?;
    let expected_counts = BTreeMap::from([
        (LongMemEvalQuestionType::KnowledgeUpdate, 78),
        (LongMemEvalQuestionType::MultiSession, 133),
        (LongMemEvalQuestionType::SingleSessionAssistant, 56),
        (LongMemEvalQuestionType::SingleSessionPreference, 30),
        (LongMemEvalQuestionType::SingleSessionUser, 70),
        (LongMemEvalQuestionType::TemporalReasoning, 133),
    ]);
    if dataset.cases.len() != LONGMEMEVAL_QUESTION_COUNT
        || dataset.abstention_count() != LONGMEMEVAL_ABSTENTION_COUNT
        || dataset.retrieval_count() != LONGMEMEVAL_RETRIEVAL_COUNT
        || dataset.question_type_counts() != expected_counts
    {
        return Err(invalid_dataset(format!(
            "LongMemEval release counts must be {LONGMEMEVAL_QUESTION_COUNT} total / {LONGMEMEVAL_ABSTENTION_COUNT} abstention / {LONGMEMEVAL_RETRIEVAL_COUNT} retrieval with official type counts"
        )));
    }
    Ok(dataset)
}

fn project_question(question: SourceQuestion) -> Result<PreparedLongMemEvalCase> {
    for (name, value) in [
        ("question", question.question.as_str()),
        ("question_date", question.question_date.as_str()),
    ] {
        validate_nonblank(name, value)?;
    }
    let question_date = parse_exact_date("question_date", &question.question_date)?;
    let answer = match question.answer {
        SourceAnswer::String(answer) => {
            validate_nonblank("answer", &answer)?;
            answer
        }
        SourceAnswer::Number(answer) => answer.to_string(),
    };
    if question.haystack_dates.is_empty()
        || question.haystack_session_ids.is_empty()
        || question.haystack_sessions.is_empty()
    {
        return Err(invalid_dataset("haystack arrays must be non-empty"));
    }
    if question.haystack_dates.len() != question.haystack_session_ids.len()
        || question.haystack_dates.len() != question.haystack_sessions.len()
    {
        return Err(invalid_dataset(
            "haystack arrays must be non-empty and equal length",
        ));
    }
    for raw_id in &question.answer_session_ids {
        validate_nonblank("answer_session_ids entry", raw_id)?;
    }

    let mut projections = Vec::with_capacity(question.haystack_sessions.len());
    for (original_session_index, ((raw_date, raw_session_id), raw_turns)) in question
        .haystack_dates
        .iter()
        .zip(&question.haystack_session_ids)
        .zip(&question.haystack_sessions)
        .enumerate()
    {
        validate_nonblank("haystack_dates entry", raw_date)?;
        validate_nonblank("haystack_session_ids entry", raw_session_id)?;
        if raw_turns.is_empty() {
            return Err(invalid_dataset(format!(
                "haystack session {original_session_index} has no turns"
            )));
        }
        let occurred_at = parse_exact_date("haystack_dates entry", raw_date)?;
        let session_source_id = session_source_id(&question.question_id, original_session_index);
        let mut turns = Vec::with_capacity(raw_turns.len());
        let mut turn_provenance = Vec::with_capacity(raw_turns.len());
        for (original_turn_index, turn) in raw_turns.iter().enumerate() {
            validate_nonblank("turn content", &turn.content)?;
            let turn_source_id = turn_source_id(
                &question.question_id,
                original_session_index,
                original_turn_index,
            );
            turns.push(ExternalMemoryTurn {
                source_id: turn_source_id.clone(),
                occurred_at,
                role: turn.role.as_str().to_string(),
                text: turn.content.clone(),
            });
            turn_provenance.push(LongMemEvalTurnProvenance {
                source_id: turn_source_id,
                session_source_id: session_source_id.clone(),
                raw_session_id: raw_session_id.clone(),
                original_session_index,
                original_turn_index,
                has_answer: turn.has_answer,
            });
        }
        projections.push(SessionProjection {
            session: ExternalMemorySession {
                source_id: session_source_id.clone(),
                occurred_at,
                turns,
            },
            provenance: LongMemEvalSessionProvenance {
                source_id: session_source_id,
                raw_session_id: raw_session_id.clone(),
                original_session_index,
                occurred_at,
            },
            turns: turn_provenance,
        });
    }
    projections.sort_by_key(|projection| {
        (
            projection.provenance.occurred_at,
            projection.provenance.original_session_index,
        )
    });

    let is_abstention = question.question_id.contains("_abs");
    let raw_session_ids = projections
        .iter()
        .map(|projection| projection.provenance.raw_session_id.as_str())
        .collect::<HashSet<_>>();
    for answer_session_id in &question.answer_session_ids {
        if !raw_session_ids.contains(answer_session_id.as_str()) {
            return Err(invalid_dataset(format!(
                "answer_session_ids references missing raw session `{answer_session_id}` for {}",
                question.question_id
            )));
        }
    }

    let evidence_labels = if is_abstention {
        EvidenceLabels::default()
    } else {
        let answer_session_ids = question
            .answer_session_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let session_gold = projections
            .iter()
            .filter(|projection| {
                answer_session_ids.contains(projection.provenance.raw_session_id.as_str())
            })
            .map(|projection| projection.provenance.source_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let turn_gold = projections
            .iter()
            .flat_map(|projection| &projection.turns)
            .filter(|turn| turn.has_answer)
            .map(|turn| turn.source_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if session_gold.is_empty() || turn_gold.is_empty() {
            return Err(invalid_dataset(format!(
                "non-abstention question {} requires independent non-empty session and turn gold",
                question.question_id
            )));
        }
        EvidenceLabels {
            session_source_ids: Some(session_gold),
            turn_source_ids: Some(turn_gold),
        }
    };

    let sessions = projections
        .iter()
        .map(|projection| projection.session.clone())
        .collect();
    let session_provenance = projections
        .iter()
        .map(|projection| projection.provenance.clone())
        .collect();
    let turn_provenance = projections
        .iter()
        .flat_map(|projection| projection.turns.iter().cloned())
        .collect();
    let prepared = validate_case(ExternalMemoryCaseV1 {
        schema_version: 1,
        isolation_key: format!(
            "{LONGMEMEVAL_DATASET}/{LONGMEMEVAL_REVISION}/{}",
            question.question_id
        ),
        sessions,
        question: question.question,
        options: Vec::new(),
        answer,
        category: question.question_type.to_string(),
        evidence_labels,
    })?;
    Ok(PreparedLongMemEvalCase {
        prepared,
        metadata: LongMemEvalQuestionMetadata {
            question_id: question.question_id,
            question_type: question.question_type,
            question_date,
        },
        session_provenance,
        turn_provenance,
        is_abstention,
    })
}

fn parse_exact_date(field: &str, value: &str) -> Result<DateTime<Utc>> {
    let parsed = NaiveDateTime::parse_from_str(value, DATE_FORMAT).map_err(|error| {
        invalid_dataset(format!(
            "{field} must be exact YYYY/MM/DD (Dow) HH:MM with a consistent weekday: {error}"
        ))
    })?;
    if parsed.format(DATE_FORMAT).to_string() != value {
        return Err(invalid_dataset(format!(
            "{field} must be exact YYYY/MM/DD (Dow) HH:MM"
        )));
    }
    Ok(parsed.and_utc())
}

fn validate_nonblank(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(invalid_dataset(format!("{field} must not be blank")));
    }
    Ok(())
}

fn session_source_id(question_id: &str, session_index: usize) -> String {
    format!("longmemeval/{question_id}/session/{session_index}")
}

fn turn_source_id(question_id: &str, session_index: usize, turn_index: usize) -> String {
    format!(
        "{}/turn/{turn_index}",
        session_source_id(question_id, session_index)
    )
}

fn invalid_dataset(message: impl Into<String>) -> ExternalMemoryError {
    ExternalMemoryError::InvalidDataset(message.into())
}

/// One authoritative external occurrence in ranked retrieval order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalOccurrenceRef {
    /// Stable session occurrence source ID.
    pub session_source_id: String,
    /// Stable turn occurrence source ID.
    pub turn_source_id: String,
}

impl LongMemEvalOccurrenceRef {
    /// Builds one session/turn occurrence pair.
    pub fn new(session_source_id: impl Into<String>, turn_source_id: impl Into<String>) -> Self {
        Self {
            session_source_id: session_source_id.into(),
            turn_source_id: turn_source_id.into(),
        }
    }
}

/// Per-case recall-any, recall-all, and binary nDCG at one cutoff.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalCutoffScore {
    /// Whether the prefix contains any gold occurrence.
    pub recall_any: f64,
    /// Whether the prefix contains every distinct gold occurrence.
    pub recall_all: f64,
    /// Binary normalized discounted cumulative gain.
    pub ndcg: f64,
    /// Ranked occurrences scanned to form this prefix.
    pub scanned_occurrences: usize,
    /// Distinct occurrence IDs observed in this prefix.
    pub unique_occurrences: usize,
}

/// Complete per-case official LongMemEval retrieval metric vector.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalRetrievalCaseScore {
    /// Effective turn-to-session metrics at five unique sessions.
    pub session_at_5: LongMemEvalCutoffScore,
    /// Effective turn-to-session metrics at ten unique sessions.
    pub session_at_10: LongMemEvalCutoffScore,
    /// Direct turn metrics at rank five.
    pub turn_at_5: LongMemEvalCutoffScore,
    /// Direct turn metrics at rank ten.
    pub turn_at_10: LongMemEvalCutoffScore,
    /// Direct turn metrics at rank fifty.
    pub turn_at_50: LongMemEvalCutoffScore,
}

/// Aggregate recall-any, recall-all, and nDCG summaries for one cutoff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalCutoffMetrics {
    /// Mean recall-any with its complete denominator.
    pub recall_any: MetricSummary,
    /// Mean recall-all with its complete denominator.
    pub recall_all: MetricSummary,
    /// Mean binary nDCG with its complete denominator.
    pub ndcg: MetricSummary,
}

/// Aggregate official retrieval metrics for all contributing cases or one type slice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalRetrievalSliceV1 {
    /// Number of retrieval cases, including missing or failed rankings.
    pub denominator: usize,
    /// Effective turn-to-session metrics at five unique sessions.
    pub session_at_5: LongMemEvalCutoffMetrics,
    /// Effective turn-to-session metrics at ten unique sessions.
    pub session_at_10: LongMemEvalCutoffMetrics,
    /// Direct turn metrics at rank five.
    pub turn_at_5: LongMemEvalCutoffMetrics,
    /// Direct turn metrics at rank ten.
    pub turn_at_10: LongMemEvalCutoffMetrics,
    /// Direct turn metrics at rank fifty.
    pub turn_at_50: LongMemEvalCutoffMetrics,
}

/// Versioned LongMemEval retrieval report with official type slices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalRetrievalMetricsV1 {
    /// Report schema version.
    pub schema_version: u32,
    /// Exact retrieval denominator, including failed and absent rankings.
    pub denominator: usize,
    /// Effective turn-to-session metrics at five unique sessions.
    pub session_at_5: LongMemEvalCutoffMetrics,
    /// Effective turn-to-session metrics at ten unique sessions.
    pub session_at_10: LongMemEvalCutoffMetrics,
    /// Direct turn metrics at rank five.
    pub turn_at_5: LongMemEvalCutoffMetrics,
    /// Direct turn metrics at rank ten.
    pub turn_at_10: LongMemEvalCutoffMetrics,
    /// Direct turn metrics at rank fifty.
    pub turn_at_50: LongMemEvalCutoffMetrics,
    /// Retrieval metrics sliced by all six official categories.
    pub question_type_slices: BTreeMap<LongMemEvalQuestionType, LongMemEvalRetrievalSliceV1>,
}

/// Scores one non-abstention case in authoritative ranked occurrence order.
pub fn score_retrieval_case(
    case: &PreparedLongMemEvalCase,
    ranked: &[LongMemEvalOccurrenceRef],
) -> Result<LongMemEvalRetrievalCaseScore> {
    if case.is_abstention {
        return Err(invalid_dataset(format!(
            "abstention case {} does not contribute retrieval metrics",
            case.metadata.question_id
        )));
    }
    let turn_to_session = case
        .turn_provenance
        .iter()
        .map(|turn| (turn.source_id.as_str(), turn.session_source_id.as_str()))
        .collect::<HashMap<_, _>>();
    let mut seen_turns = HashSet::new();
    for occurrence in ranked {
        if !seen_turns.insert(occurrence.turn_source_id.as_str()) {
            return Err(invalid_dataset(format!(
                "duplicate ranked turn occurrence `{}`",
                occurrence.turn_source_id
            )));
        }
        let expected_session = turn_to_session
            .get(occurrence.turn_source_id.as_str())
            .ok_or_else(|| {
                invalid_dataset(format!(
                    "ranked turn occurrence `{}` is outside question {}",
                    occurrence.turn_source_id, case.metadata.question_id
                ))
            })?;
        if *expected_session != occurrence.session_source_id {
            return Err(invalid_dataset(format!(
                "ranked turn occurrence `{}` belongs to session `{expected_session}`, not `{}`",
                occurrence.turn_source_id, occurrence.session_source_id
            )));
        }
    }

    let session_gold = gold_set(
        "session",
        case.prepared
            .case
            .evidence_labels
            .session_source_ids
            .as_deref(),
    )?;
    let turn_gold = gold_set(
        "turn",
        case.prepared
            .case
            .evidence_labels
            .turn_source_ids
            .as_deref(),
    )?;
    let ranked_turns = ranked
        .iter()
        .map(|occurrence| occurrence.turn_source_id.as_str())
        .collect::<Vec<_>>();
    let full_turn_corpus = case
        .turn_provenance
        .iter()
        .map(|turn| turn.source_id.as_str())
        .collect::<Vec<_>>();
    let ranked_sessions = ranked
        .iter()
        .map(|occurrence| occurrence.session_source_id.as_str())
        .collect::<Vec<_>>();
    let full_session_corpus = case
        .turn_provenance
        .iter()
        .map(|turn| turn.session_source_id.as_str())
        .collect::<Vec<_>>();

    Ok(LongMemEvalRetrievalCaseScore {
        session_at_5: effective_session_score(
            &ranked_sessions,
            &full_session_corpus,
            &session_gold,
            5,
        ),
        session_at_10: effective_session_score(
            &ranked_sessions,
            &full_session_corpus,
            &session_gold,
            10,
        ),
        turn_at_5: direct_score(&ranked_turns, &full_turn_corpus, &turn_gold, 5),
        turn_at_10: direct_score(&ranked_turns, &full_turn_corpus, &turn_gold, 10),
        turn_at_50: direct_score(&ranked_turns, &full_turn_corpus, &turn_gold, 50),
    })
}

/// Aggregates retrieval outcomes without dropping missing or failed case rankings.
pub fn aggregate_retrieval_metrics(
    cases: &[PreparedLongMemEvalCase],
    rankings: &BTreeMap<String, Vec<LongMemEvalOccurrenceRef>>,
) -> Result<LongMemEvalRetrievalMetricsV1> {
    let known_ids = cases
        .iter()
        .map(|case| case.metadata.question_id.as_str())
        .collect::<HashSet<_>>();
    if let Some(unknown) = rankings
        .keys()
        .find(|question_id| !known_ids.contains(question_id.as_str()))
    {
        return Err(invalid_dataset(format!(
            "retrieval ranking references unknown question_id `{unknown}`"
        )));
    }
    let mut overall = Vec::new();
    let mut slices = BTreeMap::<LongMemEvalQuestionType, Vec<LongMemEvalRetrievalCaseScore>>::new();
    for question_type in LongMemEvalQuestionType::ALL {
        slices.insert(question_type, Vec::new());
    }
    for case in cases.iter().filter(|case| !case.is_abstention) {
        let ranked = rankings
            .get(&case.metadata.question_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let score = score_retrieval_case(case, ranked)?;
        overall.push(score);
        if let Some(slice) = slices.get_mut(&case.metadata.question_type) {
            slice.push(score);
        }
    }
    let overall_summary = aggregate_scores(&overall);
    Ok(LongMemEvalRetrievalMetricsV1 {
        schema_version: 1,
        denominator: overall_summary.denominator,
        session_at_5: overall_summary.session_at_5,
        session_at_10: overall_summary.session_at_10,
        turn_at_5: overall_summary.turn_at_5,
        turn_at_10: overall_summary.turn_at_10,
        turn_at_50: overall_summary.turn_at_50,
        question_type_slices: slices
            .into_iter()
            .map(|(question_type, scores)| (question_type, aggregate_scores(&scores)))
            .collect(),
    })
}

fn gold_set<'a>(level: &str, gold: Option<&'a [String]>) -> Result<HashSet<&'a str>> {
    let values = gold.ok_or_else(|| invalid_dataset(format!("missing {level} gold labels")))?;
    let set = values.iter().map(String::as_str).collect::<HashSet<_>>();
    if set.is_empty() {
        return Err(invalid_dataset(format!("empty {level} gold labels")));
    }
    Ok(set)
}

fn direct_score(
    ranked: &[&str],
    full_corpus: &[&str],
    gold: &HashSet<&str>,
    cutoff: usize,
) -> LongMemEvalCutoffScore {
    cutoff_score(ranked, full_corpus, gold, cutoff)
}

fn effective_session_score(
    ranked_sessions: &[&str],
    full_corpus: &[&str],
    gold: &HashSet<&str>,
    unique_cutoff: usize,
) -> LongMemEvalCutoffScore {
    let mut unique = HashSet::new();
    let mut scanned = 0;
    for session in ranked_sessions {
        scanned += 1;
        unique.insert(*session);
        if unique.len() == unique_cutoff {
            break;
        }
    }
    cutoff_score(
        ranked_sessions,
        full_corpus,
        gold,
        scanned.max(unique_cutoff),
    )
}

fn cutoff_score(
    ranked: &[&str],
    full_corpus: &[&str],
    gold: &HashSet<&str>,
    cutoff: usize,
) -> LongMemEvalCutoffScore {
    let prefix = &ranked[..ranked.len().min(cutoff)];
    let prefix_set = prefix.iter().copied().collect::<HashSet<_>>();
    let relevance = prefix
        .iter()
        .map(|occurrence| usize::from(gold.contains(occurrence)))
        .collect::<Vec<_>>();
    let mut ideal_relevance = full_corpus
        .iter()
        .map(|occurrence| usize::from(gold.contains(occurrence)))
        .collect::<Vec<_>>();
    ideal_relevance.sort_unstable_by(|left, right| right.cmp(left));
    ideal_relevance.truncate(cutoff);
    let dcg = binary_dcg(&relevance);
    let idcg = binary_dcg(&ideal_relevance);
    LongMemEvalCutoffScore {
        recall_any: if prefix_set.iter().any(|value| gold.contains(value)) {
            1.0
        } else {
            0.0
        },
        recall_all: if gold.iter().all(|value| prefix_set.contains(value)) {
            1.0
        } else {
            0.0
        },
        ndcg: if idcg == 0.0 { 0.0 } else { dcg / idcg },
        scanned_occurrences: prefix.len(),
        unique_occurrences: prefix_set.len(),
    }
}

fn binary_dcg(relevance: &[usize]) -> f64 {
    let Some((first, tail)) = relevance.split_first() else {
        return 0.0;
    };
    *first as f64
        + tail
            .iter()
            .enumerate()
            .map(|(tail_index, relevance)| {
                let rank_index = tail_index + 1;
                *relevance as f64 / (rank_index as f64 + 1.0).log2()
            })
            .sum::<f64>()
}

fn aggregate_scores(scores: &[LongMemEvalRetrievalCaseScore]) -> LongMemEvalRetrievalSliceV1 {
    LongMemEvalRetrievalSliceV1 {
        denominator: scores.len(),
        session_at_5: aggregate_cutoff(scores.iter().map(|score| score.session_at_5), scores.len()),
        session_at_10: aggregate_cutoff(
            scores.iter().map(|score| score.session_at_10),
            scores.len(),
        ),
        turn_at_5: aggregate_cutoff(scores.iter().map(|score| score.turn_at_5), scores.len()),
        turn_at_10: aggregate_cutoff(scores.iter().map(|score| score.turn_at_10), scores.len()),
        turn_at_50: aggregate_cutoff(scores.iter().map(|score| score.turn_at_50), scores.len()),
    }
}

fn aggregate_cutoff(
    scores: impl Iterator<Item = LongMemEvalCutoffScore>,
    denominator: usize,
) -> LongMemEvalCutoffMetrics {
    let (recall_any, recall_all, ndcg) = scores.fold((0.0, 0.0, 0.0), |totals, score| {
        (
            totals.0 + score.recall_any,
            totals.1 + score.recall_all,
            totals.2 + score.ndcg,
        )
    });
    LongMemEvalCutoffMetrics {
        recall_any: MetricSummary::from_total(recall_any, denominator),
        recall_all: MetricSummary::from_total(recall_all, denominator),
        ndcg: MetricSummary::from_total(ndcg, denominator),
    }
}

/// One of the five exact upstream absolute-judge rubric templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LongMemEvalRubricKind {
    /// General factual-answer rubric.
    General,
    /// Temporal-reasoning rubric with upstream off-by-one tolerance.
    TemporalReasoning,
    /// Knowledge-update rubric that accepts old context alongside the current answer.
    KnowledgeUpdate,
    /// Personalized-response rubric.
    SingleSessionPreference,
    /// Unanswerable-question rubric.
    Abstention,
}

impl LongMemEvalRubricKind {
    /// All rubric names in lexicographic order for canonical bundle hashing.
    pub const ALL: [Self; 5] = [
        Self::Abstention,
        Self::General,
        Self::KnowledgeUpdate,
        Self::SingleSessionPreference,
        Self::TemporalReasoning,
    ];

    /// Returns the exact canonical rubric name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::TemporalReasoning => "temporal_reasoning",
            Self::KnowledgeUpdate => "knowledge_update",
            Self::SingleSessionPreference => "single_session_preference",
            Self::Abstention => "abstention",
        }
    }

    /// Returns the exact vendored upstream template bytes.
    #[must_use]
    pub const fn template(self) -> &'static str {
        match self {
            Self::General => GENERAL_RUBRIC,
            Self::TemporalReasoning => TEMPORAL_REASONING_RUBRIC,
            Self::KnowledgeUpdate => KNOWLEDGE_UPDATE_RUBRIC,
            Self::SingleSessionPreference => SINGLE_SESSION_PREFERENCE_RUBRIC,
            Self::Abstention => ABSTENTION_RUBRIC,
        }
    }

    /// Returns the pinned SHA-256 of the exact template bytes.
    #[must_use]
    pub const fn sha256(self) -> &'static str {
        match self {
            Self::General => "fba020ba3d57982efdc9a937c1c01f897b789a608c7f88e60244121f6505e5bc",
            Self::TemporalReasoning => {
                "8d33a5fdd83afeeb4592454a965eab43d1fcb2dedc042d1d3892f4254be6c273"
            }
            Self::KnowledgeUpdate => {
                "183a9b3a6197ec620940f610cdc1207201ec98c1113dd633ea685cfc322fafac"
            }
            Self::SingleSessionPreference => {
                "741ee3bcbea7ff5e8ed359acef61d2f8ded3de021bbcff6ee13de455f2e2aa9b"
            }
            Self::Abstention => "5c0b365a1e1d06db36377c735432b56e122ca3c428f89faf61d43a0d5a7e050b",
        }
    }

    /// Computes the SHA-256 of the vendored template bytes.
    #[must_use]
    pub fn computed_sha256(self) -> String {
        format!("{:x}", Sha256::digest(self.template().as_bytes()))
    }

    /// Selects the upstream rubric for one category and abstention state.
    #[must_use]
    pub const fn for_question(question_type: LongMemEvalQuestionType, is_abstention: bool) -> Self {
        if is_abstention {
            return Self::Abstention;
        }
        match question_type {
            LongMemEvalQuestionType::TemporalReasoning => Self::TemporalReasoning,
            LongMemEvalQuestionType::KnowledgeUpdate => Self::KnowledgeUpdate,
            LongMemEvalQuestionType::SingleSessionPreference => Self::SingleSessionPreference,
            LongMemEvalQuestionType::MultiSession
            | LongMemEvalQuestionType::SingleSessionAssistant
            | LongMemEvalQuestionType::SingleSessionUser => Self::General,
        }
    }

    /// Renders the exact upstream template without interpreting braces inside input values.
    pub fn render(self, question: &str, answer: &str, response: &str) -> Result<String> {
        let parts = self.template().split("{}").collect::<Vec<_>>();
        if parts.len() != 4 {
            return Err(invalid_dataset(format!(
                "rubric {} must contain exactly three placeholders",
                self.as_str()
            )));
        }
        Ok([
            parts[0], question, parts[1], answer, parts[2], response, parts[3],
        ]
        .concat())
    }
}

impl fmt::Display for LongMemEvalRubricKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Computes the domain-separated canonical digest of all five exact rubrics.
pub fn rubric_bundle_sha256() -> Result<String> {
    let rubrics = LongMemEvalRubricKind::ALL
        .into_iter()
        .map(|kind| (kind.as_str(), kind.template()))
        .collect::<BTreeMap<_, _>>();
    let compact_json = serde_json::to_vec(&rubrics)?;
    let mut digest = Sha256::new();
    digest.update(RUBRIC_DOMAIN);
    digest.update(compact_json);
    Ok(format!("{:x}", digest.finalize()))
}

/// Parses a judge response using trimmed, case-insensitive exact yes/no matching.
#[must_use]
pub fn parse_absolute_judge_label(response: &str) -> Option<bool> {
    let response = response.trim();
    if response.eq_ignore_ascii_case("yes") {
        Some(true)
    } else if response.eq_ignore_ascii_case("no") {
        Some(false)
    } else {
        None
    }
}

/// One exact template and digest recorded in the committed evaluator contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalRubricContractV1 {
    /// Exact vendored template including its three `{}` placeholders.
    pub template: String,
    /// SHA-256 of the exact UTF-8 template bytes.
    pub sha256: String,
}

/// One pinned upstream source file and digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalSourceContractV1 {
    /// Repository-relative upstream source path.
    pub path: String,
    /// SHA-256 of the pinned source bytes.
    pub sha256: String,
}

/// Deliberate judge-parser compatibility rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalParserContractV1 {
    /// Upstream behavior retained for audit comparison.
    pub upstream: String,
    /// Hardened MOA behavior used by the runner.
    pub moa: String,
    /// Inputs accepted as correct.
    pub accepted_true: Vec<String>,
    /// Inputs accepted as incorrect.
    pub accepted_false: Vec<String>,
    /// Representative retained parse failures.
    pub rejected: Vec<String>,
}

/// One deterministic retrieval compatibility vector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalRetrievalContractVectorV1 {
    /// Stable vector name.
    pub name: String,
    /// Ranked mapped occurrence IDs.
    pub ranked: Vec<String>,
    /// Full corpus mapped occurrence IDs for IDCG.
    pub corpus: Vec<String>,
    /// Gold occurrence IDs.
    pub gold: Vec<String>,
    /// Direct rank cutoff or unique-session target.
    pub cutoff: usize,
    /// Whether the cutoff uses effective unique-session scanning.
    pub effective_unique: bool,
    /// Expected number of ranked occurrences scanned.
    pub expected_scanned: usize,
    /// Expected recall-any result.
    pub expected_recall_any: f64,
    /// Expected recall-all result.
    pub expected_recall_all: f64,
    /// Expected binary nDCG result.
    pub expected_ndcg: f64,
}

/// Strict, self-contained compatibility contract for the pinned upstream evaluator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalUpstreamContractV1 {
    /// Contract schema version.
    pub schema_version: u32,
    /// Pinned evaluator repository.
    pub evaluator_repository: String,
    /// Pinned evaluator commit.
    pub evaluator_commit: String,
    /// Exact evaluator source path.
    pub evaluator_source_path: String,
    /// Exact evaluator source SHA-256.
    pub evaluator_source_sha256: String,
    /// Pinned retrieval source files and hashes.
    pub retrieval_sources: Vec<LongMemEvalSourceContractV1>,
    /// Exact rubric templates and hashes keyed by canonical name.
    pub rubrics: BTreeMap<LongMemEvalRubricKind, LongMemEvalRubricContractV1>,
    /// Domain-separated digest over the canonical rubric map.
    pub bundle_sha256: String,
    /// Official type-to-rubric mapping for non-abstention questions.
    pub type_mapping: BTreeMap<LongMemEvalQuestionType, LongMemEvalRubricKind>,
    /// Question-ID marker that overrides type mapping for abstentions.
    pub abstention_question_id_marker: String,
    /// Retrieval math and effective-cutoff compatibility vectors.
    pub retrieval_vectors: Vec<LongMemEvalRetrievalContractVectorV1>,
    /// Deliberately hardened judge parser contract.
    pub parser: LongMemEvalParserContractV1,
}

/// Loads and fully validates the committed upstream evaluator compatibility contract.
pub fn load_upstream_contract(path: &Path) -> Result<LongMemEvalUpstreamContractV1> {
    let bytes = std::fs::read(path)?;
    let contract: LongMemEvalUpstreamContractV1 = serde_json::from_slice(&bytes)?;
    validate_upstream_contract(&contract)?;
    Ok(contract)
}

fn validate_upstream_contract(contract: &LongMemEvalUpstreamContractV1) -> Result<()> {
    if contract.schema_version != 1
        || contract.evaluator_repository != LONGMEMEVAL_EVALUATOR_REPOSITORY
        || contract.evaluator_commit != LONGMEMEVAL_EVALUATOR_COMMIT
        || contract.evaluator_source_path != LONGMEMEVAL_EVALUATOR_SOURCE_PATH
        || contract.evaluator_source_sha256 != LONGMEMEVAL_EVALUATOR_SOURCE_SHA256
        || contract.bundle_sha256 != LONGMEMEVAL_RUBRIC_BUNDLE_SHA256
        || contract.abstention_question_id_marker != "_abs"
    {
        return Err(invalid_dataset(
            "LongMemEval upstream contract provenance or bundle identity is invalid",
        ));
    }
    let expected_sources = vec![
        LongMemEvalSourceContractV1 {
            path: "src/retrieval/eval_utils.py".to_string(),
            sha256: LONGMEMEVAL_RETRIEVAL_UTILS_SHA256.to_string(),
        },
        LongMemEvalSourceContractV1 {
            path: "src/retrieval/run_retrieval.py".to_string(),
            sha256: LONGMEMEVAL_RETRIEVAL_RUNNER_SHA256.to_string(),
        },
    ];
    if contract.retrieval_sources != expected_sources {
        return Err(invalid_dataset(
            "LongMemEval retrieval source contract is invalid",
        ));
    }
    for kind in LongMemEvalRubricKind::ALL {
        let rubric = contract.rubrics.get(&kind).ok_or_else(|| {
            invalid_dataset(format!("missing LongMemEval rubric contract for {kind}"))
        })?;
        if rubric.template != kind.template()
            || rubric.sha256 != kind.sha256()
            || kind.computed_sha256() != kind.sha256()
        {
            return Err(invalid_dataset(format!(
                "LongMemEval rubric contract mismatch for {kind}"
            )));
        }
    }
    if contract.rubrics.len() != LongMemEvalRubricKind::ALL.len()
        || rubric_bundle_sha256()? != LONGMEMEVAL_RUBRIC_BUNDLE_SHA256
    {
        return Err(invalid_dataset("LongMemEval rubric bundle hash mismatch"));
    }
    let expected_mapping = LongMemEvalQuestionType::ALL
        .into_iter()
        .map(|question_type| {
            (
                question_type,
                LongMemEvalRubricKind::for_question(question_type, false),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if contract.type_mapping != expected_mapping {
        return Err(invalid_dataset(
            "LongMemEval type-to-rubric mapping is invalid",
        ));
    }
    validate_parser_contract(&contract.parser)?;
    if contract.retrieval_vectors.is_empty() {
        return Err(invalid_dataset(
            "LongMemEval retrieval contract vectors must not be empty",
        ));
    }
    for vector in &contract.retrieval_vectors {
        validate_retrieval_vector(vector)?;
    }
    Ok(())
}

fn validate_parser_contract(contract: &LongMemEvalParserContractV1) -> Result<()> {
    if contract.upstream != "case-insensitive substring contains yes"
        || contract.moa != "trimmed case-insensitive exact yes or no"
        || contract.accepted_true != ["yes", " YES "]
        || contract.accepted_false != ["no", " NO "]
        || contract.rejected != ["yes, because", "not yes", "", "maybe"]
    {
        return Err(invalid_dataset(
            "LongMemEval hardened parser contract is invalid",
        ));
    }
    if !contract
        .accepted_true
        .iter()
        .all(|value| parse_absolute_judge_label(value) == Some(true))
        || !contract
            .accepted_false
            .iter()
            .all(|value| parse_absolute_judge_label(value) == Some(false))
        || !contract
            .rejected
            .iter()
            .all(|value| parse_absolute_judge_label(value).is_none())
    {
        return Err(invalid_dataset(
            "LongMemEval parser examples do not match the production parser",
        ));
    }
    Ok(())
}

fn validate_retrieval_vector(vector: &LongMemEvalRetrievalContractVectorV1) -> Result<()> {
    validate_nonblank("retrieval vector name", &vector.name)?;
    if vector.cutoff == 0 {
        return Err(invalid_dataset(
            "retrieval contract vector cutoff must be positive",
        ));
    }
    let ranked = vector.ranked.iter().map(String::as_str).collect::<Vec<_>>();
    let corpus = vector.corpus.iter().map(String::as_str).collect::<Vec<_>>();
    let gold = vector
        .gold
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    if gold.is_empty()
        || (!vector.effective_unique && ranked.iter().collect::<HashSet<_>>().len() != ranked.len())
    {
        return Err(invalid_dataset(
            "retrieval contract vector needs gold and unique ranked IDs",
        ));
    }
    let score = if vector.effective_unique {
        effective_session_score(&ranked, &corpus, &gold, vector.cutoff)
    } else {
        direct_score(&ranked, &corpus, &gold, vector.cutoff)
    };
    if score.scanned_occurrences != vector.expected_scanned
        || (score.recall_any - vector.expected_recall_any).abs() > 1e-12
        || (score.recall_all - vector.expected_recall_all).abs() > 1e-12
        || (score.ndcg - vector.expected_ndcg).abs() > 1e-12
    {
        return Err(invalid_dataset(format!(
            "retrieval contract vector `{}` does not match production metric semantics",
            vector.name
        )));
    }
    Ok(())
}

/// Exact counts recorded by the hermetic synthetic contract fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalFixtureCountsV1 {
    /// Total questions.
    pub questions: usize,
    /// `_abs` questions.
    pub abstentions: usize,
    /// Questions contributing retrieval metrics.
    pub retrieval: usize,
    /// Counts by official question type.
    pub question_types: BTreeMap<LongMemEvalQuestionType, usize>,
}

/// Strict provenance manifest for the hermetic LongMemEval contract fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LongMemEvalFixtureManifestV1 {
    /// Manifest schema version.
    pub schema_version: u32,
    /// Stable synthetic fixture identifier.
    pub dataset: String,
    /// Immutable official dataset source.
    pub source: DatasetPackageSourceV1,
    /// Immutable official evaluator source.
    pub evaluator_repository: String,
    /// Immutable official evaluator commit.
    pub evaluator_commit: String,
    /// Honest fixture origin marker.
    pub content_origin: String,
    /// Human-readable reason for the selected synthetic cases.
    pub selection_rationale: String,
    /// Official package files and digests retained as provenance only.
    pub source_files: Vec<DatasetFileProvenance>,
    /// Byte-bearing committed fixture files, excluding this self-manifest.
    pub fixture_files: Vec<DatasetFileProvenance>,
    /// Exact source-order selected question IDs.
    pub selected_question_ids: Vec<String>,
    /// Exact fixture counts.
    pub counts: LongMemEvalFixtureCountsV1,
}

impl LongMemEvalFixtureManifestV1 {
    /// Validates provenance, fixture bytes, strict loading, selected IDs, and counts.
    pub fn validate(&self, root: &Path) -> Result<()> {
        if self.schema_version != 1
            || self.dataset != "longmemeval-s-cleaned-tiny"
            || self.source.repository != LONGMEMEVAL_REPOSITORY
            || self.source.revision != LONGMEMEVAL_REVISION
            || self.evaluator_repository != LONGMEMEVAL_EVALUATOR_REPOSITORY
            || self.evaluator_commit != LONGMEMEVAL_EVALUATOR_COMMIT
            || self.content_origin != "synthetic_contract_fixture"
            || self.selection_rationale.trim().is_empty()
            || self.source_files != official_longmemeval_manifest().files
        {
            return Err(invalid_dataset(
                "invalid LongMemEval tiny fixture provenance",
            ));
        }
        verify_provenance_files(root, &self.fixture_files)?;
        let dataset = load_longmemeval_file(&root.join("longmemeval_s_cleaned_tiny.json"))?;
        load_upstream_contract(&root.join("upstream_contract_v1.json"))?;
        let selected_ids = dataset
            .cases
            .iter()
            .map(|case| case.metadata.question_id.clone())
            .collect::<Vec<_>>();
        if selected_ids != self.selected_question_ids
            || dataset.cases.len() != self.counts.questions
            || dataset.abstention_count() != self.counts.abstentions
            || dataset.retrieval_count() != self.counts.retrieval
            || dataset.question_type_counts() != self.counts.question_types
        {
            return Err(invalid_dataset(
                "LongMemEval fixture selected IDs or counts do not match its files",
            ));
        }
        Ok(())
    }
}

fn verify_provenance_files(root: &Path, files: &[DatasetFileProvenance]) -> Result<()> {
    let mut previous_path: Option<&str> = None;
    for file in files {
        if previous_path.is_some_and(|previous| previous >= file.path.as_str()) {
            return Err(invalid_dataset(
                "LongMemEval fixture files must be sorted by path",
            ));
        }
        previous_path = Some(&file.path);
        let bytes = std::fs::read(root.join(&file.path))?;
        let size_bytes = u64::try_from(bytes.len())
            .map_err(|_| invalid_dataset("fixture file length does not fit u64"))?;
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        if size_bytes != file.size_bytes || sha256 != file.sha256 {
            return Err(invalid_dataset(format!(
                "LongMemEval fixture file {} provenance mismatch",
                file.path
            )));
        }
    }
    Ok(())
}
