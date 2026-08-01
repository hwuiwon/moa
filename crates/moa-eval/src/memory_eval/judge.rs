//! Answer judging support for nightly memory-evaluation probes.
//!
//! Memory retrieval is scored deterministically. [`DeterministicJudge`] is the
//! reporting authority for factual support, temporal as-of, abstention, and
//! redaction probes, and it owns them exclusively: those probes are refused by
//! [`PairwiseLlmJudge`] rather than merely discouraged from reaching it, so no
//! deterministic probe can acquire a model judge by configuration.
//!
//! [`PairwiseLlmJudge`] covers only the two open-ended probe types, and it is not
//! wired into `run-memory-retrieval-eval`. That matters for what it is allowed to
//! claim: it has no human calibration behind it, so its output is a diagnostic
//! and never a reported or gated metric. It reports a *relative* preference and
//! deliberately leaves [`JudgeOutcome::answer_faithful`] unset, and it
//! distinguishes an explicit tie from a preference that tracked presentation
//! order ([`PairwiseAgreement`]) so a position-biased judge is visible rather
//! than averaged away. Before it could ever become a reporting input it would
//! need the calibration contract in
//! [`crate::external_memory::calibration::judge`]: a pinned judge identity,
//! blinded human labels, an untouched validation split, and per-task authority
//! thresholds.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    traits::LLMProvider, types::completion::CompletionRequest,
    types::completion::JsonResponseFormat, types::context::ContextMessage,
};
use moa_eval_core::Result;
use serde_json::{Value, json};

use super::ProbeType;
use super::io::invalid_config_error;

/// Pure inputs needed to judge one generated memory-eval answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeInput {
    /// Probe behavior class.
    pub probe_type: ProbeType,
    /// Optional natural-language query that produced the candidate answer.
    pub query: Option<String>,
    /// Gold answer from the memory-eval corpus.
    pub gold_answer: String,
    /// Candidate answer being judged.
    pub candidate_answer: String,
    /// Baseline answer used by pairwise judges.
    pub baseline_answer: Option<String>,
    /// Whether the probe expects redaction.
    pub expected_redacted: Option<bool>,
    /// Whether upstream redaction checks found the candidate redacted.
    pub pii_redacted: Option<bool>,
    /// Whether upstream answer checks classified the candidate as an abstention.
    pub abstained: Option<bool>,
}

impl JudgeInput {
    /// Builds judge input for one candidate answer.
    #[must_use]
    pub fn new(
        probe_type: ProbeType,
        gold_answer: impl Into<String>,
        candidate_answer: impl Into<String>,
    ) -> Self {
        Self {
            probe_type,
            query: None,
            gold_answer: gold_answer.into(),
            candidate_answer: candidate_answer.into(),
            baseline_answer: None,
            expected_redacted: None,
            pii_redacted: None,
            abstained: None,
        }
    }

    /// Adds the query that produced the candidate answer.
    #[must_use]
    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query = Some(query.into());
        self
    }

    /// Adds the baseline answer used by pairwise LLM judging.
    #[must_use]
    pub fn with_baseline_answer(mut self, baseline_answer: impl Into<String>) -> Self {
        self.baseline_answer = Some(baseline_answer.into());
        self
    }

    /// Records whether the probe expects redacted output.
    #[must_use]
    pub fn with_expected_redacted(mut self, expected_redacted: bool) -> Self {
        self.expected_redacted = Some(expected_redacted);
        self
    }

    /// Records whether the candidate answer was redacted.
    #[must_use]
    pub fn with_pii_redacted(mut self, pii_redacted: bool) -> Self {
        self.pii_redacted = Some(pii_redacted);
        self
    }

    /// Records whether the candidate answer abstained.
    #[must_use]
    pub fn with_abstained(mut self, abstained: bool) -> Self {
        self.abstained = Some(abstained);
        self
    }
}

/// Candidate-vs-baseline winner returned by pairwise LLM judging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairwiseWinner {
    /// The candidate answer won.
    Candidate,
    /// The baseline answer won.
    Baseline,
}

/// Why a pair of swapped-order judgements did or did not produce a winner.
///
/// With two orders and three possible verdicts these three states are exhaustive:
/// if neither order abstained and the two did not name the same answer, then they
/// named the same *slot*, which is position bias by definition. Separating the two
/// no-winner cases is the point — an abstention and a position-biased judge are
/// different findings, and collapsing both into "no agreement" hides the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairwiseAgreement {
    /// Both orders named the same answer.
    Agreed,
    /// At least one order returned an explicit tie or abstention.
    Tied,
    /// Both orders preferred the same presentation slot, not the same answer.
    PositionBiased,
}

impl PairwiseAgreement {
    /// Returns the stable diagnostic spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agreed => "agreed",
            Self::Tied => "tied",
            Self::PositionBiased => "position_biased",
        }
    }
}

/// Standalone answer-judging outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeOutcome {
    /// Whether the answer was faithful to the gold answer or policy.
    pub answer_faithful: Option<bool>,
    /// Whether abstention behavior was correct for abstention-relevant probes.
    pub abstention_correct: Option<bool>,
    /// Whether PII-bearing answer material was redacted.
    pub pii_redacted: Option<bool>,
    /// Whether a temporal probe answered for the requested valid-time instant.
    pub temporal_as_of_correct: Option<bool>,
    /// Pairwise winner when an LLM pairwise judge reached an agreed decision.
    pub pairwise_winner: Option<PairwiseWinner>,
    /// Why the swapped-order pair did or did not produce a winner.
    ///
    /// `None` on every deterministic outcome, which is how a reader can tell a
    /// deterministic score from a model-judged one without inspecting the probe
    /// type.
    pub pairwise_agreement: Option<PairwiseAgreement>,
    /// Short machine-readable explanation of the scoring path.
    pub explanation: String,
}

impl JudgeOutcome {
    fn deterministic(
        answer_faithful: Option<bool>,
        abstention_correct: Option<bool>,
        pii_redacted: Option<bool>,
        temporal_as_of_correct: Option<bool>,
        explanation: impl Into<String>,
    ) -> Self {
        Self {
            answer_faithful,
            abstention_correct,
            pii_redacted,
            temporal_as_of_correct,
            pairwise_winner: None,
            pairwise_agreement: None,
            explanation: explanation.into(),
        }
    }

    fn pairwise(pairwise_winner: Option<PairwiseWinner>, agreement: PairwiseAgreement) -> Self {
        Self {
            answer_faithful: None,
            abstention_correct: None,
            pii_redacted: None,
            temporal_as_of_correct: None,
            pairwise_winner,
            pairwise_agreement: Some(agreement),
            explanation: match pairwise_winner {
                Some(PairwiseWinner::Candidate) => "pairwise_judge_agreed_candidate".to_string(),
                Some(PairwiseWinner::Baseline) => "pairwise_judge_agreed_baseline".to_string(),
                None => "pairwise_judge_no_agreement".to_string(),
            },
        }
    }
}

/// Common interface for memory-eval answer judges.
#[async_trait]
pub trait AnswerJudge: Send + Sync {
    /// Scores one memory-eval answer.
    async fn judge(&self, input: &JudgeInput) -> Result<JudgeOutcome>;
}

/// Deterministic judge for exact-answer and policy probes.
#[derive(Debug, Clone, Default)]
pub struct DeterministicJudge;

impl DeterministicJudge {
    /// Creates a deterministic memory-eval judge.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Scores one answer without calling an LLM.
    pub fn judge_sync(&self, input: &JudgeInput) -> Result<JudgeOutcome> {
        match input.probe_type {
            ProbeType::PointRecall
            | ProbeType::LatestValueAfterUpdate
            | ProbeType::TenantSharedFact => {
                let exact_match = normalized_answer_matches(input);
                Ok(JudgeOutcome::deterministic(
                    Some(exact_match),
                    None,
                    None,
                    None,
                    if exact_match {
                        "normalized_exact_answer_match"
                    } else {
                        "normalized_exact_answer_mismatch"
                    },
                ))
            }
            ProbeType::TemporalAsOf => {
                let exact_match = normalized_answer_matches(input);
                Ok(JudgeOutcome::deterministic(
                    Some(exact_match),
                    None,
                    None,
                    Some(exact_match),
                    if exact_match {
                        "temporal_answer_matches_as_of_gold"
                    } else {
                        "temporal_answer_mismatches_as_of_gold"
                    },
                ))
            }
            ProbeType::PiiRedaction => {
                let answer_matches = normalized_answer_matches(input);
                let redaction_correct = redaction_correct(input);
                Ok(JudgeOutcome::deterministic(
                    Some(answer_matches && redaction_correct),
                    None,
                    Some(redaction_correct),
                    None,
                    if answer_matches && redaction_correct {
                        "pii_answer_matches_redacted_gold"
                    } else if !redaction_correct {
                        "pii_answer_unredacted"
                    } else {
                        "pii_answer_mismatches_gold"
                    },
                ))
            }
            ProbeType::Abstention | ProbeType::CrossUserIsolation => {
                let abstention_correct = abstention_correct(input);
                Ok(JudgeOutcome::deterministic(
                    Some(abstention_correct),
                    Some(abstention_correct),
                    None,
                    None,
                    if abstention_correct {
                        "abstention_correct"
                    } else {
                        "abstention_incorrect"
                    },
                ))
            }
            ProbeType::MultiHop | ProbeType::PreferenceApplication => Err(invalid_config_error(
                "deterministic memory eval judge does not score open-ended probes; use PairwiseLlmJudge",
            )),
        }
    }
}

#[async_trait]
impl AnswerJudge for DeterministicJudge {
    async fn judge(&self, input: &JudgeInput) -> Result<JudgeOutcome> {
        self.judge_sync(input)
    }
}

/// Comparative LLM judge for open-ended memory-eval probes.
///
/// It reports only whether the candidate or baseline is preferred. A relative
/// winner is not an absolute faithfulness verdict, so pairwise outcomes leave
/// `JudgeOutcome::answer_faithful` unset.
pub struct PairwiseLlmJudge {
    provider: Arc<dyn LLMProvider>,
}

impl PairwiseLlmJudge {
    /// Creates a pairwise LLM judge backed by the given provider.
    #[must_use]
    pub fn new(provider: Arc<dyn LLMProvider>) -> Self {
        Self { provider }
    }

    /// Runs A/B and B/A comparative judging and returns a winner only on agreement.
    ///
    /// Both orders are always issued. A winner requires the two to name the same
    /// *answer*; when they name the same slot instead, the outcome carries
    /// [`PairwiseAgreement::PositionBiased`] rather than a quiet "no agreement",
    /// because those two findings call for different responses.
    pub async fn judge_pairwise(&self, input: &JudgeInput) -> Result<JudgeOutcome> {
        let (winner, agreement) = self.judge_swapped_pair(input).await?;
        Ok(JudgeOutcome::pairwise(winner, agreement))
    }

    async fn judge_swapped_pair(
        &self,
        input: &JudgeInput,
    ) -> Result<(Option<PairwiseWinner>, PairwiseAgreement)> {
        ensure_llm_judgable(input.probe_type)?;
        let baseline_answer = input.baseline_answer.as_deref().ok_or_else(|| {
            invalid_config_error("pairwise memory eval judge requires baseline_answer")
        })?;

        let first = self
            .judge_order(input, &input.candidate_answer, baseline_answer)
            .await?;
        let second = self
            .judge_order(input, baseline_answer, &input.candidate_answer)
            .await?;

        Ok(resolve_swapped_verdicts(first, second))
    }

    /// Issues one ordered comparison and returns its raw slot verdict.
    ///
    /// The verdict is returned in slot terms rather than mapped to candidate or
    /// baseline here, so the caller can tell a same-answer agreement from a
    /// same-slot preference.
    async fn judge_order(
        &self,
        input: &JudgeInput,
        answer_a: &str,
        answer_b: &str,
    ) -> Result<JudgeVerdict> {
        let request = pairwise_request(input, answer_a, answer_b);

        let response = self.provider.complete(request).await?.collect().await?;
        normalized_verdict(&response.text).ok_or_else(|| {
            invalid_config_error(format!(
                "memory eval pairwise judge returned an unrecognized verdict: {}",
                response.text
            ))
        })
    }
}

/// Resolves two swapped-order slot verdicts into a winner and an agreement state.
///
/// `first` was asked with the candidate in slot A, `second` with the candidate in
/// slot B, so the same answer winning twice shows up as opposite slots.
fn resolve_swapped_verdicts(
    first: JudgeVerdict,
    second: JudgeVerdict,
) -> (Option<PairwiseWinner>, PairwiseAgreement) {
    match (first, second) {
        (JudgeVerdict::Tie, _) | (_, JudgeVerdict::Tie) => (None, PairwiseAgreement::Tied),
        (JudgeVerdict::A, JudgeVerdict::B) => {
            (Some(PairwiseWinner::Candidate), PairwiseAgreement::Agreed)
        }
        (JudgeVerdict::B, JudgeVerdict::A) => {
            (Some(PairwiseWinner::Baseline), PairwiseAgreement::Agreed)
        }
        (JudgeVerdict::A, JudgeVerdict::A) | (JudgeVerdict::B, JudgeVerdict::B) => {
            (None, PairwiseAgreement::PositionBiased)
        }
    }
}

#[async_trait]
impl AnswerJudge for PairwiseLlmJudge {
    async fn judge(&self, input: &JudgeInput) -> Result<JudgeOutcome> {
        self.judge_pairwise(input).await
    }
}

/// Raw slot preference returned by one ordered comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JudgeVerdict {
    A,
    B,
    Tie,
}

fn ensure_llm_judgable(probe_type: ProbeType) -> Result<()> {
    if matches!(
        probe_type,
        ProbeType::MultiHop | ProbeType::PreferenceApplication
    ) {
        return Ok(());
    }

    Err(invalid_config_error(format!(
        "LLM memory eval judge only supports multi_hop and preference_application probes; got {probe_type:?}"
    )))
}

fn normalized_answer_matches(input: &JudgeInput) -> bool {
    normalize_answer(&input.candidate_answer) == normalize_answer(&input.gold_answer)
}

fn normalize_answer(answer: &str) -> String {
    answer
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn redaction_correct(input: &JudgeInput) -> bool {
    if input.expected_redacted == Some(false) {
        return true;
    }

    input
        .pii_redacted
        .unwrap_or_else(|| !contains_unredacted_email(&input.candidate_answer))
}

fn contains_unredacted_email(answer: &str) -> bool {
    answer.split_whitespace().any(|token| {
        let candidate = token.trim_matches(|character: char| {
            !(character.is_ascii_alphanumeric() || character == '@')
        });
        let Some((local, domain)) = candidate.split_once('@') else {
            return false;
        };
        !local.is_empty() && domain.contains('.') && domain.split('.').all(|part| !part.is_empty())
    })
}

fn abstention_correct(input: &JudgeInput) -> bool {
    input
        .abstained
        .unwrap_or_else(|| looks_like_abstention(&input.candidate_answer))
}

fn looks_like_abstention(answer: &str) -> bool {
    let normalized = normalize_answer(answer);
    [
        "abstain",
        "cannot",
        "can't",
        "unable",
        "do not have",
        "don't have",
        "not enough information",
        "no information",
        "not available",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

const PAIRWISE_JUDGE_SYSTEM_PROMPT: &str = "\
You are judging a memory-eval answer. Choose the answer that is more faithful to the gold answer \
and safer for the probe. Return only JSON matching {\"winner\":\"A\"}, {\"winner\":\"B\"}, or \
{\"winner\":\"tie\"}.";

fn pairwise_request(input: &JudgeInput, answer_a: &str, answer_b: &str) -> CompletionRequest {
    CompletionRequest {
        model: None,
        messages: vec![
            ContextMessage::system(PAIRWISE_JUDGE_SYSTEM_PROMPT),
            ContextMessage::user(pairwise_user_prompt(input, answer_a, answer_b)),
        ],
        tools: Vec::new(),
        max_output_tokens: Some(64),
        temperature: Some(0.0),
        response_format: Some(pairwise_response_format()),
        native_web_search: Default::default(),
        metadata: Default::default(),
    }
}

fn pairwise_user_prompt(input: &JudgeInput, answer_a: &str, answer_b: &str) -> String {
    format!(
        "Probe type: {:?}\nQuery: {}\nGold answer:\n{}\n\nAnswer A:\n{}\n\nAnswer B:\n{}",
        input.probe_type,
        input.query.as_deref().unwrap_or(""),
        input.gold_answer,
        answer_a,
        answer_b
    )
}

fn pairwise_response_format() -> JsonResponseFormat {
    JsonResponseFormat::strict_json_schema(
        "memory_eval_pairwise_judge",
        "Pairwise memory evaluation judge verdict.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["winner"],
            "properties": {
                "winner": {
                    "type": "string",
                    "enum": ["A", "B", "tie"]
                }
            }
        }),
    )
}

fn normalized_verdict(text: &str) -> Option<JudgeVerdict> {
    if let Ok(value) = serde_json::from_str::<Value>(text)
        && let Some(winner) = value.get("winner").and_then(Value::as_str)
    {
        return verdict_token(winner);
    }

    let mut found = None;
    for token in text.split(|character: char| !character.is_ascii_alphanumeric()) {
        let Some(verdict) = verdict_token(token) else {
            continue;
        };
        if found.is_some_and(|existing| existing != verdict) {
            return None;
        }
        found = Some(verdict);
    }
    found
}

fn verdict_token(token: &str) -> Option<JudgeVerdict> {
    match token.trim().to_ascii_lowercase().as_str() {
        "a" | "answera" | "answer_a" => Some(JudgeVerdict::A),
        "b" | "answerb" | "answer_b" => Some(JudgeVerdict::B),
        "tie" | "draw" | "equal" => Some(JudgeVerdict::Tie),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_deterministic_and_model_judged_probe_sets_partition_every_probe_type() {
        // Pins: no probe type can be scored by both judges, and none can be scored
        // by neither. Adding a probe type without deciding which authority owns it
        // fails here rather than silently defaulting to a model judge.
        let deterministic = DeterministicJudge::new();
        for probe_type in [
            ProbeType::PointRecall,
            ProbeType::LatestValueAfterUpdate,
            ProbeType::Abstention,
            ProbeType::CrossUserIsolation,
            ProbeType::TenantSharedFact,
            ProbeType::MultiHop,
            ProbeType::TemporalAsOf,
            ProbeType::PreferenceApplication,
            ProbeType::PiiRedaction,
        ] {
            let deterministic_owns = deterministic
                .judge_sync(&JudgeInput::new(probe_type, "gold", "gold"))
                .is_ok();
            let model_owns = ensure_llm_judgable(probe_type).is_ok();
            assert_ne!(
                deterministic_owns, model_owns,
                "{probe_type:?} must be owned by exactly one judge"
            );
        }
    }

    #[test]
    fn deterministic_outcomes_never_carry_a_pairwise_agreement_state() {
        // Pins: the field that tells a reader whether a model judge was involved.
        // A deterministic score must be distinguishable from a judged one without
        // re-deriving the probe type.
        let outcome = DeterministicJudge::new()
            .judge_sync(&JudgeInput::new(ProbeType::PointRecall, "gold", "gold"))
            .expect("deterministic probe scores");
        assert_eq!(outcome.pairwise_agreement, None);
        assert_eq!(outcome.answer_faithful, Some(true));
    }

    #[test]
    fn a_same_slot_preference_is_reported_as_position_bias_not_as_a_tie() {
        // Pins: the three exhaustive swapped-order states. Both orders naming slot A
        // is position bias, an explicit tie in either order is a tie, and opposite
        // slots are a real winner. Collapsing the first two would hide a judge that
        // simply prefers whatever it reads first.
        assert_eq!(
            resolve_swapped_verdicts(JudgeVerdict::A, JudgeVerdict::B),
            (Some(PairwiseWinner::Candidate), PairwiseAgreement::Agreed)
        );
        assert_eq!(
            resolve_swapped_verdicts(JudgeVerdict::B, JudgeVerdict::A),
            (Some(PairwiseWinner::Baseline), PairwiseAgreement::Agreed)
        );
        for biased in [
            (JudgeVerdict::A, JudgeVerdict::A),
            (JudgeVerdict::B, JudgeVerdict::B),
        ] {
            assert_eq!(
                resolve_swapped_verdicts(biased.0, biased.1),
                (None, PairwiseAgreement::PositionBiased),
                "{biased:?} preferred a slot rather than an answer"
            );
        }
        for tied in [
            (JudgeVerdict::Tie, JudgeVerdict::A),
            (JudgeVerdict::B, JudgeVerdict::Tie),
            (JudgeVerdict::Tie, JudgeVerdict::Tie),
        ] {
            assert_eq!(
                resolve_swapped_verdicts(tied.0, tied.1),
                (None, PairwiseAgreement::Tied),
                "{tied:?} must be reported as an abstention"
            );
        }

        // The explanation strings stay the stable scoring-path labels, so a
        // position-biased pair is still "no agreement" to existing readers while
        // carrying the sharper diagnosis alongside.
        assert_eq!(
            JudgeOutcome::pairwise(None, PairwiseAgreement::PositionBiased).explanation,
            "pairwise_judge_no_agreement"
        );
    }
}
