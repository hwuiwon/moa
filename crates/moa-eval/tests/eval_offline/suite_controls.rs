//! Suite control validity over the real checked-in and generated corpora.
//!
//! The per-module unit tests prove each control behaves on synthetic inputs.
//! These tests run the same controls against the corpora that actually gate:
//! the 328-case routing corpus, the 20-query golden fixture, and a freshly
//! generated PR memory corpus.

use std::path::{Path, PathBuf};

use moa_eval::controls::authoring::{AuthoringDefect, AuthoringSplit, DEFAULT_AUTHORING_FRACTION};
use moa_eval::controls::execution_routing::{
    RoutingNull, control_predictions, manifest_provenance, oracle_predictions,
    route_accuracy_by_label, validate_routing_corpus,
};
use moa_eval::controls::golden_graph::{
    AGGREGATE_SLICE, GOLDEN_TOP_K, GoldenNull, GoldenQueryFixture, control_rankings,
    oracle_rankings, recall_evidence, recall_slices,
};
use moa_eval::controls::memory_retrieval::{
    RetrievalNull, oracle_probe_results, recall_at_4_by_probe_type, validate_generator_validity,
};
use moa_eval::controls::{
    SUITE_EXECUTION_ROUTING, SUITE_GOLDEN_GRAPH, SUITE_MEMORY_RETRIEVAL, controls_for,
    validate_registry,
};
use moa_eval::execution::corpus::load_execution_corpus;
use moa_eval::kernel::controls::{
    ControlLane, ControlRole, ControlledMetric, DEFAULT_CONTROL_ALPHA, DEFAULT_ORACLE_FLOOR,
    MIN_NULL_SEEDS, SuiteVerdict, audit_controlled_metric, derive_null_ceilings,
};
use moa_eval::memory_eval::{CorpusProfile, TranscriptStyle, generate_memory_eval_corpus};

const NULL_SEEDS: [u64; MIN_NULL_SEEDS] = [11, 22, 33, 44, 55];

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn execution_manifest() -> PathBuf {
    crate_dir().join("scenarios/execution/manifest.toml")
}

fn golden_fixture() -> PathBuf {
    crate_dir().join("tests/fixtures/golden_queries.json")
}

#[test]
fn every_registered_suite_control_declares_its_real_lane_offline() {
    // Pins: no control in the registry claims a pure-scorer lane while needing
    // Postgres, and every suite keeps both control sides.
    assert_eq!(validate_registry(), Vec::new());

    let db_controls = moa_eval::controls::SUITE_CONTROLS
        .iter()
        .filter(|control| control.lane == ControlLane::DatabaseIntegration)
        .count();
    assert_eq!(
        db_controls, 1,
        "exactly one DB-lane control is wired today (the empty-store pre-retrieval null)"
    );
}

#[tokio::test]
async fn the_routing_corpus_passes_its_authoring_validator() {
    // Pins: all 328 checked-in routing cases have hash-pinned provenance, no
    // duplicate or contradictory objectives, no self-labeling objective, and no
    // structurally impossible expectation. This replaces a fictional solution
    // function with a real authoring check.
    let corpus = load_execution_corpus(&execution_manifest())
        .await
        .expect("execution corpus loads and byte-verifies");
    let provenance = manifest_provenance(
        &corpus.manifest.routing.path.display().to_string(),
        &corpus.manifest.routing.sha256,
    );

    let defects = validate_routing_corpus(&corpus.routing_cases, &provenance);

    assert_eq!(
        defects,
        Vec::<AuthoringDefect>::new(),
        "routing corpus authoring defects"
    );
    assert_eq!(corpus.routing_cases.len(), 328);
}

#[tokio::test]
async fn routing_controls_bracket_the_corpus_from_both_sides() {
    // Pins: the oracle reaches 1.0 in every label slice and the majority-class
    // null is bounded by a seed-derived ceiling well under it, so route accuracy
    // has both a floor for the scorer and a ceiling for the class prior.
    let corpus = load_execution_corpus(&execution_manifest())
        .await
        .expect("execution corpus loads");
    let cases = &corpus.routing_cases;
    let split = AuthoringSplit::derive(
        SUITE_EXECUTION_ROUTING,
        cases.iter().map(|case| case.case_id.as_str()),
        DEFAULT_AUTHORING_FRACTION,
    );
    assert!(split.is_disjoint());
    assert!(!split.authoring.is_empty() && !split.gated.is_empty());

    let oracle = route_accuracy_by_label(cases, &oracle_predictions(cases));
    assert!(
        oracle.values().all(|value| *value >= DEFAULT_ORACLE_FLOOR),
        "oracle accuracy {oracle:?}"
    );
    assert!(
        oracle.len() >= 3,
        "expected respond/execute/needs_input slices"
    );

    for control in [
        RoutingNull::MajorityClassAuthoringSplit,
        RoutingNull::AlwaysDurable,
    ] {
        let runs = moa_eval::controls::execution_routing::null_seed_runs(
            control,
            cases,
            &split,
            &NULL_SEEDS,
        );
        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA)
            .unwrap_or_else(|error| panic!("{} ceilings: {error}", control.control_id()));
        let observed = route_accuracy_by_label(cases, &control_predictions(control, cases, &split));
        let overall = observed.values().sum::<f64>() / observed.len() as f64;
        assert!(
            overall < 0.75,
            "{} averaged {overall} across slices, which is too strong to be a null",
            control.control_id()
        );
        for (slice, ceiling) in &ceilings {
            assert!(
                observed[slice] <= ceiling.ceiling,
                "{} exceeded its own ceiling in {slice}",
                control.control_id()
            );
        }
    }
}

#[test]
fn golden_controls_bracket_the_checked_in_query_fixture() {
    // Pins: the 20 labeled golden queries have a positive control at 1.0 per query
    // and both nulls bounded per query and in aggregate. The aggregate slice is the
    // one with enough support to carry a decision: a single query holds five labels
    // drawn from a small alias pool, so a popularity prior can saturate individual
    // queries by chance. Those slices are reported as uninformative rather than
    // trusted, and the audit refuses to let a candidate clear them.
    let bytes = std::fs::read(golden_fixture()).expect("golden query fixture is readable");
    let fixture: GoldenQueryFixture =
        serde_json::from_slice(&bytes).expect("golden query fixture parses");
    let cases = fixture.cases();
    assert_eq!(cases.len(), 20);

    let split = AuthoringSplit::derive(
        SUITE_GOLDEN_GRAPH,
        cases.iter().map(|case| case.query_id.as_str()),
        DEFAULT_AUTHORING_FRACTION,
    );

    let oracle = recall_slices(&cases, &oracle_rankings(&cases, GOLDEN_TOP_K), GOLDEN_TOP_K);
    assert_eq!(
        oracle.len(),
        cases.len() + 1,
        "per-query slices plus the aggregate are reported"
    );
    assert!(
        oracle.values().all(|recall| *recall == 1.0),
        "oracle recall {oracle:?}"
    );

    for control in [GoldenNull::PopularLabelPrior, GoldenNull::QueryPermutation] {
        let runs = moa_eval::controls::golden_graph::null_seed_runs(
            control,
            &cases,
            &split,
            &NULL_SEEDS,
            GOLDEN_TOP_K,
        );
        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA)
            .unwrap_or_else(|error| panic!("{} ceilings: {error}", control.control_id()));
        assert_eq!(ceilings.len(), cases.len() + 1);

        let aggregate = &ceilings[AGGREGATE_SLICE];
        assert!(
            aggregate.ceiling < DEFAULT_ORACLE_FLOOR,
            "{} aggregate ceiling {} leaves no margin over the oracle",
            control.control_id(),
            aggregate.ceiling
        );
        assert!(!aggregate.is_uninformative());

        let observed = recall_slices(
            &cases,
            &control_rankings(control, &cases, &split, NULL_SEEDS[0], GOLDEN_TOP_K),
            GOLDEN_TOP_K,
        );
        for (slice, ceiling) in &ceilings {
            assert!(
                observed[slice] <= ceiling.ceiling,
                "{} exceeded its ceiling in {slice}",
                control.control_id()
            );
        }

        // Any saturated per-query slice must be surfaced as invalid evidence, even
        // when the candidate scores a perfect 1.0 there.
        let uninformative = ceilings
            .values()
            .filter(|ceiling| ceiling.is_uninformative())
            .count();
        let report = audit_controlled_metric(&ControlledMetric {
            suite: SUITE_GOLDEN_GRAPH.to_string(),
            metric: "expected_uid_recall_at_5".to_string(),
            candidate_overall: 1.0,
            slices: recall_evidence(
                &cases,
                &oracle_rankings(&cases, GOLDEN_TOP_K),
                &control_rankings(control, &cases, &split, NULL_SEEDS[0], GOLDEN_TOP_K),
                &ceilings,
                DEFAULT_ORACLE_FLOOR,
                GOLDEN_TOP_K,
            ),
        });
        let invalid = report.invalid_slices().count();
        assert_eq!(
            invalid,
            uninformative,
            "{}: {uninformative} saturated slice(s) must be exactly the invalid ones",
            control.control_id()
        );
        if uninformative == 0 {
            assert_eq!(report.verdict, SuiteVerdict::Valid);
            assert_eq!(report.headline_score(), Some(1.0));
        } else {
            assert_eq!(report.verdict, SuiteVerdict::InvalidSuite);
            assert_eq!(report.headline_score(), None);
            assert_eq!(report.candidate_overall, 1.0, "never adjusted");
        }
    }
}

#[test]
fn the_generated_memory_corpus_is_fair_and_pre_retrieval_empty() {
    // Pins: generated probes never carry their own answer text, every expected
    // fact is in the probe's own scope, and nothing scores before retrieval runs.
    let corpus =
        generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3], TranscriptStyle::Marked)
            .expect("PR corpus generates");

    let defects = validate_generator_validity(&corpus.probes, &corpus.ledger);

    assert_eq!(defects, Vec::new(), "generator validity defects");
}

#[test]
fn memory_retrieval_controls_bracket_the_generated_corpus_per_probe_type() {
    // Pins: the oracle reaches 1.0 in every probe-type slice and both nulls stay
    // under seed-derived ceilings, so recall@4 measures retrieval rather than
    // recency or in-scope plausibility.
    let corpus =
        generate_memory_eval_corpus(CorpusProfile::Pr, vec![4, 5, 6], TranscriptStyle::Marked)
            .expect("PR corpus generates");

    let oracle = recall_at_4_by_probe_type(&oracle_probe_results(&corpus.probes));
    assert!(!oracle.is_empty(), "oracle produced no slices");
    assert!(
        oracle
            .values()
            .all(|recall| *recall >= DEFAULT_ORACLE_FLOOR),
        "oracle recall by probe type {oracle:?}"
    );

    for control in [
        RetrievalNull::QueryIndependentRecentFacts,
        RetrievalNull::QueryPermutation,
    ] {
        let runs = moa_eval::controls::memory_retrieval::null_seed_runs(
            control,
            &corpus.probes,
            &corpus.ledger,
            &NULL_SEEDS,
        );
        assert_eq!(runs.len(), MIN_NULL_SEEDS);
        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA)
            .unwrap_or_else(|error| panic!("{} ceilings: {error}", control.control_id()));
        for (slice, ceiling) in &ceilings {
            assert!(
                ceiling.ceiling < DEFAULT_ORACLE_FLOOR,
                "{} ceiling in {slice} is {} which leaves no room for a real signal",
                control.control_id(),
                ceiling.ceiling
            );
        }
    }
}

#[test]
fn every_control_in_the_registry_has_a_module_level_implementation() {
    // Pins: the registry cannot advertise a control nothing produces. Each id is
    // matched against the control-kind enums that materialize them.
    let implemented = [
        RetrievalNull::QueryIndependentRecentFacts.control_id(),
        RetrievalNull::QueryPermutation.control_id(),
        "oracle_expected_facts",
        "empty_store_pre_retrieval",
        GoldenNull::PopularLabelPrior.control_id(),
        GoldenNull::QueryPermutation.control_id(),
        "oracle_expected_uids",
        RoutingNull::MajorityClassAuthoringSplit.control_id(),
        RoutingNull::AlwaysDurable.control_id(),
        "manifest_expected_route",
        "fixed_plausible_response",
        "scripted_state_correct_trajectory",
        moa_eval::controls::external_memory::ExternalMemoryNull::NoMemory.control_id(),
        moa_eval::controls::external_memory::ExternalMemoryNull::QueryIndependentAnswer
            .control_id(),
        "oracle_evidence",
        moa_eval::controls::fixed_rag::FixedRagNull::PopularInCorpus.control_id(),
        moa_eval::controls::fixed_rag::FixedRagNull::RandomInCorpus.control_id(),
        moa_eval::controls::fixed_rag::FixedRagNull::QuestionPermutation.control_id(),
        "pinned_source_documents",
    ];

    for control in moa_eval::controls::SUITE_CONTROLS {
        assert!(
            implemented.contains(&control.control_id),
            "{} is registered but has no implementation",
            control.control_id
        );
    }
    assert_eq!(
        controls_for(SUITE_MEMORY_RETRIEVAL)
            .filter(|control| control.role == ControlRole::NegativeNull)
            .count(),
        3
    );
    assert!(Path::new(&execution_manifest()).exists());
}
