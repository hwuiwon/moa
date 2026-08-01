//! Per-suite negative/null and positive/oracle controls.
//!
//! [`crate::kernel::controls`] owns the suite-agnostic math: how a ceiling is
//! derived and how a metric is audited. This module owns the part that cannot be
//! generic — what a null system *is* for each suite — plus the registry that
//! makes an uncontrolled headline metric impossible to add quietly.
//!
//! Every control here produces the same thing its candidate produces (a ranked
//! candidate list, a predicted label, an answer, an evidence envelope) and is
//! then scored by the suite's own scorer. That is deliberate: a control that
//! carried its own scoring arithmetic could pass while the production scorer was
//! broken, which is exactly the failure the positive control exists to catch.

use serde::{Deserialize, Serialize};

use crate::kernel::contamination::{LaneClass, LaneClassification};
use crate::kernel::controls::{
    ControlLane, ControlRole, DEFAULT_CONTROL_ALPHA, DEFAULT_ORACLE_FLOOR, MIN_NULL_SEEDS,
};

pub mod authoring;
pub mod execution_routing;
pub mod external_memory;
pub mod fixed_rag;
pub mod golden_graph;
pub mod long_conversation;
pub mod memory_retrieval;

/// Suite id for the memory retrieval eval.
pub const SUITE_MEMORY_RETRIEVAL: &str = "memory_retrieval";
/// Suite id for the golden graph-memory eval.
pub const SUITE_GOLDEN_GRAPH: &str = "golden_graph";
/// Suite id for the execution routing corpus.
pub const SUITE_EXECUTION_ROUTING: &str = "execution_routing";
/// Suite id for the long-conversation scenarios.
pub const SUITE_LONG_CONVERSATION: &str = "long_conversation";
/// Suite id for the external-memory benchmark harness.
pub const SUITE_EXTERNAL_MEMORY: &str = "external_memory";
/// Suite id for the WixQA fixed-corpus RAG lane.
pub const SUITE_WIXQA_RAG: &str = "wixqa_rag";

/// How a control's threshold was established.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CeilingSource {
    /// Derived from repeated independent null seeds at a stated error rate.
    RepeatedNullSeeds {
        /// Independent seeds behind the ceiling.
        seeds: usize,
        /// One-sided error rate.
        alpha: f64,
    },
    /// A floor the oracle must clear for the scorer to be considered intact.
    OracleFloor {
        /// Required floor.
        floor: f64,
    },
}

/// One registered control for one suite metric.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SuiteControl {
    /// Suite this control belongs to.
    pub suite: &'static str,
    /// Headline metric the control constrains.
    pub metric: &'static str,
    /// Stable control identity.
    pub control_id: &'static str,
    /// Which side of validity the control proves.
    pub role: ControlRole,
    /// Runtime the control genuinely needs.
    pub lane: ControlLane,
    /// Dimension the metric is sliced by.
    pub slice_key: &'static str,
    /// How the control's threshold was established.
    pub ceiling_source: CeilingSource,
    /// What the control does, in one line.
    pub description: &'static str,
}

/// Every registered suite control.
///
/// The table is the contract: [`validate_registry`] refuses a metric that has
/// only one side of the pair, a null whose ceiling is not seed-derived, or a
/// control that claims fewer seeds than [`MIN_NULL_SEEDS`].
pub const SUITE_CONTROLS: &[SuiteControl] = &[
    SuiteControl {
        suite: SUITE_MEMORY_RETRIEVAL,
        metric: "recall_at_4",
        control_id: "query_independent_recent_facts",
        role: ControlRole::NegativeNull,
        lane: ControlLane::PureScorer,
        slice_key: "probe_type",
        ceiling_source: CeilingSource::RepeatedNullSeeds {
            seeds: MIN_NULL_SEEDS,
            alpha: DEFAULT_CONTROL_ALPHA,
        },
        description: "ranks each user's most recent facts without reading the query",
    },
    SuiteControl {
        suite: SUITE_MEMORY_RETRIEVAL,
        metric: "recall_at_4",
        control_id: "query_permutation",
        role: ControlRole::NegativeNull,
        lane: ControlLane::PureScorer,
        slice_key: "probe_type",
        ceiling_source: CeilingSource::RepeatedNullSeeds {
            seeds: MIN_NULL_SEEDS,
            alpha: DEFAULT_CONTROL_ALPHA,
        },
        description: "scores each probe's labels against another probe's oracle candidates",
    },
    SuiteControl {
        suite: SUITE_MEMORY_RETRIEVAL,
        metric: "recall_at_4",
        control_id: "oracle_expected_facts",
        role: ControlRole::PositiveOracle,
        lane: ControlLane::PureScorer,
        slice_key: "probe_type",
        ceiling_source: CeilingSource::OracleFloor {
            floor: DEFAULT_ORACLE_FLOOR,
        },
        description: "ranks exactly the probe's expected fact ids",
    },
    SuiteControl {
        suite: SUITE_MEMORY_RETRIEVAL,
        metric: "recall_at_4",
        control_id: "empty_store_pre_retrieval",
        role: ControlRole::NegativeNull,
        lane: ControlLane::DatabaseIntegration,
        slice_key: "probe_type",
        ceiling_source: CeilingSource::RepeatedNullSeeds {
            seeds: MIN_NULL_SEEDS,
            alpha: DEFAULT_CONTROL_ALPHA,
        },
        description: "queries a real tenant with nothing ingested, proving cases are \
            unanswerable before retrieval",
    },
    SuiteControl {
        suite: SUITE_GOLDEN_GRAPH,
        metric: "expected_uid_recall_at_5",
        control_id: "popular_label_prior",
        role: ControlRole::NegativeNull,
        lane: ControlLane::PureScorer,
        slice_key: "query_id",
        ceiling_source: CeilingSource::RepeatedNullSeeds {
            seeds: MIN_NULL_SEEDS,
            alpha: DEFAULT_CONTROL_ALPHA,
        },
        description: "ranks the most frequently labeled nodes of the authoring split",
    },
    SuiteControl {
        suite: SUITE_GOLDEN_GRAPH,
        metric: "expected_uid_recall_at_5",
        control_id: "query_permutation",
        role: ControlRole::NegativeNull,
        lane: ControlLane::PureScorer,
        slice_key: "query_id",
        ceiling_source: CeilingSource::RepeatedNullSeeds {
            seeds: MIN_NULL_SEEDS,
            alpha: DEFAULT_CONTROL_ALPHA,
        },
        description: "scores each query's labels against another query's expected uids",
    },
    SuiteControl {
        suite: SUITE_GOLDEN_GRAPH,
        metric: "expected_uid_recall_at_5",
        control_id: "oracle_expected_uids",
        role: ControlRole::PositiveOracle,
        lane: ControlLane::PureScorer,
        slice_key: "query_id",
        ceiling_source: CeilingSource::OracleFloor {
            floor: DEFAULT_ORACLE_FLOOR,
        },
        description: "ranks exactly the query's expected uid set",
    },
    SuiteControl {
        suite: SUITE_EXECUTION_ROUTING,
        metric: "route_accuracy",
        control_id: "majority_class_authoring_split",
        role: ControlRole::NegativeNull,
        lane: ControlLane::PureScorer,
        slice_key: "expected_label",
        ceiling_source: CeilingSource::RepeatedNullSeeds {
            seeds: MIN_NULL_SEEDS,
            alpha: DEFAULT_CONTROL_ALPHA,
        },
        description: "predicts the authoring split's majority label for every case",
    },
    SuiteControl {
        suite: SUITE_EXECUTION_ROUTING,
        metric: "route_accuracy",
        control_id: "always_durable",
        role: ControlRole::NegativeNull,
        lane: ControlLane::PureScorer,
        slice_key: "expected_label",
        ceiling_source: CeilingSource::RepeatedNullSeeds {
            seeds: MIN_NULL_SEEDS,
            alpha: DEFAULT_CONTROL_ALPHA,
        },
        description: "predicts execute with the durable strategy for every case",
    },
    SuiteControl {
        suite: SUITE_EXECUTION_ROUTING,
        metric: "route_accuracy",
        control_id: "manifest_expected_route",
        role: ControlRole::PositiveOracle,
        lane: ControlLane::PureScorer,
        slice_key: "expected_label",
        ceiling_source: CeilingSource::OracleFloor {
            floor: DEFAULT_ORACLE_FLOOR,
        },
        description: "replays the corpus manifest's adjudicated route and strategy",
    },
    SuiteControl {
        suite: SUITE_LONG_CONVERSATION,
        metric: "blocking_assertion_pass_rate",
        control_id: "fixed_plausible_response",
        role: ControlRole::NegativeNull,
        lane: ControlLane::MockDomain,
        slice_key: "assertion_category",
        ceiling_source: CeilingSource::RepeatedNullSeeds {
            seeds: MIN_NULL_SEEDS,
            alpha: DEFAULT_CONTROL_ALPHA,
        },
        description: "answers with a plausible fixed report and touches nothing",
    },
    SuiteControl {
        suite: SUITE_LONG_CONVERSATION,
        metric: "blocking_assertion_pass_rate",
        control_id: "scripted_state_correct_trajectory",
        role: ControlRole::PositiveOracle,
        lane: ControlLane::MockDomain,
        slice_key: "assertion_category",
        ceiling_source: CeilingSource::OracleFloor {
            floor: DEFAULT_ORACLE_FLOOR,
        },
        description: "walks a scripted path that reaches the correct world state",
    },
    SuiteControl {
        suite: SUITE_EXTERNAL_MEMORY,
        metric: "answer_accuracy",
        control_id: "no_memory",
        role: ControlRole::NegativeNull,
        lane: ControlLane::PureScorer,
        slice_key: "category",
        ceiling_source: CeilingSource::RepeatedNullSeeds {
            seeds: MIN_NULL_SEEDS,
            alpha: DEFAULT_CONTROL_ALPHA,
        },
        description: "answers with no evidence rendered at all",
    },
    SuiteControl {
        suite: SUITE_EXTERNAL_MEMORY,
        metric: "answer_accuracy",
        control_id: "query_independent_answer",
        role: ControlRole::NegativeNull,
        lane: ControlLane::PureScorer,
        slice_key: "category",
        ceiling_source: CeilingSource::RepeatedNullSeeds {
            seeds: MIN_NULL_SEEDS,
            alpha: DEFAULT_CONTROL_ALPHA,
        },
        description: "picks the authoring split's most frequent option, ignoring the question",
    },
    SuiteControl {
        suite: SUITE_EXTERNAL_MEMORY,
        metric: "answer_accuracy",
        control_id: "oracle_evidence",
        role: ControlRole::PositiveOracle,
        lane: ControlLane::PureScorer,
        slice_key: "category",
        ceiling_source: CeilingSource::OracleFloor {
            floor: DEFAULT_ORACLE_FLOOR,
        },
        description: "answers from the dataset's own gold evidence turns",
    },
    SuiteControl {
        suite: SUITE_WIXQA_RAG,
        metric: "recall_at_k",
        control_id: "popular_in_corpus",
        role: ControlRole::NegativeNull,
        lane: ControlLane::PureScorer,
        slice_key: "gold_cardinality",
        ceiling_source: CeilingSource::RepeatedNullSeeds {
            seeds: MIN_NULL_SEEDS,
            alpha: DEFAULT_CONTROL_ALPHA,
        },
        description: "returns the authoring split's most frequently labeled articles",
    },
    SuiteControl {
        suite: SUITE_WIXQA_RAG,
        metric: "recall_at_k",
        control_id: "random_in_corpus",
        role: ControlRole::NegativeNull,
        lane: ControlLane::PureScorer,
        slice_key: "gold_cardinality",
        ceiling_source: CeilingSource::RepeatedNullSeeds {
            seeds: MIN_NULL_SEEDS,
            alpha: DEFAULT_CONTROL_ALPHA,
        },
        description: "returns a seeded random in-corpus article sample",
    },
    SuiteControl {
        suite: SUITE_WIXQA_RAG,
        metric: "recall_at_k",
        control_id: "question_permutation",
        role: ControlRole::NegativeNull,
        lane: ControlLane::PureScorer,
        slice_key: "gold_cardinality",
        ceiling_source: CeilingSource::RepeatedNullSeeds {
            seeds: MIN_NULL_SEEDS,
            alpha: DEFAULT_CONTROL_ALPHA,
        },
        description: "returns another question's gold articles",
    },
    SuiteControl {
        suite: SUITE_WIXQA_RAG,
        metric: "recall_at_k",
        control_id: "pinned_source_documents",
        role: ControlRole::PositiveOracle,
        lane: ControlLane::PureScorer,
        slice_key: "gold_cardinality",
        ceiling_source: CeilingSource::OracleFloor {
            floor: DEFAULT_ORACLE_FLOOR,
        },
        description: "returns exactly the question's pinned gold article ids",
    },
];

/// Contamination classification for every eval lane.
///
/// WixQA is the load-bearing entry: it retrieves from a seeded tenant Postgres
/// corpus, so it requires a package-leakage scan.
pub const LANE_CLASSIFICATIONS: &[LaneClassification] = &[
    LaneClassification {
        lane: SUITE_MEMORY_RETRIEVAL,
        class: LaneClass::FixedCorpusRetrieval,
        network_denied: true,
        rationale: "retrieves from a generated corpus ingested into an isolated eval tenant",
    },
    LaneClassification {
        lane: SUITE_GOLDEN_GRAPH,
        class: LaneClass::FixedCorpusRetrieval,
        network_denied: true,
        rationale: "retrieves from 100 checked-in graph fixtures in an isolated tenant",
    },
    LaneClassification {
        lane: SUITE_WIXQA_RAG,
        class: LaneClass::FixedCorpusRetrieval,
        network_denied: true,
        rationale: "closed WixQA knowledge base seeded into tenant Postgres; no search-time \
            web path exists, so retrieving a source article is expected behavior",
    },
    LaneClassification {
        lane: SUITE_EXTERNAL_MEMORY,
        class: LaneClass::PublicAnswerGeneration,
        network_denied: true,
        rationale: "public LongMemEval and PersonaMem packages pinned by canonical hash",
    },
    LaneClassification {
        lane: SUITE_EXECUTION_ROUTING,
        class: LaneClass::ClosedFixtureSuite,
        network_denied: true,
        rationale: "checked-in labeled cases with a scripted provider; no corpus is retrieved",
    },
    LaneClassification {
        lane: SUITE_LONG_CONVERSATION,
        class: LaneClass::ClosedFixtureSuite,
        network_denied: true,
        rationale: "scripted and recorded scenarios; any web result is a frozen recording",
    },
];

/// A defect in the control or lane registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "defect", rename_all = "snake_case")]
pub enum RegistryDefect {
    /// A metric has no negative/null control.
    MissingNegativeControl {
        /// Suite id.
        suite: String,
        /// Metric id.
        metric: String,
    },
    /// A metric has no positive/oracle control.
    MissingPositiveControl {
        /// Suite id.
        suite: String,
        /// Metric id.
        metric: String,
    },
    /// A null control's ceiling is not derived from enough seeds.
    UnderpoweredCeiling {
        /// Control id.
        control_id: String,
        /// Seeds claimed.
        seeds: usize,
    },
    /// A control's threshold kind does not match its role.
    MismatchedCeilingSource {
        /// Control id.
        control_id: String,
    },
    /// Two controls share an id inside one suite metric.
    DuplicateControlId {
        /// Control id.
        control_id: String,
    },
    /// A suite has controls but no lane classification.
    UnclassifiedLane {
        /// Suite id.
        suite: String,
    },
    /// A lane classification contradicts itself.
    InvalidLaneClassification {
        /// Lane id.
        lane: String,
        /// Why it is invalid.
        reason: String,
    },
}

/// Returns every control registered for one suite.
pub fn controls_for(suite: &str) -> impl Iterator<Item = &'static SuiteControl> {
    SUITE_CONTROLS
        .iter()
        .filter(move |control| control.suite == suite)
}

/// Returns the classification for one lane.
#[must_use]
pub fn lane_classification(lane: &str) -> Option<&'static LaneClassification> {
    LANE_CLASSIFICATIONS
        .iter()
        .find(|classification| classification.lane == lane)
}

/// Validates that every registered metric carries a complete control pair.
#[must_use]
pub fn validate_registry() -> Vec<RegistryDefect> {
    use std::collections::{BTreeMap, BTreeSet};

    let mut defects = Vec::new();
    let mut by_metric: BTreeMap<(&str, &str), Vec<&SuiteControl>> = BTreeMap::new();
    for control in SUITE_CONTROLS {
        by_metric
            .entry((control.suite, control.metric))
            .or_default()
            .push(control);
        match (control.role, control.ceiling_source) {
            (ControlRole::NegativeNull, CeilingSource::RepeatedNullSeeds { seeds, .. }) => {
                if seeds < MIN_NULL_SEEDS {
                    defects.push(RegistryDefect::UnderpoweredCeiling {
                        control_id: control.control_id.to_string(),
                        seeds,
                    });
                }
            }
            (ControlRole::PositiveOracle, CeilingSource::OracleFloor { .. }) => {}
            _ => defects.push(RegistryDefect::MismatchedCeilingSource {
                control_id: control.control_id.to_string(),
            }),
        }
    }

    for ((suite, metric), controls) in &by_metric {
        let mut ids = BTreeSet::new();
        for control in controls {
            if !ids.insert(control.control_id) {
                defects.push(RegistryDefect::DuplicateControlId {
                    control_id: control.control_id.to_string(),
                });
            }
        }
        if !controls
            .iter()
            .any(|control| control.role == ControlRole::NegativeNull)
        {
            defects.push(RegistryDefect::MissingNegativeControl {
                suite: (*suite).to_string(),
                metric: (*metric).to_string(),
            });
        }
        if !controls
            .iter()
            .any(|control| control.role == ControlRole::PositiveOracle)
        {
            defects.push(RegistryDefect::MissingPositiveControl {
                suite: (*suite).to_string(),
                metric: (*metric).to_string(),
            });
        }
        if lane_classification(suite).is_none() {
            defects.push(RegistryDefect::UnclassifiedLane {
                suite: (*suite).to_string(),
            });
        }
    }

    for classification in LANE_CLASSIFICATIONS {
        if let Err(error) = classification.validate() {
            defects.push(RegistryDefect::InvalidLaneClassification {
                lane: classification.lane.to_string(),
                reason: error.to_string(),
            });
        }
    }
    defects
}

/// Deterministic 64-bit mixer used by seeded controls.
///
/// Controls need reproducible pseudo-randomness with a recorded seed, not
/// cryptographic quality. Keeping the mixer here means every suite's random and
/// permutation control draws from the same documented stream.
#[must_use]
pub const fn splitmix64(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Deterministically permutes `0..len` under a seed, never fixing an index.
///
/// A permutation null that maps a case to itself leaks the real label, so the
/// rotation offset is forced to be non-zero for `len > 1`.
#[must_use]
pub fn derangement(len: usize, seed: u64) -> Vec<usize> {
    if len <= 1 {
        return (0..len).collect();
    }
    let offset = 1 + (splitmix64(seed) % (len as u64 - 1)) as usize;
    (0..len).map(|index| (index + offset) % len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_metric_has_both_control_sides_and_a_classified_lane() {
        // Pins: the whole point of the registry. A headline metric cannot be
        // added with only a null, only an oracle, or no lane classification.
        assert_eq!(validate_registry(), Vec::new());
    }

    #[test]
    fn the_i4_suite_table_is_fully_covered() {
        // Pins: all six suites named in the plan's control table are registered,
        // so a suite cannot quietly drop out of the program.
        for suite in [
            SUITE_MEMORY_RETRIEVAL,
            SUITE_GOLDEN_GRAPH,
            SUITE_EXECUTION_ROUTING,
            SUITE_LONG_CONVERSATION,
            SUITE_EXTERNAL_MEMORY,
            SUITE_WIXQA_RAG,
        ] {
            let controls = controls_for(suite).collect::<Vec<_>>();
            assert!(
                controls
                    .iter()
                    .any(|control| control.role == ControlRole::NegativeNull),
                "{suite} has no negative control"
            );
            assert!(
                controls
                    .iter()
                    .any(|control| control.role == ControlRole::PositiveOracle),
                "{suite} has no positive control"
            );
            assert!(
                controls.iter().all(|control| !control.slice_key.is_empty()),
                "{suite} has a control with no slice dimension"
            );
        }
    }

    #[test]
    fn wixqa_is_a_closed_corpus_and_requires_a_package_scan() {
        // Pins: WixQA is fixed-corpus and requires a package-leakage scan.
        let wixqa = lane_classification(SUITE_WIXQA_RAG).expect("wixqa is classified");
        assert_eq!(wixqa.class, LaneClass::FixedCorpusRetrieval);
        assert!(wixqa.network_denied);
        assert!(wixqa.requires_leakage_scan());
    }

    #[test]
    fn database_backed_controls_are_not_advertised_as_offline() {
        // Pins: the empty-store control needs Postgres and says so.
        let empty_store = SUITE_CONTROLS
            .iter()
            .find(|control| control.control_id == "empty_store_pre_retrieval")
            .expect("registered");
        assert!(empty_store.lane.requires_postgres());

        for control in SUITE_CONTROLS {
            if control.lane == ControlLane::PureScorer {
                assert!(
                    !control.lane.requires_postgres(),
                    "{} claims a pure lane but needs postgres",
                    control.control_id
                );
            }
        }
    }

    #[test]
    fn a_derangement_never_maps_an_index_to_itself() {
        // Pins: a permutation null cannot accidentally score a case against its
        // own labels.
        for seed in 0..64_u64 {
            for len in 2..17_usize {
                let permutation = derangement(len, seed);
                assert_eq!(permutation.len(), len);
                assert!(
                    permutation
                        .iter()
                        .enumerate()
                        .all(|(index, mapped)| index != *mapped),
                    "len {len} seed {seed} has a fixed point: {permutation:?}"
                );
                let unique = permutation
                    .iter()
                    .copied()
                    .collect::<std::collections::BTreeSet<_>>();
                assert_eq!(unique.len(), len, "not a permutation");
            }
        }
        assert_eq!(derangement(1, 7), vec![0]);
        assert!(derangement(0, 7).is_empty());
    }

    #[test]
    fn different_seeds_produce_different_permutations() {
        // Pins: repeated null seeds really are different runs.
        let permutations = (0..8_u64)
            .map(|seed| derangement(9, seed))
            .collect::<std::collections::BTreeSet<_>>();
        assert!(permutations.len() > 1, "seeds produced one permutation");
    }
}
