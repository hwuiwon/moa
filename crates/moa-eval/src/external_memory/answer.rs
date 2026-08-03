//! Separate reader, dataset scorer, and absolute-judge contracts.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::cost::NormalizedUsage;
use super::dataset::{ExternalMemoryCase, ExternalMemorySession, PreparedExternalMemoryCase};
use super::{ExternalMemoryError, Result as ExternalMemoryResult};

/// Stable estimator identifier persisted in V2 reports.
pub const TOKEN_ESTIMATOR_CHARS_DIV_4: &str = "chars_div_4_v1";
/// Full-context control envelope prefix.
pub const FULL_CONTEXT_PREFIX: &str = "FULL_CONTEXT_V1\n";
/// Stable context-window exclusion reason.
pub const READER_CONTEXT_LIMIT_REASON: &str = "reader-context-limit";
/// Stable PersonaMem oracle exclusion reason.
pub const PERSONAMEM_ORACLE_UNSUPPORTED_REASON: &str =
    "oracle-evidence-requires-longmemeval-labels";

/// Ordered benchmark mode applied to every dataset case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalMemoryMode {
    /// Production memory formation, retrieval, and evidence rendering.
    Primary,
    /// Reader with empty evidence.
    NoMemory,
    /// Reader with the complete source conversation.
    FullContext,
    /// Reader with independently labeled gold turns only.
    OracleEvidence,
}

impl ExternalMemoryMode {
    /// Returns the required deterministic report order.
    #[must_use]
    pub const fn ordered() -> [Self; 4] {
        [
            Self::Primary,
            Self::NoMemory,
            Self::FullContext,
            Self::OracleEvidence,
        ]
    }
}

/// Whether a generated-answer or control result is supported for one case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", deny_unknown_fields)]
pub enum SupportStatus {
    /// The dataset and model inputs satisfy the contract.
    Supported,
    /// The result is intentionally excluded with an explicit reason.
    Unsupported {
        /// Why this case/control cannot be evaluated.
        reason: String,
    },
}

/// Exact provider-neutral reader prompt text used for fitting and dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedReaderPrompt {
    /// System text sent to the provider.
    pub system: String,
    /// User text sent to the provider.
    pub user: String,
}

impl RenderedReaderPrompt {
    /// Estimates the exact request text under `chars_div_4_v1`.
    #[must_use]
    pub fn estimated_input_tokens(&self) -> u64 {
        estimate_chars_div_4(self.system.chars().count() + self.user.chars().count())
    }
}

/// Rendered evidence and support for one mode before any provider call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeEvidence {
    /// Whether the mode has independent evidence prerequisites.
    pub support: SupportStatus,
    /// Exact evidence text sent to the shared reader prompt renderer.
    pub rendered_evidence: String,
    /// Evidence-only estimate under `chars_div_4_v1`.
    pub rendered_evidence_tokens: u64,
}

#[derive(Serialize)]
struct ControlEnvelope<'a> {
    schema_version: u32,
    mode: &'a str,
    sessions: &'a [ExternalMemorySession],
}

/// Renders the exact evidence for a non-primary mode without truncation.
pub fn render_control_evidence(
    case: &PreparedExternalMemoryCase,
    mode: ExternalMemoryMode,
    dataset: &str,
) -> ExternalMemoryResult<ModeEvidence> {
    let rendered_evidence = match mode {
        ExternalMemoryMode::Primary => {
            return Err(ExternalMemoryError::InvalidConfig(
                "primary evidence must come from the selected backend".to_string(),
            ));
        }
        ExternalMemoryMode::NoMemory => String::new(),
        ExternalMemoryMode::FullContext => {
            render_control_envelope("full_context", &case.case.sessions)?
        }
        ExternalMemoryMode::OracleEvidence => {
            if dataset == super::personamem::PERSONAMEM_DATASET {
                return Ok(ModeEvidence {
                    support: SupportStatus::Unsupported {
                        reason: PERSONAMEM_ORACLE_UNSUPPORTED_REASON.to_string(),
                    },
                    rendered_evidence: String::new(),
                    rendered_evidence_tokens: 0,
                });
            }
            if dataset != super::longmemeval::LONGMEMEVAL_DATASET {
                return Err(ExternalMemoryError::InvalidDataset(
                    "oracle evidence is defined only for LongMemEval".to_string(),
                ));
            }
            let labels = case
                .case
                .evidence_labels
                .turn_source_ids
                .as_ref()
                .filter(|labels| !labels.is_empty())
                .ok_or_else(|| {
                    ExternalMemoryError::InvalidDataset(
                        "oracle evidence requires independent turn labels".to_string(),
                    )
                })?;
            let selected = labels.iter().collect::<std::collections::HashSet<_>>();
            let sessions = case
                .case
                .sessions
                .iter()
                .filter_map(|session| {
                    let mut filtered = session.clone();
                    filtered
                        .turns
                        .retain(|turn| selected.contains(&turn.source_id));
                    (!filtered.turns.is_empty()).then_some(filtered)
                })
                .collect::<Vec<_>>();
            let rendered_count = sessions
                .iter()
                .map(|session| session.turns.len())
                .sum::<usize>();
            if rendered_count != labels.len() {
                return Err(ExternalMemoryError::InvalidDataset(
                    "oracle evidence labels do not resolve exactly".to_string(),
                ));
            }
            render_control_envelope("oracle_evidence", &sessions)?
        }
    };
    Ok(ModeEvidence {
        rendered_evidence_tokens: estimate_chars_div_4(rendered_evidence.chars().count()),
        rendered_evidence,
        support: SupportStatus::Supported,
    })
}

fn render_control_envelope(
    mode: &str,
    sessions: &[ExternalMemorySession],
) -> ExternalMemoryResult<String> {
    let json = serde_json::to_string(&ControlEnvelope {
        schema_version: 1,
        mode,
        sessions,
    })?;
    Ok(format!("{FULL_CONTEXT_PREFIX}{json}"))
}

/// Renders the single reader prompt contract shared by fitting and dispatch.
#[must_use]
pub fn render_reader_prompt(
    case: &PreparedExternalMemoryCase,
    rendered_evidence: &str,
    prompt_version: &str,
    dataset: &str,
) -> RenderedReaderPrompt {
    let instructions = if dataset == super::personamem::PERSONAMEM_DATASET {
        "Select exactly one option from the supplied memory evidence. Return only its parenthesized label, such as `(a)`."
    } else {
        "Answer only from the supplied memory evidence. Be concise. If evidence is insufficient, say so."
    };
    let ordered_options = case
        .case
        .options
        .iter()
        .enumerate()
        .map(|(index, option)| format!("{}. {option}", index + 1))
        .collect::<Vec<_>>();
    RenderedReaderPrompt {
        system: format!(
            "You are the benchmark answer reader.\nPrompt version: {prompt_version}\n{instructions}"
        ),
        user: format!(
            "Question:\n{}\n\nOptions:\n{}\n\nEvidence:\n{}",
            case.case.question,
            if ordered_options.is_empty() {
                "none".to_string()
            } else {
                ordered_options.join("\n")
            },
            rendered_evidence
        ),
    }
}

/// Applies the reader window to the exact rendered provider request text.
#[must_use]
pub fn reader_fit_support(
    prompt: &RenderedReaderPrompt,
    context_window: u64,
    output_token_reserve: u64,
) -> SupportStatus {
    if context_window == 0
        || output_token_reserve == 0
        || prompt
            .estimated_input_tokens()
            .saturating_add(output_token_reserve)
            > context_window
    {
        SupportStatus::Unsupported {
            reason: READER_CONTEXT_LIMIT_REASON.to_string(),
        }
    } else {
        SupportStatus::Supported
    }
}

const fn estimate_chars_div_4(chars: usize) -> u64 {
    (chars as u64).div_ceil(4)
}

/// Control whose prerequisites can be checked without executing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    /// Same reader with empty evidence.
    NoMemory,
    /// Same reader with complete source context.
    FullContext,
    /// Same reader with dataset-labeled turn evidence.
    OracleEvidence,
}

/// Resolves support without inferring missing evidence or truncating full context.
#[must_use]
pub fn control_prerequisite(
    case: &PreparedExternalMemoryCase,
    control: ControlKind,
    reader_token_limit: usize,
) -> SupportStatus {
    match control {
        ControlKind::NoMemory => SupportStatus::Supported,
        ControlKind::FullContext => {
            if reader_token_limit == 0 || case.full_context_token_estimate() > reader_token_limit {
                SupportStatus::Unsupported {
                    reason: "full context exceeds the reader token limit".to_string(),
                }
            } else {
                SupportStatus::Supported
            }
        }
        ControlKind::OracleEvidence => {
            if case
                .case
                .evidence_labels
                .turn_source_ids
                .as_ref()
                .is_none_or(Vec::is_empty)
            {
                SupportStatus::Unsupported {
                    reason: "oracle evidence requires turn-level evidence labels".to_string(),
                }
            } else {
                SupportStatus::Supported
            }
        }
    }
}

/// Provider-neutral request sent to the benchmark reader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderRequest {
    /// Case isolation key.
    pub isolation_key: String,
    /// Question.
    pub question: String,
    /// Optional answer choices.
    pub options: Vec<String>,
    /// Exact rendered evidence returned by the backend.
    pub rendered_evidence: String,
    /// Versioned reader prompt.
    pub prompt_version: String,
}

/// Provider-neutral reader response with normalized actual usage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderResponse {
    /// Generated answer text.
    pub answer: String,
    /// Exact model that answered.
    pub model: String,
    /// Prompt schema/version.
    pub prompt_version: String,
    /// Provider-normalized actual usage.
    pub usage: NormalizedUsage,
    /// Measured request latency.
    pub latency_ms: u64,
}

/// A generated-answer reader.
#[async_trait]
pub trait Reader: Send + Sync {
    /// Produces one answer from exact rendered evidence.
    async fn answer(&self, request: ReaderRequest) -> Result<ReaderResponse, String>;
}

/// Dataset-owned deterministic answer score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerScore {
    /// Metric identifier.
    pub metric: String,
    /// Score in the dataset's documented range.
    pub value: f64,
    /// Explicit denominator contribution.
    pub denominator: usize,
}

/// Dataset-owned deterministic scoring support for one generated answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", deny_unknown_fields)]
pub enum AnswerScoreOutcome {
    /// The dataset supplies a deterministic answer score.
    Supported(AnswerScore),
    /// The dataset requires a different authority, such as an absolute judge.
    Unsupported {
        /// Stable reason retained in the per-case artifact.
        reason: String,
    },
}

/// Dataset-specific scorer, separate from reader and absolute judge.
pub trait AnswerScorer: Send + Sync {
    /// Scores one generated answer under dataset-owned rules.
    fn score(
        &self,
        case: &ExternalMemoryCase,
        answer: &ReaderResponse,
    ) -> Result<AnswerScoreOutcome, String>;
}

/// Input to an absolute judge; it intentionally has no baseline/comparator field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsoluteJudgeRequest {
    /// Question.
    pub question: String,
    /// Dataset reference answer.
    pub reference_answer: String,
    /// Candidate answer to judge absolutely.
    pub candidate_answer: String,
    /// Category-specific rubric text.
    pub rubric: String,
    /// Versioned judge prompt.
    pub prompt_version: String,
}

/// Provider-neutral absolute-judge output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsoluteJudgeResponse {
    /// Absolute support/correctness decision.
    pub supported: bool,
    /// Short judge rationale retained for audit.
    pub rationale: String,
    /// Exact judge model.
    pub model: String,
    /// Versioned judge prompt.
    pub prompt_version: String,
    /// Provider-normalized usage.
    pub usage: NormalizedUsage,
    /// Measured request latency.
    pub latency_ms: u64,
}

/// Dataset-independent absolute answer judge.
#[async_trait]
pub trait AbsoluteAnswerJudge: Send + Sync {
    /// Judges one candidate without seeing a baseline or comparator output.
    async fn judge(&self, request: AbsoluteJudgeRequest) -> Result<AbsoluteJudgeResponse, String>;
}

/// Derives the exact judge identity one absolute decision was produced under.
///
/// A calibration describes an instrument, so a report can only claim it by naming
/// the instrument that actually answered: the model that replied, the prompt
/// version and rubric text that were sent, the parser that turned the reply into
/// a label, and the domain it was applied to. That identity is what
/// [`super::calibration::judge::calibration_expiry`] compares, so deriving it from
/// the request/response pair is what stops a report from inheriting a calibration
/// measured on a different judge.
///
/// `rendered_prompt` must be the exact instruction text the judge sent, not the
/// version label: a prompt edit that keeps the label is precisely the change a
/// version string cannot detect. The rubric hash comes from the request because
/// the rubric is an input the harness controls, while the model is read from the
/// response because that is what actually served, which can differ from what was
/// asked for.
///
/// # Errors
///
/// Returns [`ExternalMemoryError::InvalidConfig`] when the response answered under
/// a different prompt version than the request carried, when the rendered prompt
/// or rubric is empty, or when any label is blank. A prompt-version disagreement
/// is a silent calibration break: the decisions would be attributed to a prompt
/// that did not produce them.
pub fn absolute_judge_identity(
    request: &AbsoluteJudgeRequest,
    response: &AbsoluteJudgeResponse,
    rendered_prompt: &str,
    output_parser_version: &str,
    domain: &str,
) -> ExternalMemoryResult<super::calibration::judge::JudgeIdentity> {
    if request.prompt_version != response.prompt_version {
        return Err(ExternalMemoryError::InvalidConfig(format!(
            "absolute judge answered under prompt version `{}` but was sent `{}`",
            response.prompt_version, request.prompt_version
        )));
    }
    super::calibration::judge::JudgeIdentity::new(
        &response.model,
        &response.prompt_version,
        rendered_prompt.as_bytes(),
        request.rubric.as_bytes(),
        output_parser_version,
        domain,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_memory::cost::{NormalizedUsage, UsageProvenance};

    fn usage() -> NormalizedUsage {
        NormalizedUsage {
            input_tokens_uncached: 100,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 8,
            provenance: UsageProvenance::Actual,
        }
    }

    fn judge_request(prompt_version: &str, rubric: &str) -> AbsoluteJudgeRequest {
        AbsoluteJudgeRequest {
            question: "what deploy target?".to_string(),
            reference_answer: "prod-us-east".to_string(),
            candidate_answer: "prod-us-east".to_string(),
            rubric: rubric.to_string(),
            prompt_version: prompt_version.to_string(),
        }
    }

    fn judge_response(model: &str, prompt_version: &str) -> AbsoluteJudgeResponse {
        AbsoluteJudgeResponse {
            supported: true,
            rationale: "matches the reference".to_string(),
            model: model.to_string(),
            prompt_version: prompt_version.to_string(),
            usage: usage(),
            latency_ms: 12,
        }
    }

    #[test]
    fn absolute_judge_identity_pins_the_instrument_that_actually_answered() {
        // Pins: the derived fingerprint changes when the model, the exact prompt
        // text, the rubric, the parser, or the domain changes, and a response that
        // answered under a different prompt version is refused rather than
        // silently attributed to the requested one. Each component matters on its
        // own, so no single change can slip past a calibration.
        let request = judge_request("v3", "rubric text");
        let response = judge_response("judge-model-1", "v3");
        let baseline = absolute_judge_identity(
            &request,
            &response,
            "exact rendered prompt",
            "strict-json-v1",
            "longmemeval",
        )
        .expect("derivable identity");

        let variants = [
            absolute_judge_identity(
                &request,
                &judge_response("judge-model-2", "v3"),
                "exact rendered prompt",
                "strict-json-v1",
                "longmemeval",
            ),
            absolute_judge_identity(
                &request,
                &response,
                "edited rendered prompt",
                "strict-json-v1",
                "longmemeval",
            ),
            absolute_judge_identity(
                &judge_request("v3", "edited rubric"),
                &response,
                "exact rendered prompt",
                "strict-json-v1",
                "longmemeval",
            ),
            absolute_judge_identity(
                &request,
                &response,
                "exact rendered prompt",
                "lenient-json-v2",
                "longmemeval",
            ),
            absolute_judge_identity(
                &request,
                &response,
                "exact rendered prompt",
                "strict-json-v1",
                "personamem",
            ),
        ];
        for variant in variants {
            let variant = variant.expect("derivable identity");
            assert_ne!(
                variant.fingerprint(),
                baseline.fingerprint(),
                "a changed instrument must not share a calibration fingerprint"
            );
        }

        let error = absolute_judge_identity(
            &request,
            &judge_response("judge-model-1", "v4"),
            "exact rendered prompt",
            "strict-json-v1",
            "longmemeval",
        )
        .expect_err("a prompt-version disagreement must fail closed");
        assert!(error.to_string().contains("prompt version"), "{error}");

        assert!(
            absolute_judge_identity(
                &judge_request("v3", ""),
                &response,
                "exact rendered prompt",
                "strict-json-v1",
                "longmemeval",
            )
            .is_err(),
            "an empty rubric cannot be hashed into an identity"
        );
    }
}
