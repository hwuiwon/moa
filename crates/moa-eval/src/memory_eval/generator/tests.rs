//! Generator behavior tests.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    CorpusProfile, SyntheticSession, SyntheticTurn, TranscriptStyle, generate_memory_eval_corpus,
};

#[test]
fn marked_restatements_stay_verbatim_and_natural_restatements_paraphrase() {
    // Pins: the marked lane keeps byte-identical restatement transcripts so
    // exact fact-hash collapse stays deterministic, while the natural lane
    // paraphrases restatements — real users rephrase, so the recorded lane
    // exercises the write-time duplicate detector instead of text equality.
    // Paraphrases must still name the canonical subject and object so
    // extraction can produce matching fact content.
    let marked =
        generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3], TranscriptStyle::Marked)
            .expect("generate PR marked corpus");
    let natural =
        generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3], TranscriptStyle::Natural)
            .expect("generate PR natural corpus");

    for corpus in [&marked, &natural] {
        let verbatim = corpus.manifest.transcript_style == TranscriptStyle::Marked;
        let turns = turns_by_fact_id(&corpus.sessions);
        let restating = corpus
            .ledger
            .iter()
            .filter(|fact| fact.restates.is_some())
            .collect::<Vec<_>>();
        assert!(restating.len() >= 10);
        for fact in restating {
            let canonical_id = fact.restates.as_deref().expect("canonical id");
            let canonical = turns
                .get(canonical_id)
                .expect("canonical turn should exist");
            let restatement = turns
                .get(fact.fact_id.as_str())
                .expect("restating turn should exist");
            if verbatim {
                assert_eq!(restatement.transcript, canonical.transcript);
            } else {
                assert_ne!(
                    restatement.transcript, canonical.transcript,
                    "natural restatement must paraphrase, not repeat"
                );
                assert!(
                    restatement.transcript.contains(&fact.subject)
                        && restatement.transcript.contains(&fact.object),
                    "paraphrase must preserve fact content: {}",
                    restatement.transcript
                );
            }
        }
    }
}

#[test]
fn probes_never_target_restating_fact_ids() {
    // Pins: restating facts exist only to be merged, not queried.
    let corpus =
        generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3], TranscriptStyle::Marked)
            .expect("generate PR marked corpus");
    let restating = corpus
        .ledger
        .iter()
        .filter(|fact| fact.restates.is_some())
        .map(|fact| fact.fact_id.as_str())
        .collect::<BTreeSet<_>>();

    assert!(restating.len() >= 10);
    for probe in &corpus.probes {
        for fact_id in probe.referenced_fact_ids() {
            assert!(!restating.contains(fact_id));
        }
    }
}

#[test]
fn generator_prior_assignment_is_deterministic_and_disjoint() {
    // Pins: synthetic quality priors mark expected facts high and colliders low without overlap.
    let first =
        generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3], TranscriptStyle::Marked)
            .expect("generate first PR corpus");
    let second =
        generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3], TranscriptStyle::Marked)
            .expect("generate second PR corpus");
    let expected = first
        .probes
        .iter()
        .flat_map(|probe| probe.expected_fact_ids.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    let first_priors = first
        .ledger
        .iter()
        .map(|fact| {
            (
                fact.fact_id.as_str(),
                (fact.prior_uses, fact.prior_successes),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let second_priors = second
        .ledger
        .iter()
        .map(|fact| {
            (
                fact.fact_id.as_str(),
                (fact.prior_uses, fact.prior_successes),
            )
        })
        .collect::<BTreeMap<_, _>>();

    assert_eq!(first_priors, second_priors);
    assert!(
        expected
            .iter()
            .all(|fact_id| first_priors.get(fact_id).copied() == Some((Some(8), Some(7))))
    );
    let low_prior_ids = first_priors
        .iter()
        .filter_map(|(fact_id, prior)| (*prior == (Some(8), Some(1))).then_some(*fact_id))
        .collect::<BTreeSet<_>>();
    assert!(!low_prior_ids.is_empty());
    assert!(low_prior_ids.is_disjoint(&expected));
    assert!(first.ledger.iter().all(|fact| {
        fact.restates.is_none()
            || first_priors.get(fact.fact_id.as_str()).copied() == Some((None, None))
    }));
}

fn turns_by_fact_id(sessions: &[SyntheticSession]) -> BTreeMap<&str, &SyntheticTurn> {
    let mut turns = BTreeMap::new();
    for session in sessions {
        for turn in &session.turns {
            for fact_id in &turn.fact_ids {
                turns.insert(fact_id.as_str(), turn);
            }
        }
    }
    turns
}
