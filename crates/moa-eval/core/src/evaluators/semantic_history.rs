//! Semantic/history assertion evaluator.
//!
//! Some claims are about the conversation rather than the world or the final
//! sentence: a fact planted sixteen turns ago was recalled, a redacted
//! identifier never reappeared, a cited source was actually read. Those live
//! here, over the recorded history and lineage observations, so they cannot be
//! faked by a final response that merely sounds right.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::assertion::{
    AssertionCategory, AssertionEvaluator, AssertionVerdict, EvaluatorDeterminism,
};
use crate::evidence::{EvidenceEnvelope, HistoryRole};

/// Registered id of the history-recall evaluator.
pub const HISTORY_RECALL_EVALUATOR_ID: &str = "history_recall";

/// Which history records a config applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HistoryScope {
    /// Every recorded role.
    #[default]
    Any,
    /// End-user turns only.
    User,
    /// Agent turns only.
    Assistant,
    /// Tool output records only.
    Tool,
    /// System or harness directives only.
    System,
}

impl HistoryScope {
    fn accepts(self, role: HistoryRole) -> bool {
        match self {
            Self::Any => true,
            Self::User => role == HistoryRole::User,
            Self::Assistant => role == HistoryRole::Assistant,
            Self::Tool => role == HistoryRole::Tool,
            Self::System => role == HistoryRole::System,
        }
    }
}

/// Parameters for [`HistoryRecallEvaluator`].
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryRecallConfig {
    /// Which roles the reference rules read.
    pub scope: HistoryScope,
    /// Fragments that must appear somewhere in the scoped history.
    pub must_reference: Vec<String>,
    /// Fragments that must never appear in the scoped history.
    pub must_not_reference: Vec<String>,
    /// Lineage references that must have been recorded.
    pub required_citations: Vec<String>,
    /// Minimum number of scoped history records required.
    pub min_records: usize,
}

/// Requires the recorded conversation and lineage to carry named facts.
#[derive(Debug, Default, Clone, Copy)]
pub struct HistoryRecallEvaluator;

impl AssertionEvaluator for HistoryRecallEvaluator {
    fn id(&self) -> &'static str {
        HISTORY_RECALL_EVALUATOR_ID
    }

    fn version(&self) -> u32 {
        1
    }

    fn category(&self) -> AssertionCategory {
        AssertionCategory::SemanticHistory
    }

    fn determinism(&self) -> EvaluatorDeterminism {
        EvaluatorDeterminism::Deterministic
    }

    fn evaluate(&self, config: &Value, evidence: &EvidenceEnvelope) -> AssertionVerdict {
        let config: HistoryRecallConfig = match serde_json::from_value(config.clone()) {
            Ok(config) => config,
            Err(error) => return AssertionVerdict::invalid_config(error),
        };

        if config.must_reference.is_empty()
            && config.must_not_reference.is_empty()
            && config.required_citations.is_empty()
            && config.min_records == 0
        {
            return AssertionVerdict::failed(
                json!({}),
                json!({}),
                "history_recall assertion declares no rules",
            );
        }

        let scoped = evidence
            .observations
            .history
            .iter()
            .filter(|record| config.scope.accepts(record.role))
            .collect::<Vec<_>>();
        let corpus = scoped
            .iter()
            .map(|record| record.text.to_lowercase())
            .collect::<Vec<_>>()
            .join("\n");
        let citations = evidence
            .observations
            .lineage
            .iter()
            .map(|record| record.reference.as_str())
            .collect::<Vec<_>>();

        let mut failures = Vec::new();
        if scoped.len() < config.min_records {
            failures.push(format!(
                "history holds {} scoped records but {} are required",
                scoped.len(),
                config.min_records
            ));
        }
        for fragment in &config.must_reference {
            if !corpus.contains(&fragment.to_lowercase()) {
                failures.push(format!("history never references '{fragment}'"));
            }
        }
        for fragment in &config.must_not_reference {
            if corpus.contains(&fragment.to_lowercase()) {
                failures.push(format!("history references forbidden '{fragment}'"));
            }
        }
        for citation in &config.required_citations {
            if !citations.iter().any(|reference| reference == citation) {
                failures.push(format!("lineage never recorded citation '{citation}'"));
            }
        }

        let expected = json!({
            "scope": format!("{:?}", config.scope),
            "must_reference": config.must_reference,
            "must_not_reference": config.must_not_reference,
            "required_citations": config.required_citations,
            "min_records": config.min_records,
        });
        let observed = json!({
            "scoped_records": scoped.len(),
            "citations": citations,
        });
        if failures.is_empty() {
            AssertionVerdict::passed(expected, observed, "history and lineage carry every fact")
        } else {
            AssertionVerdict::failed(expected, observed, failures.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{HISTORY_RECALL_EVALUATOR_ID, HistoryRecallEvaluator};
    use crate::assertion::AssertionEvaluator;
    use crate::evidence::{EvidenceEnvelope, EvidenceSubject, HistoryRole};
    use serde_json::json;

    fn evidence() -> EvidenceEnvelope {
        EvidenceEnvelope::builder(EvidenceSubject::default())
            .source("unit_test")
            .history(HistoryRole::User, "the release ticket is TCK-1")
            .history(HistoryRole::Assistant, "acknowledged, tracking TCK-1")
            .lineage("citation", "TCK-1")
            .build()
    }

    #[test]
    fn a_recalled_fact_passes() {
        let verdict = HistoryRecallEvaluator.evaluate(
            &json!({ "scope": "assistant", "must_reference": ["TCK-1"] }),
            &evidence(),
        );

        assert!(verdict.passed, "{}", verdict.diagnostic);
    }

    #[test]
    fn a_fact_present_only_in_another_role_fails_the_scoped_rule() {
        // Pins: scoping is real. A fact the user supplied does not prove the
        // agent recalled it.
        let verdict = HistoryRecallEvaluator.evaluate(
            &json!({ "scope": "assistant", "must_reference": ["release ticket"] }),
            &evidence(),
        );

        assert!(!verdict.passed);
    }

    #[test]
    fn a_forbidden_fragment_fails() {
        let verdict = HistoryRecallEvaluator
            .evaluate(&json!({ "must_not_reference": ["TCK-1"] }), &evidence());

        assert!(!verdict.passed);
    }

    #[test]
    fn a_missing_citation_fails() {
        let verdict = HistoryRecallEvaluator
            .evaluate(&json!({ "required_citations": ["TCK-9"] }), &evidence());

        assert!(!verdict.passed);
        assert!(verdict.diagnostic.contains("TCK-9"));
    }

    #[test]
    fn a_ruleless_config_fails_instead_of_passing_vacuously() {
        let verdict = HistoryRecallEvaluator.evaluate(&json!({}), &evidence());

        assert!(!verdict.passed);
    }

    #[test]
    fn the_registered_identity_is_stable() {
        assert_eq!(HistoryRecallEvaluator.id(), HISTORY_RECALL_EVALUATOR_ID);
        assert_eq!(HistoryRecallEvaluator.version(), 1);
    }
}
