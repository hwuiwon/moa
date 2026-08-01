//! Golden graph-memory suite controls.
//!
//! The golden fixture set publishes, per query, the exact uid set that must
//! appear in the top five. That gives a clean positive control (rank exactly the
//! expected set) and two nulls.
//!
//! The plan names "highest-degree nodes" as the popularity null. Node degree
//! only exists after ingestion, inside the DB lane, and it is a *proxy* for what
//! actually makes a popularity null strong: how often a node is a correct answer.
//! This module therefore fits the prior on **label frequency in the authoring
//! split** — strictly more adversarial than degree, computable in the same lane
//! as the scorer, and fitted only on cases that do not gate.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::controls::authoring::AuthoringSplit;
use crate::controls::derangement;
use crate::kernel::controls::{NullCeiling, NullSeedRun, SliceEvidence};

/// Top-k window the golden suite asserts over.
pub const GOLDEN_TOP_K: usize = 5;

/// One labeled golden retrieval query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldenQueryCase {
    /// Stable query identity derived from fixture order.
    pub query_id: String,
    /// Query text.
    pub query: String,
    /// Uid aliases that must appear in the top window.
    pub expected_uids: Vec<String>,
}

/// Golden query fixture document.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GoldenQueryFixture {
    /// Labeled retrieval queries.
    pub queries: Vec<GoldenQueryEntry>,
}

/// One raw fixture entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GoldenQueryEntry {
    /// Query text.
    pub query: String,
    /// Expected uid aliases in the top five.
    pub expected_top_5_uids: Vec<String>,
}

impl GoldenQueryFixture {
    /// Converts fixture entries into stable, identified cases.
    #[must_use]
    pub fn cases(&self) -> Vec<GoldenQueryCase> {
        self.queries
            .iter()
            .enumerate()
            .map(|(index, entry)| GoldenQueryCase {
                query_id: format!("q-{:02}", index + 1),
                query: entry.query.clone(),
                expected_uids: entry.expected_top_5_uids.clone(),
            })
            .collect()
    }
}

/// Which negative control to materialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoldenNull {
    /// The authoring split's most frequently labeled uids, for every query.
    PopularLabelPrior,
    /// Another query's expected uids, scored against this query's labels.
    QueryPermutation,
}

impl GoldenNull {
    /// Returns the registered control id.
    #[must_use]
    pub const fn control_id(self) -> &'static str {
        match self {
            Self::PopularLabelPrior => "popular_label_prior",
            Self::QueryPermutation => "query_permutation",
        }
    }
}

/// Fits the popularity prior on the authoring split only.
///
/// Returns uids ordered by how often they are a correct answer in the authoring
/// split, with ties broken by uid so the prior is deterministic. Gated cases are
/// never counted: a prior fitted on the gate would make the null artificially
/// strong and shrink the candidate's measured margin.
#[must_use]
pub fn label_popularity_prior(cases: &[GoldenQueryCase], split: &AuthoringSplit) -> Vec<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for case in cases
        .iter()
        .filter(|case| split.is_authoring(&case.query_id))
    {
        for uid in &case.expected_uids {
            *counts.entry(uid.as_str()).or_insert(0) += 1;
        }
    }
    let mut ranked = counts.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    ranked.into_iter().map(|(uid, _)| uid.to_string()).collect()
}

/// Returns one control's ranked uid list per case.
///
/// `seed` rotates the popularity prior's tail and selects the permutation, so
/// repeated seeds are genuinely independent null runs.
#[must_use]
pub fn control_rankings(
    control: GoldenNull,
    cases: &[GoldenQueryCase],
    split: &AuthoringSplit,
    seed: u64,
    k: usize,
) -> Vec<Vec<String>> {
    match control {
        GoldenNull::PopularLabelPrior => {
            let prior = label_popularity_prior(cases, split);
            if prior.is_empty() {
                return cases.iter().map(|_| Vec::new()).collect();
            }
            let offset = (crate::controls::splitmix64(seed) % prior.len() as u64) as usize;
            let rotated = prior
                .iter()
                .cycle()
                .skip(offset)
                .take(k.min(prior.len()))
                .cloned()
                .collect::<Vec<_>>();
            cases.iter().map(|_| rotated.clone()).collect()
        }
        GoldenNull::QueryPermutation => {
            let permutation = derangement(cases.len(), seed);
            cases
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    cases[permutation[index]]
                        .expected_uids
                        .iter()
                        .take(k)
                        .cloned()
                        .collect()
                })
                .collect()
        }
    }
}

/// Returns the oracle ranking: exactly the query's expected uid set.
#[must_use]
pub fn oracle_rankings(cases: &[GoldenQueryCase], k: usize) -> Vec<Vec<String>> {
    cases
        .iter()
        .map(|case| case.expected_uids.iter().take(k).cloned().collect())
        .collect()
}

/// Scores one ranked uid list against a query's expected set.
///
/// The metric the golden suite asserts on is coverage of the expected set inside
/// the top window; both the candidate and every control are scored here.
#[must_use]
pub fn expected_uid_recall_at(ranked: &[String], expected: &[String], k: usize) -> f64 {
    if expected.is_empty() {
        return 0.0;
    }
    let window = ranked.iter().take(k).collect::<BTreeSet<_>>();
    let found = expected.iter().filter(|uid| window.contains(uid)).count() as f64;
    found / expected.len() as f64
}

/// Slice key holding the mean over every query.
pub const AGGREGATE_SLICE: &str = "all";

/// Scores every case, keyed by query id.
#[must_use]
pub fn recall_by_query(
    cases: &[GoldenQueryCase],
    rankings: &[Vec<String>],
    k: usize,
) -> BTreeMap<String, f64> {
    cases
        .iter()
        .zip(rankings)
        .map(|(case, ranked)| {
            (
                case.query_id.clone(),
                expected_uid_recall_at(ranked, &case.expected_uids, k),
            )
        })
        .collect()
}

/// Scores every case plus the aggregate mean over all queries.
///
/// Per-query slices are diagnostics with one case each, so a popularity prior can
/// saturate an individual query by chance. The aggregate slice is the one with
/// enough support to carry a decision, and both are reported.
#[must_use]
pub fn recall_slices(
    cases: &[GoldenQueryCase],
    rankings: &[Vec<String>],
    k: usize,
) -> BTreeMap<String, f64> {
    let mut slices = recall_by_query(cases, rankings, k);
    if !slices.is_empty() {
        let mean = slices.values().sum::<f64>() / slices.len() as f64;
        slices.insert(AGGREGATE_SLICE.to_string(), mean);
    }
    slices
}

/// Builds repeated null seed runs for one negative control.
#[must_use]
pub fn null_seed_runs(
    control: GoldenNull,
    cases: &[GoldenQueryCase],
    split: &AuthoringSplit,
    seeds: &[u64],
    k: usize,
) -> Vec<NullSeedRun> {
    seeds
        .iter()
        .map(|seed| {
            let rankings = control_rankings(control, cases, split, *seed, k);
            NullSeedRun::new(*seed, recall_slices(cases, &rankings, k))
        })
        .collect()
}

/// Assembles per-query control evidence.
#[must_use]
pub fn recall_evidence(
    cases: &[GoldenQueryCase],
    candidate_rankings: &[Vec<String>],
    null_rankings: &[Vec<String>],
    ceilings: &BTreeMap<String, NullCeiling>,
    oracle_floor: f64,
    k: usize,
) -> Vec<SliceEvidence> {
    let candidate = recall_slices(cases, candidate_rankings, k);
    let null = recall_slices(cases, null_rankings, k);
    let oracle = recall_slices(cases, &oracle_rankings(cases, k), k);
    candidate
        .iter()
        .filter_map(|(slice, value)| {
            let ceiling = ceilings.get(slice)?;
            Some(SliceEvidence {
                slice: slice.clone(),
                candidate: *value,
                null_observed: null.get(slice).copied().unwrap_or(0.0),
                null_ceiling: ceiling.clone(),
                oracle_observed: oracle.get(slice).copied().unwrap_or(0.0),
                oracle_floor,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controls::authoring::DEFAULT_AUTHORING_FRACTION;

    fn cases() -> Vec<GoldenQueryCase> {
        (0..8)
            .map(|index| GoldenQueryCase {
                query_id: format!("q-{:02}", index + 1),
                query: format!("query {index}"),
                expected_uids: vec![
                    format!("fact-{:02}", index * 2 + 1),
                    format!("fact-{:02}", index * 2 + 2),
                ],
            })
            .collect()
    }

    fn split(cases: &[GoldenQueryCase]) -> AuthoringSplit {
        AuthoringSplit::derive(
            "golden",
            cases.iter().map(|case| case.query_id.as_str()),
            DEFAULT_AUTHORING_FRACTION,
        )
    }

    #[test]
    fn the_oracle_ranking_reaches_full_recall_on_every_query() {
        // Pins: the positive control proves the metric can reach 1.0.
        let cases = cases();
        let rankings = oracle_rankings(&cases, GOLDEN_TOP_K);
        let by_query = recall_by_query(&cases, &rankings, GOLDEN_TOP_K);

        assert!(
            by_query.values().all(|recall| *recall == 1.0),
            "oracle recall {by_query:?}"
        );
    }

    #[test]
    fn the_popularity_prior_is_fitted_only_on_the_authoring_split() {
        // Pins: the null never learns from a gated case, so it cannot be tuned by
        // the data it is supposed to bound.
        let cases = cases();
        let split = split(&cases);
        let prior = label_popularity_prior(&cases, &split);

        let gated_only = cases
            .iter()
            .filter(|case| split.is_gated(&case.query_id))
            .flat_map(|case| case.expected_uids.iter().cloned())
            .collect::<BTreeSet<_>>();
        let authoring_only = cases
            .iter()
            .filter(|case| split.is_authoring(&case.query_id))
            .flat_map(|case| case.expected_uids.iter().cloned())
            .collect::<BTreeSet<_>>();
        assert!(
            !authoring_only.is_empty(),
            "split produced no authoring cases"
        );
        for uid in &prior {
            assert!(
                authoring_only.contains(uid),
                "prior contains {uid} which is not an authoring label"
            );
            assert!(
                !gated_only.contains(uid) || authoring_only.contains(uid),
                "prior leaked a gated-only label"
            );
        }
    }

    #[test]
    fn the_permutation_null_never_returns_a_querys_own_labels() {
        let cases = cases();
        let split = split(&cases);
        let rankings = control_rankings(
            GoldenNull::QueryPermutation,
            &cases,
            &split,
            5,
            GOLDEN_TOP_K,
        );

        for (case, ranked) in cases.iter().zip(&rankings) {
            assert!(
                case.expected_uids.iter().all(|uid| !ranked.contains(uid)),
                "{} received its own labels",
                case.query_id
            );
        }
        assert!(
            recall_by_query(&cases, &rankings, GOLDEN_TOP_K)
                .values()
                .all(|recall| *recall == 0.0)
        );
    }

    #[test]
    fn a_scorer_that_ignores_rank_order_is_caught_by_the_window() {
        // Pins: recall is computed over the top-k window only, so padding the
        // tail with the right answers does not score.
        let expected = vec!["fact-01".to_string(), "fact-02".to_string()];
        let padded = (0..GOLDEN_TOP_K)
            .map(|index| format!("noise-{index}"))
            .chain(expected.iter().cloned())
            .collect::<Vec<_>>();

        assert_eq!(
            expected_uid_recall_at(&padded, &expected, GOLDEN_TOP_K),
            0.0
        );
        assert_eq!(
            expected_uid_recall_at(&expected, &expected, GOLDEN_TOP_K),
            1.0
        );
    }

    #[test]
    fn repeated_seeds_yield_a_derivable_ceiling_per_query() {
        use crate::kernel::controls::{DEFAULT_CONTROL_ALPHA, derive_null_ceilings};

        let cases = cases();
        let split = split(&cases);
        let runs = null_seed_runs(
            GoldenNull::PopularLabelPrior,
            &cases,
            &split,
            &[1, 2, 3, 4, 5],
            GOLDEN_TOP_K,
        );

        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA).expect("ceilings");
        assert_eq!(
            ceilings.len(),
            cases.len() + 1,
            "per-query slices plus the aggregate"
        );
        assert!(
            ceilings.values().all(|ceiling| ceiling.seeds == 5),
            "every query ceiling needs five seeds"
        );
    }
}
