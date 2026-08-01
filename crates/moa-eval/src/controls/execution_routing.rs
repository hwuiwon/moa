//! Execution-routing suite controls and the checked-in-corpus authoring validator.
//!
//! Routing accuracy is the metric most vulnerable to a class prior: a corpus
//! with 60% `execute` cases makes "always execute" look competent. Both nulls
//! here are exactly that kind of degenerate predictor, and the majority class is
//! fitted on the authoring split so the gate is never used to tune its own
//! control.
//!
//! There is no solution function for these cases. Each expected label was
//! adjudicated by a person, and no code can re-derive it — writing one would be
//! fiction dressed as validation. What this module validates instead is the
//! *authoring*: hash-pinned provenance, no duplicate objectives, no
//! contradictory labels on the same objective, no objective that states its own
//! label, and no structurally impossible case.

use std::collections::BTreeMap;

use moa_core::types::execution_planning::ExecutionStrategy;
use serde::{Deserialize, Serialize};

use crate::controls::authoring::{
    AuthoredCase, AuthoringDefect, AuthoringSplit, CaseProvenance, LabelLeakLexicon,
    validate_authored_cases,
};
use crate::execution::routing::{
    ExecutionRoutingCase, ExecutionRoutingClassifierFixture, ExecutionRoutingLabel,
};

/// One predicted route, in the same shape the corpus grades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingPrediction {
    /// Predicted public route label.
    pub label: ExecutionRoutingLabel,
    /// Predicted strategy, meaningful only for `Execute`.
    pub strategy: Option<ExecutionStrategy>,
}

/// Which negative control to materialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingNull {
    /// The authoring split's majority label, for every case.
    MajorityClassAuthoringSplit,
    /// Execute with the durable strategy, for every case.
    AlwaysDurable,
}

impl RoutingNull {
    /// Returns the registered control id.
    #[must_use]
    pub const fn control_id(self) -> &'static str {
        match self {
            Self::MajorityClassAuthoringSplit => "majority_class_authoring_split",
            Self::AlwaysDurable => "always_durable",
        }
    }
}

/// Returns the stable slice key for one case: its adjudicated label.
#[must_use]
pub fn slice_key(case: &ExecutionRoutingCase) -> String {
    label_key(case.expected_label)
}

fn label_key(label: ExecutionRoutingLabel) -> String {
    match label {
        ExecutionRoutingLabel::Respond => "respond",
        ExecutionRoutingLabel::Execute => "execute",
        ExecutionRoutingLabel::NeedsInput => "needs_input",
    }
    .to_string()
}

/// Fits the majority class on the authoring split only.
///
/// Ties resolve to the label with the lowest stable key so the control is
/// deterministic. Returns `None` when the authoring split is empty, because a
/// majority-class null with nothing to learn from is not a control.
#[must_use]
pub fn majority_label(
    cases: &[ExecutionRoutingCase],
    split: &AuthoringSplit,
) -> Option<(ExecutionRoutingLabel, Option<ExecutionStrategy>)> {
    let mut counts: BTreeMap<String, (usize, ExecutionRoutingLabel)> = BTreeMap::new();
    let mut strategy_counts: BTreeMap<String, usize> = BTreeMap::new();
    for case in cases
        .iter()
        .filter(|case| split.is_authoring(&case.case_id))
    {
        let key = label_key(case.expected_label);
        let entry = counts
            .entry(key.clone())
            .or_insert((0, case.expected_label));
        entry.0 += 1;
        if let Some(strategy) = case.expected_strategy {
            *strategy_counts
                .entry(format!("{key}:{strategy:?}"))
                .or_insert(0) += 1;
        }
    }
    let (key, (_, label)) = counts
        .iter()
        .max_by(|left, right| left.1.0.cmp(&right.1.0).then_with(|| right.0.cmp(left.0)))
        .map(|(key, value)| (key.clone(), *value))?;
    let strategy = if label == ExecutionRoutingLabel::Execute {
        let inline = strategy_counts
            .get(&format!("{key}:Inline"))
            .copied()
            .unwrap_or(0);
        let durable = strategy_counts
            .get(&format!("{key}:Durable"))
            .copied()
            .unwrap_or(0);
        Some(if durable > inline {
            ExecutionStrategy::Durable
        } else {
            ExecutionStrategy::Inline
        })
    } else {
        None
    };
    Some((label, strategy))
}

/// Returns one prediction per case for a negative control.
///
/// Both nulls are constant predictors, so `seed` cannot change them. It is still
/// accepted and recorded: a ceiling derived from repeated seeds of a genuinely
/// constant null must be allowed to come out degenerate rather than be given
/// invented variance.
#[must_use]
pub fn control_predictions(
    control: RoutingNull,
    cases: &[ExecutionRoutingCase],
    split: &AuthoringSplit,
) -> Vec<RoutingPrediction> {
    let prediction = match control {
        RoutingNull::MajorityClassAuthoringSplit => majority_label(cases, split).map_or(
            RoutingPrediction {
                label: ExecutionRoutingLabel::Respond,
                strategy: None,
            },
            |(label, strategy)| RoutingPrediction { label, strategy },
        ),
        RoutingNull::AlwaysDurable => RoutingPrediction {
            label: ExecutionRoutingLabel::Execute,
            strategy: Some(ExecutionStrategy::Durable),
        },
    };
    cases.iter().map(|_| prediction).collect()
}

/// Returns the oracle prediction per case: the manifest's adjudicated route.
#[must_use]
pub fn oracle_predictions(cases: &[ExecutionRoutingCase]) -> Vec<RoutingPrediction> {
    cases
        .iter()
        .map(|case| RoutingPrediction {
            label: case.expected_label,
            strategy: case.expected_strategy,
        })
        .collect()
}

/// Returns whether a prediction satisfies a case exactly.
///
/// Route and strategy both have to match, which is the same bar the corpus's own
/// `passed` field applies.
#[must_use]
pub fn prediction_passes(case: &ExecutionRoutingCase, prediction: &RoutingPrediction) -> bool {
    case.expected_label == prediction.label && case.expected_strategy == prediction.strategy
}

/// Scores route accuracy per adjudicated label.
#[must_use]
pub fn route_accuracy_by_label(
    cases: &[ExecutionRoutingCase],
    predictions: &[RoutingPrediction],
) -> BTreeMap<String, f64> {
    let mut totals: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for (case, prediction) in cases.iter().zip(predictions) {
        let entry = totals.entry(slice_key(case)).or_insert((0, 0));
        entry.1 += 1;
        if prediction_passes(case, prediction) {
            entry.0 += 1;
        }
    }
    totals
        .into_iter()
        .map(|(slice, (passed, total))| (slice, passed as f64 / total as f64))
        .collect()
}

/// Builds repeated null seed runs for one negative control.
#[must_use]
pub fn null_seed_runs(
    control: RoutingNull,
    cases: &[ExecutionRoutingCase],
    split: &AuthoringSplit,
    seeds: &[u64],
) -> Vec<crate::kernel::controls::NullSeedRun> {
    let predictions = control_predictions(control, cases, split);
    seeds
        .iter()
        .map(|seed| {
            crate::kernel::controls::NullSeedRun::new(
                *seed,
                route_accuracy_by_label(cases, &predictions),
            )
        })
        .collect()
}

/// Label phrases whose presence in an objective would give the answer away.
#[must_use]
pub fn routing_label_lexicon() -> LabelLeakLexicon {
    LabelLeakLexicon::new([
        (
            "needs_input",
            vec!["needs input", "needs_input", "needs clarification"],
        ),
        ("execute", vec!["execute route", "durable execution run"]),
        ("respond", vec!["respond route", "no tools needed"]),
    ])
}

/// Derives per-case authoring provenance from the corpus manifest entry.
///
/// A checked-in generated corpus does not have per-case authorship, so the
/// honest provenance is corpus-level: the pinned file and its content hash.
/// Attribution therefore holds exactly as long as the hash matches.
#[must_use]
pub fn manifest_provenance(path: &str, sha256: &str) -> CaseProvenance {
    CaseProvenance {
        authored_by: "execution corpus generator".to_string(),
        source: format!("{path}#sha256={sha256}"),
        adjudicated_by: "execution corpus manifest".to_string(),
    }
}

/// Validates authoring quality of the checked-in routing corpus.
///
/// Runs the shared authoring checks and then the routing-specific
/// impossible-case invariants:
///
/// - a strategy is present exactly for `Execute`;
/// - a `NeedsInput` case whose scripted classifier accepted a response must name
///   at least one missing input, otherwise no correct router could produce it;
/// - Durable-upgrade evidence exists only when an upgrade signal does;
/// - `near_boundary` marks an Execute/Inline boundary case, not another label.
#[must_use]
pub fn validate_routing_corpus(
    cases: &[ExecutionRoutingCase],
    provenance: &CaseProvenance,
) -> Vec<AuthoringDefect> {
    let authored = cases
        .iter()
        .map(|case| AuthoredCase {
            case_id: case.case_id.clone(),
            input: case.objective.clone(),
            expected_label: label_key(case.expected_label),
            provenance: Some(provenance.clone()),
        })
        .collect::<Vec<_>>();
    let mut defects = validate_authored_cases(&authored, &routing_label_lexicon());

    for case in cases {
        let strategy_expected = case.expected_label == ExecutionRoutingLabel::Execute;
        if strategy_expected != case.expected_strategy.is_some() {
            defects.push(AuthoringDefect::ImpossibleCase {
                case_id: case.case_id.clone(),
                reason: format!(
                    "expected_strategy must be present exactly for execute; label {:?} strategy {:?}",
                    case.expected_label, case.expected_strategy
                ),
            });
        }
        if case.expected_label == ExecutionRoutingLabel::NeedsInput
            && let ExecutionRoutingClassifierFixture::Response { output, .. } = &case.classifier
            && output.missing_inputs.is_empty()
        {
            defects.push(AuthoringDefect::ImpossibleCase {
                case_id: case.case_id.clone(),
                reason: "needs_input case names no missing inputs".to_string(),
            });
        }
        if case.expected_durable_upgrade_evidence.is_some() != case.durable_upgrade.is_some() {
            defects.push(AuthoringDefect::ImpossibleCase {
                case_id: case.case_id.clone(),
                reason: "durable-upgrade evidence exists only with an upgrade signal".to_string(),
            });
        }
        if case.near_boundary && case.expected_label != ExecutionRoutingLabel::Execute {
            defects.push(AuthoringDefect::ImpossibleCase {
                case_id: case.case_id.clone(),
                reason: "near_boundary marks execute/inline boundary cases only".to_string(),
            });
        }
    }
    defects
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controls::authoring::DEFAULT_AUTHORING_FRACTION;
    use moa_brain::execution_planning::routing::{
        ExecutionRouteClassifierLabel, ExecutionRouteClassifierOutput,
    };
    use moa_core::types::completion::TokenUsage;
    use moa_core::types::execution_planning::ExecutionRouteClassifierOutcome;

    fn case(
        case_id: &str,
        objective: &str,
        label: ExecutionRoutingLabel,
        strategy: Option<ExecutionStrategy>,
    ) -> ExecutionRoutingCase {
        let classifier_label = match label {
            ExecutionRoutingLabel::Respond => ExecutionRouteClassifierLabel::Respond,
            ExecutionRoutingLabel::Execute => ExecutionRouteClassifierLabel::Execute,
            ExecutionRoutingLabel::NeedsInput => ExecutionRouteClassifierLabel::NeedsInput,
        };
        ExecutionRoutingCase {
            schema_version: 1,
            case_id: case_id.to_string(),
            objective: objective.to_string(),
            attachment_count: 0,
            has_recent_target: false,
            available_skills: Vec::new(),
            classifier: ExecutionRoutingClassifierFixture::Response {
                output: ExecutionRouteClassifierOutput {
                    label: classifier_label,
                    strategy,
                    rationale: "fixture".to_string(),
                    confidence_bps: 9000,
                    missing_inputs: if label == ExecutionRoutingLabel::NeedsInput {
                        vec!["target environment".to_string()]
                    } else {
                        Vec::new()
                    },
                },
                usage: TokenUsage::default(),
                cost_microusd: 1,
            },
            expected_classifier_outcome: ExecutionRouteClassifierOutcome::Accepted,
            expected_label: label,
            expected_strategy: strategy,
            near_boundary: false,
            durable_upgrade: None,
            expected_durable_upgrade_evidence: None,
            tags: Vec::new(),
        }
    }

    fn corpus() -> Vec<ExecutionRoutingCase> {
        let mut cases = Vec::new();
        for index in 0..12 {
            cases.push(case(
                &format!("execute-{index:03}"),
                &format!("Roll out change set {index} across the fleet"),
                ExecutionRoutingLabel::Execute,
                Some(ExecutionStrategy::Inline),
            ));
        }
        for index in 0..6 {
            cases.push(case(
                &format!("respond-{index:03}"),
                &format!("Summarize how subsystem {index} handles retries"),
                ExecutionRoutingLabel::Respond,
                None,
            ));
        }
        for index in 0..4 {
            cases.push(case(
                &format!("clarify-{index:03}"),
                &format!("Fix the thing in service {index}"),
                ExecutionRoutingLabel::NeedsInput,
                None,
            ));
        }
        cases
    }

    fn split(cases: &[ExecutionRoutingCase]) -> AuthoringSplit {
        AuthoringSplit::derive(
            "execution_routing",
            cases.iter().map(|case| case.case_id.as_str()),
            DEFAULT_AUTHORING_FRACTION,
        )
    }

    fn provenance() -> CaseProvenance {
        manifest_provenance("routing.jsonl", &"a".repeat(64))
    }

    #[test]
    fn the_oracle_control_passes_every_case() {
        // Pins: replaying the adjudicated label scores 1.0 in every slice, so the
        // grader is capable of a perfect score at all.
        let cases = corpus();
        let accuracy = route_accuracy_by_label(&cases, &oracle_predictions(&cases));

        assert_eq!(accuracy["execute"], 1.0);
        assert_eq!(accuracy["respond"], 1.0);
        assert_eq!(accuracy["needs_input"], 1.0);
    }

    #[test]
    fn the_majority_class_null_is_fitted_on_the_authoring_split() {
        // Pins: the prior comes from cases that do not gate. Fitting it on the
        // gated set would inflate the null and shrink the measured margin.
        let cases = corpus();
        let split = split(&cases);
        let gated_only = cases
            .iter()
            .filter(|case| split.is_gated(&case.case_id))
            .cloned()
            .collect::<Vec<_>>();
        let empty_split = AuthoringSplit::derive("execution_routing", Vec::<&str>::new(), 0.0);

        assert!(majority_label(&gated_only, &empty_split).is_none());
        let (label, strategy) =
            majority_label(&cases, &split).expect("authoring split is non-empty");
        assert_eq!(label, ExecutionRoutingLabel::Execute);
        assert_eq!(strategy, Some(ExecutionStrategy::Inline));
    }

    #[test]
    fn the_majority_class_null_scores_the_class_prior_and_zero_elsewhere() {
        // Pins: the null's slice profile is exactly what a class prior earns,
        // which is why a single global mean would hide it.
        let cases = corpus();
        let split = split(&cases);
        let predictions =
            control_predictions(RoutingNull::MajorityClassAuthoringSplit, &cases, &split);
        let accuracy = route_accuracy_by_label(&cases, &predictions);

        assert_eq!(accuracy["execute"], 1.0);
        assert_eq!(accuracy["respond"], 0.0);
        assert_eq!(accuracy["needs_input"], 0.0);
    }

    #[test]
    fn the_always_durable_null_fails_even_the_execute_slice() {
        // Pins: strategy is part of correctness, so a constant durable predictor
        // cannot ride the execute class prior.
        let cases = corpus();
        let split = split(&cases);
        let predictions = control_predictions(RoutingNull::AlwaysDurable, &cases, &split);
        let accuracy = route_accuracy_by_label(&cases, &predictions);

        assert_eq!(accuracy["execute"], 0.0);
        assert_eq!(accuracy["respond"], 0.0);
    }

    #[test]
    fn a_constant_null_produces_a_degenerate_but_seed_backed_ceiling() {
        use crate::kernel::controls::{DEFAULT_CONTROL_ALPHA, derive_null_ceilings};

        // Pins: five seeds of a genuinely constant null yield a zero-width
        // ceiling that says so, instead of inventing slack.
        let cases = corpus();
        let split = split(&cases);
        let runs = null_seed_runs(
            RoutingNull::MajorityClassAuthoringSplit,
            &cases,
            &split,
            &[1, 2, 3, 4, 5],
        );
        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA).expect("ceilings");

        assert_eq!(ceilings["execute"].ceiling, 1.0);
        assert!(ceilings["execute"].is_degenerate());
        assert_eq!(ceilings["respond"].ceiling, 0.0);
    }

    #[test]
    fn a_well_authored_corpus_reports_no_defects() {
        assert_eq!(
            validate_routing_corpus(&corpus(), &provenance()),
            Vec::new()
        );
    }

    #[test]
    fn a_strategy_on_a_non_execute_case_is_an_impossible_case() {
        // Pins: no correct router can both refuse to execute and pick a strategy.
        let mut cases = corpus();
        cases[12].expected_strategy = Some(ExecutionStrategy::Inline);

        let defects = validate_routing_corpus(&cases, &provenance());

        assert!(
            defects.iter().any(|defect| matches!(
                defect,
                AuthoringDefect::ImpossibleCase { case_id, .. } if case_id == "respond-000"
            )),
            "defects {defects:?}"
        );
    }

    #[test]
    fn a_needs_input_case_with_no_missing_inputs_is_an_impossible_case() {
        let mut cases = corpus();
        if let ExecutionRoutingClassifierFixture::Response { output, .. } =
            &mut cases[18].classifier
        {
            output.missing_inputs.clear();
        }

        let defects = validate_routing_corpus(&cases, &provenance());

        assert!(
            defects.iter().any(|defect| matches!(
                defect,
                AuthoringDefect::ImpossibleCase { case_id, reason }
                    if case_id == "clarify-000" && reason.contains("missing inputs")
            )),
            "defects {defects:?}"
        );
    }

    #[test]
    fn an_objective_that_states_its_own_label_is_a_leak() {
        let mut cases = corpus();
        cases[18].objective = "This one needs input before anything happens".to_string();

        let defects = validate_routing_corpus(&cases, &provenance());

        assert!(
            defects.iter().any(|defect| matches!(
                defect,
                AuthoringDefect::LabelLeak { case_id, .. } if case_id == "clarify-000"
            )),
            "defects {defects:?}"
        );
    }
}
