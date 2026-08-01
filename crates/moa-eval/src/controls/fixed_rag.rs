//! Fixed-corpus RAG controls, used by the WixQA lane.
//!
//! WixQA is a closed corpus: articles are seeded into an isolated tenant and
//! retrieved from Postgres. There is no web-search surface, so retrieving a gold
//! article is expected behavior rather than contamination — the leakage question
//! for this lane is whether the *corpus* contains question/answer artifacts, which
//! [`crate::kernel::contamination`] answers.
//!
//! What the controls answer is different: how much of the reported recall a
//! retriever earns without reading the question. Three nulls, all producing ranked
//! article-id lists that the lane's own metric function scores:
//!
//! - **popular in corpus** returns the authoring split's most frequently labeled
//!   articles;
//! - **random in corpus** returns a seeded sample, which is the chance level for
//!   this corpus size and window;
//! - **question permutation** returns another question's gold articles, measuring
//!   credit for returning any plausible article.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::controls::authoring::AuthoringSplit;
use crate::controls::derangement;
use crate::kernel::controls::{NullCeiling, NullSeedRun, SliceEvidence};

/// One labeled fixed-corpus retrieval question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixedRagQuestion {
    /// Stable question identity.
    pub question_id: String,
    /// Question text, recorded for provenance only; controls never read it.
    pub question: String,
    /// Gold source-object ids for this question.
    pub gold_object_ids: Vec<String>,
}

impl FixedRagQuestion {
    /// Returns the slice key: how many source objects the question needs.
    ///
    /// Single-source and multi-source questions have structurally different
    /// recall ceilings, so a blended mean hides which kind regressed.
    #[must_use]
    pub fn slice_key(&self) -> &'static str {
        if self.gold_object_ids.len() <= 1 {
            "single_source"
        } else {
            "multi_source"
        }
    }
}

/// Which negative control to materialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixedRagNull {
    /// The authoring split's most frequently labeled objects.
    PopularInCorpus,
    /// A seeded random in-corpus sample.
    RandomInCorpus,
    /// Another question's gold objects.
    QuestionPermutation,
}

impl FixedRagNull {
    /// Returns the registered control id.
    #[must_use]
    pub const fn control_id(self) -> &'static str {
        match self {
            Self::PopularInCorpus => "popular_in_corpus",
            Self::RandomInCorpus => "random_in_corpus",
            Self::QuestionPermutation => "question_permutation",
        }
    }
}

/// Fits the popularity prior on the authoring split only.
#[must_use]
pub fn popularity_prior(questions: &[FixedRagQuestion], split: &AuthoringSplit) -> Vec<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for question in questions
        .iter()
        .filter(|question| split.is_authoring(&question.question_id))
    {
        for object_id in &question.gold_object_ids {
            *counts.entry(object_id.as_str()).or_insert(0) += 1;
        }
    }
    let mut ranked = counts.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    ranked
        .into_iter()
        .map(|(object_id, _)| object_id.to_string())
        .collect()
}

/// Returns one ranked object-id list per question for a negative control.
///
/// `corpus_object_ids` must be the full pinned corpus, sorted, so the random
/// control draws from the real object space rather than from the labels.
#[must_use]
pub fn control_rankings(
    control: FixedRagNull,
    questions: &[FixedRagQuestion],
    corpus_object_ids: &[String],
    split: &AuthoringSplit,
    seed: u64,
    top_k: usize,
) -> Vec<Vec<String>> {
    match control {
        FixedRagNull::PopularInCorpus => {
            let prior = popularity_prior(questions, split);
            if prior.is_empty() {
                return questions.iter().map(|_| Vec::new()).collect();
            }
            let offset = (crate::controls::splitmix64(seed) % prior.len() as u64) as usize;
            let ranked = prior
                .iter()
                .cycle()
                .skip(offset)
                .take(top_k.min(prior.len()))
                .cloned()
                .collect::<Vec<_>>();
            questions.iter().map(|_| ranked.clone()).collect()
        }
        FixedRagNull::RandomInCorpus => questions
            .iter()
            .enumerate()
            .map(|(index, _)| {
                if corpus_object_ids.is_empty() {
                    return Vec::new();
                }
                let mut state = crate::controls::splitmix64(seed ^ index as u64);
                let mut picked = Vec::new();
                let mut seen = std::collections::BTreeSet::new();
                while picked.len() < top_k.min(corpus_object_ids.len()) {
                    state = crate::controls::splitmix64(state);
                    let candidate = (state % corpus_object_ids.len() as u64) as usize;
                    if seen.insert(candidate) {
                        picked.push(corpus_object_ids[candidate].clone());
                    }
                }
                picked
            })
            .collect(),
        FixedRagNull::QuestionPermutation => {
            let permutation = derangement(questions.len(), seed);
            questions
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    questions[permutation[index]]
                        .gold_object_ids
                        .iter()
                        .take(top_k)
                        .cloned()
                        .collect()
                })
                .collect()
        }
    }
}

/// Returns the oracle ranking: exactly the question's pinned gold objects.
#[must_use]
pub fn oracle_rankings(questions: &[FixedRagQuestion], top_k: usize) -> Vec<Vec<String>> {
    questions
        .iter()
        .map(|question| {
            question
                .gold_object_ids
                .iter()
                .take(top_k)
                .cloned()
                .collect()
        })
        .collect()
}

/// Scores recall@k for one ranked list.
///
/// Deliberately identical arithmetic to the lane's own per-query recall so a
/// control and the candidate are never scored differently.
#[must_use]
pub fn recall_at(ranked: &[String], gold: &[String], top_k: usize) -> f64 {
    if gold.is_empty() {
        return 0.0;
    }
    let window = ranked
        .iter()
        .take(top_k)
        .collect::<std::collections::BTreeSet<_>>();
    let matched = gold.iter().filter(|id| window.contains(id)).count() as f64;
    matched / gold.len() as f64
}

/// Scores recall@k per gold-cardinality slice.
#[must_use]
pub fn recall_by_slice(
    questions: &[FixedRagQuestion],
    rankings: &[Vec<String>],
    top_k: usize,
) -> BTreeMap<String, f64> {
    let mut totals: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for (question, ranked) in questions.iter().zip(rankings) {
        let entry = totals
            .entry(question.slice_key().to_string())
            .or_insert((0.0, 0));
        entry.0 += recall_at(ranked, &question.gold_object_ids, top_k);
        entry.1 += 1;
    }
    totals
        .into_iter()
        .map(|(slice, (total, count))| (slice, total / count as f64))
        .collect()
}

/// Builds repeated null seed runs for one negative control.
#[must_use]
pub fn null_seed_runs(
    control: FixedRagNull,
    questions: &[FixedRagQuestion],
    corpus_object_ids: &[String],
    split: &AuthoringSplit,
    seeds: &[u64],
    top_k: usize,
) -> Vec<NullSeedRun> {
    seeds
        .iter()
        .map(|seed| {
            let rankings =
                control_rankings(control, questions, corpus_object_ids, split, *seed, top_k);
            NullSeedRun::new(*seed, recall_by_slice(questions, &rankings, top_k))
        })
        .collect()
}

/// Assembles per-slice control evidence for observed candidate rankings.
#[must_use]
pub fn recall_evidence(
    questions: &[FixedRagQuestion],
    candidate_rankings: &[Vec<String>],
    null_rankings: &[Vec<String>],
    ceilings: &BTreeMap<String, NullCeiling>,
    oracle_floor: f64,
    top_k: usize,
) -> Vec<SliceEvidence> {
    let candidate = recall_by_slice(questions, candidate_rankings, top_k);
    let null = recall_by_slice(questions, null_rankings, top_k);
    let oracle = recall_by_slice(questions, &oracle_rankings(questions, top_k), top_k);
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
    use crate::kernel::controls::{DEFAULT_CONTROL_ALPHA, derive_null_ceilings};

    const TOP_K: usize = 10;

    fn corpus_ids() -> Vec<String> {
        (0..200).map(|index| format!("kb-{index:03}")).collect()
    }

    fn questions() -> Vec<FixedRagQuestion> {
        (0..40)
            .map(|index| {
                let gold = if index % 4 == 0 {
                    vec![format!("kb-{index:03}"), format!("kb-{:03}", index + 1)]
                } else {
                    vec![format!("kb-{index:03}")]
                };
                FixedRagQuestion {
                    question_id: format!("q-{index:03}"),
                    question: format!("question {index}"),
                    gold_object_ids: gold,
                }
            })
            .collect()
    }

    fn split(questions: &[FixedRagQuestion]) -> AuthoringSplit {
        AuthoringSplit::derive(
            "wixqa_rag",
            questions.iter().map(|q| q.question_id.as_str()),
            DEFAULT_AUTHORING_FRACTION,
        )
    }

    #[test]
    fn the_oracle_control_reaches_full_recall_in_both_slices() {
        let questions = questions();
        let recall = recall_by_slice(&questions, &oracle_rankings(&questions, TOP_K), TOP_K);

        assert_eq!(recall["single_source"], 1.0);
        assert_eq!(recall["multi_source"], 1.0);
    }

    #[test]
    fn the_random_null_is_near_chance_and_varies_by_seed() {
        // Pins: the chance level for a 200-object corpus at k=10 is small, and the
        // ceiling is derived from that spread rather than asserted.
        let questions = questions();
        let split = split(&questions);
        let ids = corpus_ids();
        let runs = null_seed_runs(
            FixedRagNull::RandomInCorpus,
            &questions,
            &ids,
            &split,
            &[1, 2, 3, 4, 5],
            TOP_K,
        );
        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA).expect("ceilings");

        assert_eq!(ceilings.len(), 2);
        for ceiling in ceilings.values() {
            assert!(
                ceiling.ceiling < 0.30,
                "random null ceiling should be near chance: {ceiling:?}"
            );
        }
        assert!(
            ceilings.values().any(|ceiling| !ceiling.is_degenerate()),
            "random draws must vary across seeds"
        );
    }

    #[test]
    fn the_popularity_null_is_fitted_on_the_authoring_split_only() {
        let questions = questions();
        let split = split(&questions);
        let prior = popularity_prior(&questions, &split);
        let authoring_labels = questions
            .iter()
            .filter(|question| split.is_authoring(&question.question_id))
            .flat_map(|question| question.gold_object_ids.iter().cloned())
            .collect::<std::collections::BTreeSet<_>>();

        assert!(!prior.is_empty());
        for object_id in &prior {
            assert!(
                authoring_labels.contains(object_id),
                "{object_id} is not an authoring label"
            );
        }
    }

    #[test]
    fn the_permutation_null_never_returns_a_questions_own_gold_objects() {
        let questions = questions();
        let split = split(&questions);
        let rankings = control_rankings(
            FixedRagNull::QuestionPermutation,
            &questions,
            &corpus_ids(),
            &split,
            9,
            TOP_K,
        );

        for (question, ranked) in questions.iter().zip(&rankings) {
            assert!(
                question
                    .gold_object_ids
                    .iter()
                    .all(|gold| !ranked.contains(gold)),
                "{} received its own gold objects",
                question.question_id
            );
        }
    }

    #[test]
    fn a_retriever_that_only_reproduces_the_popularity_prior_fails_the_audit() {
        use crate::kernel::controls::{
            ControlledMetric, DEFAULT_ORACLE_FLOOR, SuiteVerdict, audit_controlled_metric,
        };

        let questions = questions();
        let split = split(&questions);
        let ids = corpus_ids();
        let null_rankings = control_rankings(
            FixedRagNull::PopularInCorpus,
            &questions,
            &ids,
            &split,
            1,
            TOP_K,
        );
        let runs = null_seed_runs(
            FixedRagNull::PopularInCorpus,
            &questions,
            &ids,
            &split,
            &[1, 2, 3, 4, 5],
            TOP_K,
        );
        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA).expect("ceilings");

        let mimic = audit_controlled_metric(&ControlledMetric {
            suite: crate::controls::SUITE_WIXQA_RAG.to_string(),
            metric: "recall_at_k".to_string(),
            candidate_overall: 0.10,
            slices: recall_evidence(
                &questions,
                &null_rankings,
                &null_rankings,
                &ceilings,
                DEFAULT_ORACLE_FLOOR,
                TOP_K,
            ),
        });
        assert_eq!(mimic.verdict, SuiteVerdict::InvalidSuite);

        let real = audit_controlled_metric(&ControlledMetric {
            suite: crate::controls::SUITE_WIXQA_RAG.to_string(),
            metric: "recall_at_k".to_string(),
            candidate_overall: 0.90,
            slices: recall_evidence(
                &questions,
                &oracle_rankings(&questions, TOP_K),
                &null_rankings,
                &ceilings,
                DEFAULT_ORACLE_FLOOR,
                TOP_K,
            ),
        });
        assert_eq!(real.verdict, SuiteVerdict::Valid);
        assert_eq!(real.headline_score(), Some(0.90));
    }
}
