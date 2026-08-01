//! Memory-retrieval suite controls and generated-corpus validity checks.
//!
//! Both nulls and the oracle produce ordinary [`ProbeResult`] values and are
//! then scored by [`ProbeResult::recall_at`] — the same function the candidate
//! report uses. Nothing here re-implements recall.
//!
//! The two nulls attack different artifacts:
//!
//! - **query-independent recent facts** ignores the query entirely and returns
//!   each user's newest visible facts. If the corpus happens to make recency a
//!   good answer, this null scores well and the suite is measuring recency
//!   rather than retrieval.
//! - **query permutation** keeps each probe's labels and hands it another
//!   probe's oracle candidate list. It measures how much credit the metric gives
//!   for returning *any* plausible in-scope fact.
//!
//! Generator validity is checked separately: a generated case is only fair if
//! its expected answer is not sitting in the query text and if nothing can be
//! scored before retrieval happens.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::controls::derangement;
use crate::kernel::contamination::{containment, shingles};
use crate::kernel::controls::{NullSeedRun, SliceEvidence};
use crate::memory_eval::{
    CandidateLegs, LedgerFact, Probe, ProbeResult, RETRIEVAL_EVAL_FINAL_K, RetrievedCandidate,
    stable_uuid_from_label,
};

/// Answer containment above which a probe query has copied its own answer.
pub const TRIVIAL_COPY_CONTAINMENT: f64 = 0.90;

/// Which negative control to materialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalNull {
    /// Each user's newest visible facts, chosen without reading the query.
    QueryIndependentRecentFacts,
    /// Another probe's oracle candidates scored against this probe's labels.
    QueryPermutation,
}

impl RetrievalNull {
    /// Returns the registered control id.
    #[must_use]
    pub const fn control_id(self) -> &'static str {
        match self {
            Self::QueryIndependentRecentFacts => "query_independent_recent_facts",
            Self::QueryPermutation => "query_permutation",
        }
    }
}

fn candidate(fact_id: &str, rank: usize, score: f64) -> RetrievedCandidate {
    RetrievedCandidate {
        uid: stable_uuid_from_label(fact_id),
        rank,
        score,
        similarity: None,
        lexical_evidence: None,
        fact_id: Some(fact_id.to_string()),
        equivalent_fact_ids: Vec::new(),
        legs: CandidateLegs::default(),
    }
}

fn probe_result(probe: &Probe, candidates: Vec<RetrievedCandidate>) -> ProbeResult {
    let all_expected_found_at_4 = if probe.expected_fact_ids.is_empty() {
        None
    } else {
        Some(probe.expected_fact_ids.iter().all(|expected| {
            candidates
                .iter()
                .take(RETRIEVAL_EVAL_FINAL_K)
                .any(|found| found.fact_id.as_deref() == Some(expected.as_str()))
        }))
    };
    ProbeResult {
        probe_id: probe.probe_id.clone(),
        user_id: probe.user_id.to_string(),
        probe_type: probe.probe_type,
        expected_fact_ids: probe.expected_fact_ids.clone(),
        expected_fact_grades: probe.expected_fact_grades.clone(),
        blocked_fact_ids: probe.blocked_fact_ids.clone(),
        candidates,
        post_rerank_candidates: None,
        rendered_candidate_count: None,
        retrieval_latency_ms: 0,
        all_expected_found_at_4,
        forbidden_fact_absent_at_4: None,
        stored_pii_redacted: None,
        retrieval_temporal_as_of_correct: None,
        temporal_filter_parsed: None,
        temporal_filter_matches_as_of: None,
        preference_context_hit: None,
        graph_diagnostics: None,
        graph_comparison: None,
    }
}

/// Returns the ledger facts a probe's scope can legitimately reach.
///
/// Tenant-tier facts in the same storage partition plus the probe user's own
/// contact-tier facts. A null that returned cross-user facts would be scored as
/// a privacy leak rather than as a weak retriever, which is a different finding.
#[must_use]
pub fn visible_facts<'a>(probe: &Probe, facts: &'a [LedgerFact]) -> Vec<&'a LedgerFact> {
    facts
        .iter()
        .filter(|fact| {
            fact.storage_partition_id == probe.storage_partition_id
                && (fact.scope == moa_memory_types::ScopeTier::Tenant
                    || fact.user_id == probe.user_id)
        })
        .collect()
}

/// Materializes the query-independent recent-facts null.
///
/// The seed only breaks ties between facts with identical validity instants, so
/// repeated seeds produce genuinely different runs on a corpus with ties and
/// identical runs on one without — which is the honest input to a ceiling.
#[must_use]
pub fn recent_facts_probe_results(
    probes: &[Probe],
    facts: &[LedgerFact],
    seed: u64,
    k: usize,
) -> Vec<ProbeResult> {
    probes
        .iter()
        .map(|probe| {
            let mut visible = visible_facts(probe, facts);
            visible.sort_by(|left, right| {
                right.valid_from.cmp(&left.valid_from).then_with(|| {
                    tie_break(seed, &left.fact_id).cmp(&tie_break(seed, &right.fact_id))
                })
            });
            let candidates = visible
                .into_iter()
                .take(k)
                .enumerate()
                .map(|(index, fact)| {
                    candidate(&fact.fact_id, index + 1, 1.0 - index as f64 / k as f64)
                })
                .collect();
            probe_result(probe, candidates)
        })
        .collect()
}

fn tie_break(seed: u64, fact_id: &str) -> u64 {
    let mut state = seed;
    for byte in fact_id.as_bytes() {
        state = crate::controls::splitmix64(state ^ u64::from(*byte));
    }
    state
}

/// Materializes the query-permutation null.
///
/// Probe `i` keeps its own labels and receives probe `permuted(i)`'s oracle
/// candidates. The permutation is a derangement, so no probe ever sees its own
/// answers.
#[must_use]
pub fn query_permutation_probe_results(probes: &[Probe], seed: u64) -> Vec<ProbeResult> {
    let permutation = derangement(probes.len(), seed);
    probes
        .iter()
        .enumerate()
        .map(|(index, probe)| {
            let donor = &probes[permutation[index]];
            probe_result(probe, oracle_candidates(donor))
        })
        .collect()
}

/// Returns the oracle candidate list for one probe: exactly its expected facts.
#[must_use]
pub fn oracle_candidates(probe: &Probe) -> Vec<RetrievedCandidate> {
    let mut expected = probe.expected_fact_ids.clone();
    expected.sort_by_key(|fact_id| {
        std::cmp::Reverse(
            probe
                .expected_fact_grades
                .get(fact_id)
                .copied()
                .unwrap_or(u8::MAX),
        )
    });
    expected
        .iter()
        .enumerate()
        .map(|(index, fact_id)| candidate(fact_id, index + 1, 1.0 - index as f64 / 100.0))
        .collect()
}

/// Materializes the positive/oracle control.
#[must_use]
pub fn oracle_probe_results(probes: &[Probe]) -> Vec<ProbeResult> {
    probes
        .iter()
        .map(|probe| probe_result(probe, oracle_candidates(probe)))
        .collect()
}

/// Materializes the pre-retrieval state: nothing was retrieved at all.
///
/// A generated case is only fair if it cannot be scored before retrieval runs.
#[must_use]
pub fn pre_retrieval_probe_results(probes: &[Probe]) -> Vec<ProbeResult> {
    probes
        .iter()
        .map(|probe| probe_result(probe, Vec::new()))
        .collect()
}

/// Scores recall@4 per probe type using the production per-probe scorer.
#[must_use]
pub fn recall_at_4_by_probe_type(results: &[ProbeResult]) -> BTreeMap<String, f64> {
    let mut sums: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for result in results {
        if let Some(recall) = result.recall_at(RETRIEVAL_EVAL_FINAL_K) {
            let entry = sums
                .entry(result.probe_type.as_str().to_string())
                .or_insert((0.0, 0));
            entry.0 += recall;
            entry.1 += 1;
        }
    }
    sums.into_iter()
        .map(|(slice, (total, count))| (slice, total / count as f64))
        .collect()
}

/// Builds repeated null seed runs for one negative control.
#[must_use]
pub fn null_seed_runs(
    control: RetrievalNull,
    probes: &[Probe],
    facts: &[LedgerFact],
    seeds: &[u64],
) -> Vec<NullSeedRun> {
    seeds
        .iter()
        .map(|seed| {
            let results = match control {
                RetrievalNull::QueryIndependentRecentFacts => {
                    recent_facts_probe_results(probes, facts, *seed, RETRIEVAL_EVAL_FINAL_K)
                }
                RetrievalNull::QueryPermutation => query_permutation_probe_results(probes, *seed),
            };
            NullSeedRun::new(*seed, recall_at_4_by_probe_type(&results))
        })
        .collect()
}

/// Assembles per-slice control evidence for `recall_at_4`.
///
/// `candidate_results` are the probe results the suite actually observed; they
/// are scored by the same function as the controls so the comparison is exact.
#[must_use]
pub fn recall_at_4_evidence(
    candidate_results: &[ProbeResult],
    null_results: &[ProbeResult],
    oracle_results: &[ProbeResult],
    ceilings: &BTreeMap<String, crate::kernel::controls::NullCeiling>,
    oracle_floor: f64,
) -> Vec<SliceEvidence> {
    let candidate = recall_at_4_by_probe_type(candidate_results);
    let null = recall_at_4_by_probe_type(null_results);
    let oracle = recall_at_4_by_probe_type(oracle_results);
    candidate
        .iter()
        .filter_map(|(slice, candidate_value)| {
            let ceiling = ceilings.get(slice)?;
            Some(SliceEvidence {
                slice: slice.clone(),
                candidate: *candidate_value,
                null_observed: null.get(slice).copied().unwrap_or(0.0),
                null_ceiling: ceiling.clone(),
                oracle_observed: oracle.get(slice).copied().unwrap_or(0.0),
                oracle_floor,
            })
        })
        .collect()
}

/// A way a generated corpus would make its own cases unfair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "defect", rename_all = "snake_case")]
pub enum GeneratorValidityDefect {
    /// The probe query already contains the expected answer text.
    ExpectedAnswerCopiedIntoQuery {
        /// Offending probe.
        probe_id: String,
        /// Fact whose answer text leaked into the query.
        fact_id: String,
        /// Answer containment measured in the query.
        containment: f64,
    },
    /// A probe references a fact the ledger does not contain.
    ExpectedFactMissingFromLedger {
        /// Offending probe.
        probe_id: String,
        /// Missing fact.
        fact_id: String,
    },
    /// A probe expects a fact its own scope cannot reach, so it is unsatisfiable.
    ExpectedFactOutOfScope {
        /// Offending probe.
        probe_id: String,
        /// Out-of-scope fact.
        fact_id: String,
    },
    /// A slice already scores above zero before anything is retrieved.
    PreRetrievalStateAlreadyScores {
        /// Offending slice.
        slice: String,
        /// Score observed with no candidates at all.
        score: f64,
    },
}

/// Validates that generated memory cases are fair and pre-retrieval empty.
///
/// This is the only place a generator validity check belongs: the memory corpus
/// has an initialization oracle (the ledger it was generated from), so
/// "expected facts exist, are reachable, and are not already in the query" is
/// checkable. Checked-in human-adjudicated corpora get an authoring validator
/// instead — see [`crate::controls::authoring`].
#[must_use]
pub fn validate_generator_validity(
    probes: &[Probe],
    facts: &[LedgerFact],
) -> Vec<GeneratorValidityDefect> {
    let facts_by_id = facts
        .iter()
        .map(|fact| (fact.fact_id.as_str(), fact))
        .collect::<BTreeMap<_, _>>();
    let mut defects = Vec::new();

    for probe in probes {
        let query_shingles = shingles(&probe.query);
        let visible = visible_facts(probe, facts)
            .into_iter()
            .map(|fact| fact.fact_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        for fact_id in &probe.expected_fact_ids {
            let Some(fact) = facts_by_id.get(fact_id.as_str()) else {
                defects.push(GeneratorValidityDefect::ExpectedFactMissingFromLedger {
                    probe_id: probe.probe_id.clone(),
                    fact_id: fact_id.clone(),
                });
                continue;
            };
            if !visible.contains(fact_id.as_str()) {
                defects.push(GeneratorValidityDefect::ExpectedFactOutOfScope {
                    probe_id: probe.probe_id.clone(),
                    fact_id: fact_id.clone(),
                });
            }
            let copied = containment(&query_shingles, &fact.answer);
            if copied >= TRIVIAL_COPY_CONTAINMENT {
                defects.push(GeneratorValidityDefect::ExpectedAnswerCopiedIntoQuery {
                    probe_id: probe.probe_id.clone(),
                    fact_id: fact_id.clone(),
                    containment: copied,
                });
            }
        }
    }

    for (slice, score) in recall_at_4_by_probe_type(&pre_retrieval_probe_results(probes)) {
        if score > 0.0 {
            defects.push(GeneratorValidityDefect::PreRetrievalStateAlreadyScores { slice, score });
        }
    }
    defects
}

/// Returns the stable uid a control assigns to a ledger fact id.
///
/// Exposed so a DB-lane control can align its own uid mapping with the pure
/// scorer's without duplicating the derivation.
#[must_use]
pub fn control_uid_for_fact(fact_id: &str) -> Uuid {
    stable_uuid_from_label(fact_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use moa_core::types::identifiers::{SessionId, StoragePartitionId, UserId};
    use moa_core::types::security::SensitivityClass;
    use moa_memory_types::ScopeTier;

    use crate::kernel::controls::{DEFAULT_CONTROL_ALPHA, derive_null_ceilings};
    use crate::memory_eval::ProbeType;

    fn partition() -> StoragePartitionId {
        StoragePartitionId::from("tenant-1".to_string())
    }

    fn fact(fact_id: &str, user: &str, day: u32, answer: &str) -> LedgerFact {
        LedgerFact {
            storage_partition_id: partition(),
            user_id: UserId::from(user.to_string()),
            scope: ScopeTier::Contact,
            fact_id: fact_id.to_string(),
            valid_from: Utc.with_ymd_and_hms(2026, 7, day, 0, 0, 0).unwrap(),
            valid_to: None,
            subject: "subject".to_string(),
            predicate: "predicate".to_string(),
            object: "object".to_string(),
            answer: answer.to_string(),
            supersedes: Vec::new(),
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: SessionId::new(),
            source_turn_seq: 1,
            pii_class: SensitivityClass::None,
            expected_redacted: false,
        }
    }

    fn probe(probe_id: &str, user: &str, query: &str, expected: &[&str]) -> Probe {
        Probe {
            probe_id: probe_id.to_string(),
            probe_type: ProbeType::PointRecall,
            storage_partition_id: partition(),
            user_id: UserId::from(user.to_string()),
            query: query.to_string(),
            rewrite_query: None,
            expected_rewrite: None,
            query_class: None,
            answer: "answer".to_string(),
            expected_fact_ids: expected.iter().map(|id| (*id).to_string()).collect(),
            expected_fact_grades: BTreeMap::new(),
            blocked_fact_ids: Vec::new(),
            as_of: None,
            expected_redacted: false,
        }
    }

    fn corpus() -> (Vec<Probe>, Vec<LedgerFact>) {
        let facts = vec![
            fact("f-1", "u-1", 1, "the staging cluster runs in frankfurt"),
            fact("f-2", "u-1", 2, "the on call rotation is weekly"),
            fact("f-3", "u-1", 3, "the release channel is stable"),
            fact("f-4", "u-1", 4, "the retention window is thirty days"),
            fact("f-5", "u-1", 5, "the billing owner is dana"),
            fact("f-6", "u-2", 6, "the sandbox quota is four cores"),
        ];
        let probes = vec![
            probe("p-1", "u-1", "where does staging run", &["f-1"]),
            probe("p-2", "u-1", "how often is the rotation", &["f-2"]),
            probe("p-3", "u-1", "which release channel is used", &["f-3"]),
            probe("p-4", "u-2", "what is the sandbox quota", &["f-6"]),
        ];
        (probes, facts)
    }

    #[test]
    fn the_oracle_control_reaches_full_recall_through_the_production_scorer() {
        // Pins: the positive control proves recall@4 can reach 1.0 at all, so a
        // low candidate score indicts the candidate and not the scorer.
        let (probes, _) = corpus();
        let results = oracle_probe_results(&probes);
        let by_slice = recall_at_4_by_probe_type(&results);

        assert_eq!(by_slice["point_recall"], 1.0);
        assert!(
            results
                .iter()
                .all(|result| result.all_expected_found_at_4.expect("expected facts"))
        );
    }

    #[test]
    fn the_recent_facts_null_never_reads_the_query() {
        // Pins: two probes with different queries and the same scope receive the
        // identical candidate list, which is what makes this a null.
        let (probes, facts) = corpus();
        let results = recent_facts_probe_results(&probes, &facts, 7, RETRIEVAL_EVAL_FINAL_K);

        let first = &results[0].candidates;
        let second = &results[1].candidates;
        assert_eq!(
            first.iter().map(|c| c.fact_id.clone()).collect::<Vec<_>>(),
            second.iter().map(|c| c.fact_id.clone()).collect::<Vec<_>>()
        );
        assert_eq!(first[0].fact_id.as_deref(), Some("f-5"), "newest first");
    }

    #[test]
    fn the_recent_facts_null_stays_inside_the_probes_own_scope() {
        // Pins: the null is a weak retriever, not a cross-user leak; a leak would
        // be a different finding with a different owner.
        let (probes, facts) = corpus();
        let results = recent_facts_probe_results(&probes, &facts, 3, RETRIEVAL_EVAL_FINAL_K);
        let other_user = &results[3];

        assert!(
            other_user
                .candidates
                .iter()
                .all(|found| found.fact_id.as_deref() == Some("f-6")),
            "u-2 must only see its own facts: {:?}",
            other_user.candidates
        );
    }

    #[test]
    fn the_permutation_null_scores_a_probe_against_someone_elses_candidates() {
        // Pins: no probe receives its own oracle list, so a high permutation
        // score means the metric rewards any in-scope fact.
        let (probes, _) = corpus();
        let results = query_permutation_probe_results(&probes, 11);

        for (index, result) in results.iter().enumerate() {
            let own = probes[index].expected_fact_ids[0].as_str();
            assert!(
                result
                    .candidates
                    .iter()
                    .all(|found| found.fact_id.as_deref() != Some(own)),
                "probe {} received its own fact",
                result.probe_id
            );
        }
        assert_eq!(recall_at_4_by_probe_type(&results)["point_recall"], 0.0);
    }

    #[test]
    fn repeated_null_seeds_produce_a_derivable_ceiling() {
        // Pins: the ceiling for this suite comes from five independent seeds, not
        // from a constant written into the gate.
        let (probes, facts) = corpus();
        let seeds = [1, 2, 3, 4, 5];
        let runs = null_seed_runs(
            RetrievalNull::QueryIndependentRecentFacts,
            &probes,
            &facts,
            &seeds,
        );
        assert_eq!(runs.len(), 5);

        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA).expect("ceilings");
        let ceiling = &ceilings["point_recall"];
        assert_eq!(ceiling.seeds, 5);
        assert!(ceiling.ceiling < 1.0, "ceiling {}", ceiling.ceiling);
    }

    #[test]
    fn a_query_that_copies_its_own_answer_is_a_generator_defect() {
        // Pins: generated data is only fair when the answer is not already in the
        // query text.
        let (mut probes, facts) = corpus();
        probes[0].query =
            "where does staging run the staging cluster runs in frankfurt".to_string();

        let defects = validate_generator_validity(&probes, &facts);

        assert_eq!(defects.len(), 1, "defects {defects:?}");
        assert!(matches!(
            &defects[0],
            GeneratorValidityDefect::ExpectedAnswerCopiedIntoQuery { probe_id, fact_id, .. }
                if probe_id == "p-1" && fact_id == "f-1"
        ));
    }

    #[test]
    fn a_fair_generated_corpus_reports_no_generator_defects() {
        // Pins: the validator is not a blanket alarm on ordinary word overlap.
        let (probes, facts) = corpus();
        assert_eq!(validate_generator_validity(&probes, &facts), Vec::new());
    }

    #[test]
    fn an_unreachable_or_absent_expected_fact_is_a_generator_defect() {
        // Pins: an impossible case cannot sit in the corpus lowering every score.
        let (mut probes, facts) = corpus();
        probes.push(probe("p-5", "u-2", "who owns billing", &["f-5"]));
        probes.push(probe("p-6", "u-1", "what about nothing", &["f-99"]));

        let defects = validate_generator_validity(&probes, &facts);

        assert!(
            defects.contains(&GeneratorValidityDefect::ExpectedFactOutOfScope {
                probe_id: "p-5".to_string(),
                fact_id: "f-5".to_string()
            })
        );
        assert!(
            defects.contains(&GeneratorValidityDefect::ExpectedFactMissingFromLedger {
                probe_id: "p-6".to_string(),
                fact_id: "f-99".to_string()
            })
        );
    }

    #[test]
    fn the_pre_retrieval_state_scores_zero_on_every_slice() {
        // Pins: nothing is scoreable before retrieval, so the candidate's score
        // is attributable to retrieval.
        let (probes, _) = corpus();
        let scores = recall_at_4_by_probe_type(&pre_retrieval_probe_results(&probes));

        assert!(
            scores.values().all(|score| *score == 0.0),
            "pre-retrieval scores {scores:?}"
        );
    }
}
