//! `xtask eval-suite-controls` command implementation.
//!
//! Runs every control that can execute outside a live database and writes one
//! suite-validity report. The exit status is the gate: a registry with a missing
//! control side, an authoring defect in a checked-in corpus, a null above its
//! derived ceiling, or an oracle below its floor all fail the command.
//!
//! The report always carries the candidate score unchanged. When a suite is
//! invalid, `headline_score` is `null` rather than a null-corrected number.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use moa_eval::controls::authoring::{AuthoringDefect, AuthoringSplit, DEFAULT_AUTHORING_FRACTION};
use moa_eval::controls::{
    CeilingSource, LANE_CLASSIFICATIONS, SUITE_CONTROLS, SUITE_EXECUTION_ROUTING,
    SUITE_EXTERNAL_MEMORY, SUITE_GOLDEN_GRAPH, SUITE_LONG_CONVERSATION, SUITE_MEMORY_RETRIEVAL,
    SUITE_WIXQA_RAG, SuiteControl, execution_routing, external_memory, fixed_rag, golden_graph,
    lane_classification, long_conversation, memory_retrieval, validate_registry,
};
use moa_eval::execution::corpus::load_execution_corpus;
use moa_eval::external_memory::dataset::load_common_json;
use moa_eval::kernel::contamination::{
    ArtifactKind, CaseSplit, ContaminationError, CorpusObject, EvalCaseText, LeakageFinding,
    LeakageScanner, PinnedCorpus, SourceProvenance, sha256_text,
};
use moa_eval::kernel::controls::{
    ControlLane, ControlRole, DEFAULT_CONTROL_ALPHA, DEFAULT_ORACLE_FLOOR, MIN_NULL_SEEDS,
    NullCeiling, derive_null_ceilings,
};
use moa_eval::memory_eval::{
    CorpusProfile, RETRIEVAL_EVAL_FINAL_K, TranscriptStyle, generate_memory_eval_corpus,
};
use serde::{Deserialize, Serialize};

const DEFAULT_OUTPUT: &str = "target/eval-controls/suite-controls.json";
const NULL_SEEDS: [u64; MIN_NULL_SEEDS] = [11, 22, 33, 44, 55];
const FIXED_RAG_TOP_K: usize = 10;
const SUITE_OWNED_LEAKAGE_LANES: [&str; 3] = [
    SUITE_MEMORY_RETRIEVAL,
    SUITE_GOLDEN_GRAPH,
    SUITE_EXTERNAL_MEMORY,
];
const FIXED_RAG_SCANNER_FIXTURE: &str = "hermetic_fixed_rag_scanner_fixture";
const PACKAGE_LEAKAGE_SCOPE: &str = "command_owned_corpora_plus_labeled_scanner_fixtures";

/// Whether a registered control ran in this command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ControlExecutionStatus {
    /// The control ran and produced slice evidence.
    Executed,
    /// The control belongs to the Postgres-backed lane owned by its suite.
    SkippedRequiresDatabase,
}

/// One control's derived thresholds and observed score per slice.
#[derive(Debug, Clone, Serialize)]
struct ControlOutcome {
    suite: String,
    metric: String,
    control_id: String,
    role: ControlRole,
    lane: ControlLane,
    status: ControlExecutionStatus,
    seeds: Vec<u64>,
    alpha: f64,
    slices: BTreeMap<String, ControlSlice>,
    violations: Vec<String>,
}

/// One slice's derived ceiling and the score the control actually reached.
#[derive(Debug, Clone, Serialize)]
struct ControlSlice {
    observed: f64,
    ceiling: Option<f64>,
    floor: Option<f64>,
    seed_mean: Option<f64>,
    seed_std_dev: Option<f64>,
    degenerate: Option<bool>,
}

/// Whole-command report.
#[derive(Debug, Clone, Serialize)]
struct SuiteControlsReport {
    schema_version: u8,
    generated_at: String,
    registry_defects: Vec<serde_json::Value>,
    execution_defects: Vec<String>,
    lanes: Vec<serde_json::Value>,
    authoring_defects: BTreeMap<String, Vec<AuthoringDefect>>,
    controls: Vec<ControlOutcome>,
    package_leakage_scope: &'static str,
    leakage_execution_defects: Vec<String>,
    leakage_scans: Vec<LeakageOutcome>,
}

/// Typed result of one required lane's package-leakage scan.
#[derive(Debug, Clone, Serialize)]
struct LeakageOutcome {
    #[serde(flatten)]
    coverage: LeakageCoverage,
    corpus_id: String,
    objects_scanned: usize,
    cases_scanned: usize,
    blocking_findings: Vec<LeakageFinding>,
    informational_findings: Vec<LeakageFinding>,
}

/// Whether a scan fulfills this command's corpus obligation or only tests the scanner.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "coverage", rename_all = "snake_case")]
enum LeakageCoverage {
    /// Evidence for a real corpus this command loads itself.
    RequiredLane { lane: String },
    /// Hermetic scanner evidence that does not fulfill any lane's corpus scan.
    ScannerFixture { fixture_id: String },
}

impl LeakageCoverage {
    fn required_lane(&self) -> Option<&str> {
        match self {
            Self::RequiredLane { lane } => Some(lane),
            Self::ScannerFixture { .. } => None,
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::RequiredLane { lane } => lane,
            Self::ScannerFixture { fixture_id } => fixture_id,
        }
    }
}

impl LeakageOutcome {
    fn is_valid(&self) -> bool {
        self.blocking_findings.is_empty()
    }
}

struct Options {
    output: PathBuf,
    help: bool,
}

impl Options {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self> {
        let mut output = PathBuf::from(DEFAULT_OUTPUT);
        let mut help = false;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--out" | "--output" => {
                    output = PathBuf::from(args.next().context("--out requires a path")?);
                }
                "--help" | "-h" => help = true,
                other => bail!("unknown eval-suite-controls argument: {other}"),
            }
        }
        Ok(Self { output, help })
    }
}

fn print_help() {
    println!(
        "xtask eval-suite-controls [--out <path>]\n\n\
         Runs the pure-scorer and mock-domain suite controls, derives every null\n\
         ceiling from repeated seeds, and fails when a control misbehaves.\n\
         Database-lane controls run in their own suite lane (db-memory) and are\n\
         reported here as skipped_requires_database."
    );
}

/// Runs the suite-controls command.
pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<()> {
    let options = Options::parse(args)?;
    if options.help {
        print_help();
        return Ok(());
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build Tokio runtime for suite controls")?;
    let report = runtime.block_on(build_report())?;
    write_report(&options.output, &report)?;

    let failures = report
        .controls
        .iter()
        .flat_map(|control| control.violations.iter().cloned())
        .collect::<Vec<_>>();
    let leakage_failures = report
        .leakage_scans
        .iter()
        .filter(|outcome| !outcome.is_valid())
        .count();
    let authoring_failures = report
        .authoring_defects
        .iter()
        .filter(|(_, defects)| !defects.is_empty())
        .count();

    println!(
        "wrote suite control report: output={} controls={} leakage_scans={} registry_defects={} execution_defects={} leakage_execution_defects={} authoring_suites_with_defects={} violations={} leakage_failures={}",
        options.output.display(),
        report.controls.len(),
        report.leakage_scans.len(),
        report.registry_defects.len(),
        report.execution_defects.len(),
        report.leakage_execution_defects.len(),
        authoring_failures,
        failures.len(),
        leakage_failures,
    );
    if !report.registry_defects.is_empty() {
        bail!(
            "control registry is incomplete: {}",
            serde_json::to_string(&report.registry_defects)?
        );
    }
    if !report.execution_defects.is_empty() {
        bail!(
            "registered control execution is incomplete: {}",
            report.execution_defects.join("; ")
        );
    }
    if !report.leakage_execution_defects.is_empty() {
        bail!(
            "required leakage scan execution is incomplete: {}",
            report.leakage_execution_defects.join("; ")
        );
    }
    if authoring_failures > 0 {
        bail!(
            "checked-in corpus authoring defects: {}",
            serde_json::to_string(&report.authoring_defects)?
        );
    }
    if !failures.is_empty() {
        bail!("suite controls failed: {}", failures.join("; "));
    }
    if leakage_failures > 0 {
        bail!("{leakage_failures} required leakage scan(s) failed");
    }
    Ok(())
}

async fn build_report() -> Result<SuiteControlsReport> {
    let mut controls = Vec::new();
    let mut leakage_scans = Vec::new();
    let mut authoring_defects = BTreeMap::new();

    // Execution routing: checked-in corpus, authoring validator plus both nulls.
    let manifest = repo_path("crates/moa-eval/scenarios/execution/manifest.toml");
    let corpus = load_execution_corpus(&manifest)
        .await
        .with_context(|| format!("load execution corpus {}", manifest.display()))?;
    let provenance = execution_routing::manifest_provenance(
        &corpus.manifest.routing.path.display().to_string(),
        &corpus.manifest.routing.sha256,
    );
    authoring_defects.insert(
        SUITE_EXECUTION_ROUTING.to_string(),
        execution_routing::validate_routing_corpus(&corpus.routing_cases, &provenance),
    );
    let routing_split = AuthoringSplit::derive(
        SUITE_EXECUTION_ROUTING,
        corpus
            .routing_cases
            .iter()
            .map(|case| case.case_id.as_str()),
        DEFAULT_AUTHORING_FRACTION,
    );
    for null in [
        execution_routing::RoutingNull::MajorityClassAuthoringSplit,
        execution_routing::RoutingNull::AlwaysDurable,
    ] {
        let runs = execution_routing::null_seed_runs(
            null,
            &corpus.routing_cases,
            &routing_split,
            &NULL_SEEDS,
        );
        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA)
            .with_context(|| format!("derive ceilings for {}", null.control_id()))?;
        let observed = execution_routing::route_accuracy_by_label(
            &corpus.routing_cases,
            &execution_routing::control_predictions(null, &corpus.routing_cases, &routing_split),
        );
        controls.push(null_outcome(
            SUITE_EXECUTION_ROUTING,
            "route_accuracy",
            null.control_id(),
            &ceilings,
            &observed,
        )?);
    }
    controls.push(oracle_outcome(
        SUITE_EXECUTION_ROUTING,
        "route_accuracy",
        "manifest_expected_route",
        &execution_routing::route_accuracy_by_label(
            &corpus.routing_cases,
            &execution_routing::oracle_predictions(&corpus.routing_cases),
        ),
    )?);

    // Golden graph: checked-in query fixture.
    let golden_path = repo_path("crates/moa-eval/tests/fixtures/golden_queries.json");
    let golden_bytes =
        std::fs::read(&golden_path).with_context(|| format!("read {}", golden_path.display()))?;
    let fixture: golden_graph::GoldenQueryFixture = serde_json::from_slice(&golden_bytes)
        .with_context(|| format!("parse {}", golden_path.display()))?;
    let golden_cases = fixture.cases();
    let golden_split = AuthoringSplit::derive(
        SUITE_GOLDEN_GRAPH,
        golden_cases.iter().map(|case| case.query_id.as_str()),
        DEFAULT_AUTHORING_FRACTION,
    );
    let (golden_pinned, golden_objects, golden_leakage_cases) =
        golden_leakage_inputs(&golden_cases, &golden_split)?;
    leakage_scans.push(leakage_outcome(
        SUITE_GOLDEN_GRAPH,
        &golden_pinned,
        &golden_objects,
        &golden_leakage_cases,
    )?);
    for null in [
        golden_graph::GoldenNull::PopularLabelPrior,
        golden_graph::GoldenNull::QueryPermutation,
    ] {
        let runs = golden_graph::null_seed_runs(
            null,
            &golden_cases,
            &golden_split,
            &NULL_SEEDS,
            golden_graph::GOLDEN_TOP_K,
        );
        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA)
            .with_context(|| format!("derive ceilings for {}", null.control_id()))?;
        let observed = golden_graph::recall_slices(
            &golden_cases,
            &golden_graph::control_rankings(
                null,
                &golden_cases,
                &golden_split,
                NULL_SEEDS[0],
                golden_graph::GOLDEN_TOP_K,
            ),
            golden_graph::GOLDEN_TOP_K,
        );
        controls.push(null_outcome(
            SUITE_GOLDEN_GRAPH,
            "expected_uid_recall_at_5",
            null.control_id(),
            &ceilings,
            &observed,
        )?);
    }
    controls.push(oracle_outcome(
        SUITE_GOLDEN_GRAPH,
        "expected_uid_recall_at_5",
        "oracle_expected_uids",
        &golden_graph::recall_slices(
            &golden_cases,
            &golden_graph::oracle_rankings(&golden_cases, golden_graph::GOLDEN_TOP_K),
            golden_graph::GOLDEN_TOP_K,
        ),
    )?);

    // Memory retrieval: generated corpus; the pure-scorer controls only.
    let memory_corpus =
        generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3], TranscriptStyle::Marked)
            .context("generate PR memory corpus")?;
    // This split belongs to leakage analysis only. The pure-scorer nulls below
    // remain all-probe, per-ProbeType diagnostics; they are not presented as a
    // held-out authoring/validation experiment because the generator currently
    // has only one independent query-template family for several probe types.
    let memory_leakage_split = AuthoringSplit::derive(
        SUITE_MEMORY_RETRIEVAL,
        memory_corpus
            .probes
            .iter()
            .map(|probe| probe.probe_type.as_str()),
        DEFAULT_AUTHORING_FRACTION,
    );
    let (memory_pinned, memory_objects, memory_leakage_cases) =
        memory_leakage_inputs(&memory_corpus, &memory_leakage_split);
    leakage_scans.push(leakage_outcome(
        SUITE_MEMORY_RETRIEVAL,
        &memory_pinned,
        &memory_objects,
        &memory_leakage_cases,
    )?);
    for null in [
        memory_retrieval::RetrievalNull::QueryIndependentRecentFacts,
        memory_retrieval::RetrievalNull::QueryPermutation,
    ] {
        let runs = memory_retrieval::null_seed_runs(
            null,
            &memory_corpus.probes,
            &memory_corpus.ledger,
            &NULL_SEEDS,
        );
        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA)
            .with_context(|| format!("derive ceilings for {}", null.control_id()))?;
        let observed = memory_retrieval::recall_at_4_by_probe_type(&match null {
            memory_retrieval::RetrievalNull::QueryIndependentRecentFacts => {
                memory_retrieval::recent_facts_probe_results(
                    &memory_corpus.probes,
                    &memory_corpus.ledger,
                    NULL_SEEDS[0],
                    RETRIEVAL_EVAL_FINAL_K,
                )
            }
            memory_retrieval::RetrievalNull::QueryPermutation => {
                memory_retrieval::query_permutation_probe_results(
                    &memory_corpus.probes,
                    NULL_SEEDS[0],
                )
            }
        });
        controls.push(null_outcome(
            SUITE_MEMORY_RETRIEVAL,
            "recall_at_4",
            null.control_id(),
            &ceilings,
            &observed,
        )?);
    }
    controls.push(oracle_outcome(
        SUITE_MEMORY_RETRIEVAL,
        "recall_at_4",
        "oracle_expected_facts",
        &memory_retrieval::recall_at_4_by_probe_type(&memory_retrieval::oracle_probe_results(
            &memory_corpus.probes,
        )),
    )?);
    let generator_defects =
        memory_retrieval::validate_generator_validity(&memory_corpus.probes, &memory_corpus.ledger);
    if !generator_defects.is_empty() {
        bail!(
            "generated memory corpus is not fair: {}",
            serde_json::to_string(&generator_defects)?
        );
    }

    // Long conversation: mock-domain lane.
    let case = long_conversation::release_task_case();
    let runs = long_conversation::null_seed_runs(&case, &NULL_SEEDS);
    let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA)
        .context("derive ceilings for fixed_plausible_response")?;
    let observed = long_conversation::blocking_pass_rate_by_category(
        &case,
        &long_conversation::fixed_plausible_response_envelope(NULL_SEEDS[0]),
    );
    controls.push(null_outcome(
        SUITE_LONG_CONVERSATION,
        "blocking_assertion_pass_rate",
        "fixed_plausible_response",
        &ceilings,
        &observed,
    )?);
    controls.push(oracle_outcome(
        SUITE_LONG_CONVERSATION,
        "blocking_assertion_pass_rate",
        "scripted_state_correct_trajectory",
        &long_conversation::mean_pass_rate_by_category(
            &case,
            &long_conversation::scripted_oracle_envelopes(),
        ),
    )?);

    // External memory: the checked-in common-format package exercises the same
    // exact-answer scorer as the provider-backed benchmark without calling a provider.
    let external_path =
        repo_path("crates/moa-eval/tests/fixtures/external_memory/common_cases.json");
    let external_cases = load_common_json(&external_path)
        .with_context(|| format!("load external-memory controls {}", external_path.display()))?;
    let external_split = AuthoringSplit::derive(
        SUITE_EXTERNAL_MEMORY,
        external_cases
            .iter()
            .map(|case| case.case.isolation_key.as_str()),
        DEFAULT_AUTHORING_FRACTION,
    );
    let (external_pinned, external_objects, external_leakage_cases) =
        external_memory_leakage_inputs(&external_path, &external_cases, &external_split)?;
    leakage_scans.push(leakage_outcome(
        SUITE_EXTERNAL_MEMORY,
        &external_pinned,
        &external_objects,
        &external_leakage_cases,
    )?);
    for null in [
        external_memory::ExternalMemoryNull::NoMemory,
        external_memory::ExternalMemoryNull::QueryIndependentAnswer,
    ] {
        let runs =
            external_memory::null_seed_runs(null, &external_cases, &external_split, &NULL_SEEDS);
        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA)
            .with_context(|| format!("derive ceilings for {}", null.control_id()))?;
        let observed = external_memory::accuracy_by_category(
            &external_cases,
            &external_memory::control_answers(
                null,
                &external_cases,
                &external_split,
                NULL_SEEDS[0],
            ),
        );
        controls.push(null_outcome(
            SUITE_EXTERNAL_MEMORY,
            "answer_accuracy",
            null.control_id(),
            &ceilings,
            &observed,
        )?);
    }
    controls.push(oracle_outcome(
        SUITE_EXTERNAL_MEMORY,
        "answer_accuracy",
        "oracle_evidence",
        &external_memory::accuracy_by_category(
            &external_cases,
            &external_memory::oracle_answers(&external_cases),
        ),
    )?);

    // WixQA controls only need labeled questions and the closed object space.
    // This deterministic workload exercises both gold-cardinality slices and
    // the scanner itself, but it is not the selected live benchmark corpus.
    // `wixqa-rag-eval` owns that required preflight scan.
    let (fixed_rag_questions, corpus_object_ids) = fixed_rag_control_workload();
    let fixed_rag_split = AuthoringSplit::derive(
        SUITE_WIXQA_RAG,
        fixed_rag_questions
            .iter()
            .map(|question| question.question_id.as_str()),
        DEFAULT_AUTHORING_FRACTION,
    );
    let (fixed_rag_pinned, fixed_rag_objects, fixed_rag_leakage_cases) =
        fixed_rag_leakage_inputs(&fixed_rag_questions, &corpus_object_ids, &fixed_rag_split);
    leakage_scans.push(scanner_fixture_outcome(
        FIXED_RAG_SCANNER_FIXTURE,
        &fixed_rag_pinned,
        &fixed_rag_objects,
        &fixed_rag_leakage_cases,
    )?);
    for null in [
        fixed_rag::FixedRagNull::PopularInCorpus,
        fixed_rag::FixedRagNull::RandomInCorpus,
        fixed_rag::FixedRagNull::QuestionPermutation,
    ] {
        let runs = fixed_rag::null_seed_runs(
            null,
            &fixed_rag_questions,
            &corpus_object_ids,
            &fixed_rag_split,
            &NULL_SEEDS,
            FIXED_RAG_TOP_K,
        );
        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA)
            .with_context(|| format!("derive ceilings for {}", null.control_id()))?;
        let observed = fixed_rag::recall_by_slice(
            &fixed_rag_questions,
            &fixed_rag::control_rankings(
                null,
                &fixed_rag_questions,
                &corpus_object_ids,
                &fixed_rag_split,
                NULL_SEEDS[0],
                FIXED_RAG_TOP_K,
            ),
            FIXED_RAG_TOP_K,
        );
        controls.push(null_outcome(
            SUITE_WIXQA_RAG,
            "recall_at_k",
            null.control_id(),
            &ceilings,
            &observed,
        )?);
    }
    controls.push(oracle_outcome(
        SUITE_WIXQA_RAG,
        "recall_at_k",
        "pinned_source_documents",
        &fixed_rag::recall_by_slice(
            &fixed_rag_questions,
            &fixed_rag::oracle_rankings(&fixed_rag_questions, FIXED_RAG_TOP_K),
            FIXED_RAG_TOP_K,
        ),
    )?);

    controls.extend(
        SUITE_CONTROLS
            .iter()
            .filter(|control| control.lane.requires_postgres())
            .map(skipped_database_outcome),
    );
    let execution_defects = validate_execution_coverage(&controls);
    let leakage_execution_defects = validate_leakage_coverage(&leakage_scans);

    let registry_defects = validate_registry()
        .into_iter()
        .map(serde_json::to_value)
        .collect::<serde_json::Result<Vec<_>>>()?;
    let lanes = LANE_CLASSIFICATIONS
        .iter()
        .map(serde_json::to_value)
        .collect::<serde_json::Result<Vec<_>>>()?;

    Ok(SuiteControlsReport {
        schema_version: 3,
        generated_at: chrono::Utc::now().to_rfc3339(),
        registry_defects,
        execution_defects,
        lanes,
        authoring_defects,
        controls,
        package_leakage_scope: PACKAGE_LEAKAGE_SCOPE,
        leakage_execution_defects,
        leakage_scans,
    })
}

fn null_outcome(
    suite: &str,
    metric: &str,
    control_id: &str,
    ceilings: &BTreeMap<String, NullCeiling>,
    observed: &BTreeMap<String, f64>,
) -> Result<ControlOutcome> {
    let registration = registered_control(suite, metric, control_id)?;
    let mut slices = BTreeMap::new();
    let mut violations = Vec::new();
    let expected_slices = ceilings.keys().cloned().collect::<BTreeSet<_>>();
    let observed_slices = observed.keys().cloned().collect::<BTreeSet<_>>();
    if observed_slices != expected_slices {
        violations.push(format!(
            "{suite}/{metric}/{control_id}: observed slices [{}] do not match derived ceiling slices [{}]",
            observed_slices.into_iter().collect::<Vec<_>>().join(","),
            expected_slices.into_iter().collect::<Vec<_>>().join(",")
        ));
    }
    for (slice, ceiling) in ceilings {
        let value = observed.get(slice).copied().unwrap_or(0.0);
        if value > ceiling.ceiling {
            violations.push(format!(
                "{suite}/{metric}/{control_id}: null scored {value} above its {} ceiling in slice {slice}",
                ceiling.ceiling
            ));
        }
        slices.insert(
            slice.clone(),
            ControlSlice {
                observed: value,
                ceiling: Some(ceiling.ceiling),
                floor: None,
                seed_mean: Some(ceiling.mean),
                seed_std_dev: Some(ceiling.std_dev),
                degenerate: Some(ceiling.is_degenerate()),
            },
        );
    }
    Ok(ControlOutcome {
        suite: suite.to_string(),
        metric: metric.to_string(),
        control_id: control_id.to_string(),
        role: registration.role,
        lane: registration.lane,
        status: ControlExecutionStatus::Executed,
        seeds: NULL_SEEDS.to_vec(),
        alpha: DEFAULT_CONTROL_ALPHA,
        slices,
        violations,
    })
}

fn oracle_outcome(
    suite: &str,
    metric: &str,
    control_id: &str,
    observed: &BTreeMap<String, f64>,
) -> Result<ControlOutcome> {
    let registration = registered_control(suite, metric, control_id)?;
    let mut slices = BTreeMap::new();
    let mut violations = Vec::new();
    for (slice, value) in observed {
        if *value < DEFAULT_ORACLE_FLOOR {
            violations.push(format!(
                "{suite}/{metric}/{control_id}: oracle scored {value} below the {DEFAULT_ORACLE_FLOOR} floor in slice {slice}"
            ));
        }
        slices.insert(
            slice.clone(),
            ControlSlice {
                observed: *value,
                ceiling: None,
                floor: Some(DEFAULT_ORACLE_FLOOR),
                seed_mean: None,
                seed_std_dev: None,
                degenerate: None,
            },
        );
    }
    Ok(ControlOutcome {
        suite: suite.to_string(),
        metric: metric.to_string(),
        control_id: control_id.to_string(),
        role: registration.role,
        lane: registration.lane,
        status: ControlExecutionStatus::Executed,
        seeds: Vec::new(),
        alpha: DEFAULT_CONTROL_ALPHA,
        slices,
        violations,
    })
}

fn registered_control(
    suite: &str,
    metric: &str,
    control_id: &str,
) -> Result<&'static SuiteControl> {
    SUITE_CONTROLS
        .iter()
        .find(|control| {
            control.suite == suite && control.metric == metric && control.control_id == control_id
        })
        .with_context(|| format!("control {suite}/{metric}/{control_id} is not registered"))
}

fn skipped_database_outcome(registration: &SuiteControl) -> ControlOutcome {
    ControlOutcome {
        suite: registration.suite.to_string(),
        metric: registration.metric.to_string(),
        control_id: registration.control_id.to_string(),
        role: registration.role,
        lane: registration.lane,
        status: ControlExecutionStatus::SkippedRequiresDatabase,
        seeds: Vec::new(),
        alpha: match registration.ceiling_source {
            CeilingSource::RepeatedNullSeeds { alpha, .. } => alpha,
            CeilingSource::OracleFloor { .. } => DEFAULT_CONTROL_ALPHA,
        },
        slices: BTreeMap::new(),
        violations: Vec::new(),
    }
}

fn fixed_rag_control_workload() -> (Vec<fixed_rag::FixedRagQuestion>, Vec<String>) {
    let corpus_object_ids = (0..200)
        .map(|index| format!("kb-{index:03}"))
        .collect::<Vec<_>>();
    let questions = (0..40)
        .map(|index| fixed_rag::FixedRagQuestion {
            question_id: format!("q-{index:03}"),
            question: format!("fixed control question {index}"),
            gold_object_ids: if index % 4 == 0 {
                vec![format!("kb-{index:03}"), format!("kb-{:03}", index + 1)]
            } else {
                vec![format!("kb-{index:03}")]
            },
        })
        .collect();
    (questions, corpus_object_ids)
}

#[derive(Deserialize)]
struct GoldenCorpusFixture {
    uid_seed: String,
    summary: String,
}

fn golden_leakage_inputs(
    cases: &[golden_graph::GoldenQueryCase],
    split: &AuthoringSplit,
) -> Result<(PinnedCorpus, Vec<CorpusObject>, Vec<EvalCaseText>)> {
    let fixture_dir = repo_path("crates/moa-eval/tests/fixtures/golden_100");
    let mut paths = std::fs::read_dir(&fixture_dir)
        .with_context(|| format!("read {}", fixture_dir.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    let captured_at = chrono::Utc::now();
    let objects = paths
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path)
                .with_context(|| format!("read golden corpus fixture {}", path.display()))?;
            let fixture: GoldenCorpusFixture = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse golden corpus fixture {}", path.display()))?;
            let text = format!("Fact: tenant shared {}", fixture.summary);
            Ok(CorpusObject {
                object_id: format!("fact-{}", fixture.uid_seed),
                declared_kind: ArtifactKind::SourceDocument,
                content_sha256: Some(sha256_text(&text)),
                provenance: Some(SourceProvenance {
                    source_uri: path.display().to_string(),
                    upstream_revision: sha256_text(std::str::from_utf8(&bytes).with_context(
                        || format!("golden corpus fixture {} is UTF-8", path.display()),
                    )?),
                    retrieved_at: captured_at,
                }),
                text,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let eval_cases = cases
        .iter()
        .map(|case| EvalCaseText {
            case_id: case.query_id.clone(),
            split: leakage_case_split(split, &case.query_id),
            question: case.query.clone(),
            answer: case.expected_uids.join(" "),
        })
        .collect::<Vec<_>>();
    Ok((
        pinned_from_objects("golden-100", &objects),
        objects,
        eval_cases,
    ))
}

fn memory_leakage_inputs(
    corpus: &moa_eval::memory_eval::GeneratedMemoryEvalCorpus,
    split: &AuthoringSplit,
) -> (PinnedCorpus, Vec<CorpusObject>, Vec<EvalCaseText>) {
    let captured_at = chrono::Utc::now();
    let objects = corpus
        .sessions
        .iter()
        .flat_map(|session| {
            session.turns.iter().map(|turn| {
                let object_id = format!("{}:{}", session.session_id, turn.turn_seq);
                CorpusObject {
                    object_id,
                    declared_kind: ArtifactKind::SourceDocument,
                    content_sha256: Some(sha256_text(&turn.transcript)),
                    provenance: Some(SourceProvenance {
                        source_uri: format!(
                            "generated://{}/{}/{}",
                            corpus.manifest.corpus_id, session.session_id, turn.turn_seq
                        ),
                        upstream_revision: corpus.manifest.corpus_id.clone(),
                        retrieved_at: captured_at,
                    }),
                    text: turn.transcript.clone(),
                }
            })
        })
        .collect::<Vec<_>>();
    let eval_cases = corpus
        .probes
        .iter()
        .map(|probe| EvalCaseText {
            case_id: probe.probe_id.clone(),
            // Generated probes repeat one semantic template across seeds,
            // tenants, and users. Keep each template family on one side of the
            // authoring boundary so those intentional cohorts cannot leak
            // across the split.
            split: leakage_case_split(split, probe.probe_type.as_str()),
            question: probe.query.clone(),
            answer: probe.answer.clone(),
        })
        .collect::<Vec<_>>();
    (
        pinned_from_objects(&corpus.manifest.corpus_id, &objects),
        objects,
        eval_cases,
    )
}

fn external_memory_leakage_inputs(
    path: &Path,
    cases: &[moa_eval::external_memory::dataset::PreparedExternalMemoryCase],
    split: &AuthoringSplit,
) -> Result<(PinnedCorpus, Vec<CorpusObject>, Vec<EvalCaseText>)> {
    let package_bytes = std::fs::read(path)
        .with_context(|| format!("read external-memory package {}", path.display()))?;
    let package_revision = format!(
        "sha256:{}",
        sha256_text(std::str::from_utf8(&package_bytes)?)
    );
    let captured_at = chrono::Utc::now();
    let objects = cases
        .iter()
        .flat_map(|prepared| {
            prepared.chronological_turns.iter().map(|turn| {
                let object_id = format!(
                    "{}:{}:{}",
                    prepared.case.isolation_key, turn.session_source_id, turn.turn_source_id
                );
                CorpusObject {
                    object_id,
                    declared_kind: ArtifactKind::SourceDocument,
                    content_sha256: Some(sha256_text(&turn.text)),
                    provenance: Some(SourceProvenance {
                        source_uri: format!("{}#{}", path.display(), turn.turn_source_id),
                        upstream_revision: package_revision.clone(),
                        retrieved_at: captured_at,
                    }),
                    text: turn.text.clone(),
                }
            })
        })
        .collect::<Vec<_>>();
    let eval_cases = cases
        .iter()
        .map(|prepared| EvalCaseText {
            case_id: prepared.case.isolation_key.clone(),
            split: leakage_case_split(split, &prepared.case.isolation_key),
            question: prepared.case.question.clone(),
            answer: prepared.case.answer.clone(),
        })
        .collect::<Vec<_>>();
    Ok((
        pinned_from_objects("external-memory-common-fixture-v1", &objects),
        objects,
        eval_cases,
    ))
}

fn fixed_rag_leakage_inputs(
    questions: &[fixed_rag::FixedRagQuestion],
    corpus_object_ids: &[String],
    split: &AuthoringSplit,
) -> (PinnedCorpus, Vec<CorpusObject>, Vec<EvalCaseText>) {
    let captured_at = chrono::Utc::now();
    let objects = corpus_object_ids
        .iter()
        .map(|object_id| {
            let text = format!("closed corpus source document {object_id}");
            CorpusObject {
                object_id: object_id.clone(),
                declared_kind: ArtifactKind::SourceDocument,
                content_sha256: Some(sha256_text(&text)),
                provenance: Some(SourceProvenance {
                    source_uri: format!("generated://fixed-rag/{object_id}"),
                    upstream_revision: "fixed-rag-control-v1".to_string(),
                    retrieved_at: captured_at,
                }),
                text,
            }
        })
        .collect::<Vec<_>>();
    let eval_cases = questions
        .iter()
        .map(|question| EvalCaseText {
            case_id: question.question_id.clone(),
            split: leakage_case_split(split, &question.question_id),
            question: question.question.clone(),
            answer: question.gold_object_ids.join(" "),
        })
        .collect::<Vec<_>>();
    (
        pinned_from_objects("fixed-rag-control-v1", &objects),
        objects,
        eval_cases,
    )
}

fn pinned_from_objects(corpus_id: &str, objects: &[CorpusObject]) -> PinnedCorpus {
    PinnedCorpus::new(
        corpus_id,
        objects
            .iter()
            .map(|object| (object.object_id.clone(), sha256_text(&object.text))),
    )
}

fn leakage_case_split(split: &AuthoringSplit, case_id: &str) -> CaseSplit {
    if split.is_authoring(case_id) {
        CaseSplit::Authoring
    } else {
        CaseSplit::GatedTest
    }
}

fn leakage_outcome(
    lane: &str,
    pinned: &PinnedCorpus,
    objects: &[CorpusObject],
    cases: &[EvalCaseText],
) -> Result<LeakageOutcome> {
    let classification = lane_classification(lane)
        .with_context(|| format!("eval lane `{lane}` has no contamination classification"))?;
    if !classification.requires_leakage_scan() {
        bail!("eval lane `{lane}` does not require a package-leakage scan");
    }
    scan_outcome(
        LeakageCoverage::RequiredLane {
            lane: lane.to_string(),
        },
        pinned,
        objects,
        cases,
    )
}

fn scanner_fixture_outcome(
    fixture_id: &str,
    pinned: &PinnedCorpus,
    objects: &[CorpusObject],
    cases: &[EvalCaseText],
) -> Result<LeakageOutcome> {
    if fixture_id.trim().is_empty() {
        bail!("scanner fixture id must not be blank");
    }
    scan_outcome(
        LeakageCoverage::ScannerFixture {
            fixture_id: fixture_id.to_string(),
        },
        pinned,
        objects,
        cases,
    )
}

fn scan_outcome(
    coverage: LeakageCoverage,
    pinned: &PinnedCorpus,
    objects: &[CorpusObject],
    cases: &[EvalCaseText],
) -> Result<LeakageOutcome> {
    match LeakageScanner::new().scan(pinned, objects, cases) {
        Ok(report) => Ok(LeakageOutcome {
            coverage,
            corpus_id: report.corpus_id,
            objects_scanned: report.objects_scanned,
            cases_scanned: report.cases_scanned,
            blocking_findings: Vec::new(),
            informational_findings: report.informational,
        }),
        Err(ContaminationError::LeakageDetected {
            corpus_id,
            findings,
            ..
        }) => Ok(LeakageOutcome {
            coverage,
            corpus_id,
            objects_scanned: objects.len(),
            cases_scanned: cases.len(),
            blocking_findings: findings,
            informational_findings: Vec::new(),
        }),
        Err(error) => Err(error.into()),
    }
}

fn validate_leakage_coverage(outcomes: &[LeakageOutcome]) -> Vec<String> {
    let expected = SUITE_OWNED_LEAKAGE_LANES
        .iter()
        .map(|lane| (*lane).to_string())
        .collect::<BTreeSet<_>>();
    let actual = outcomes
        .iter()
        .filter_map(|outcome| outcome.coverage.required_lane().map(str::to_string))
        .collect::<BTreeSet<_>>();
    let mut defects = Vec::new();
    for lane in &expected {
        if lane_classification(lane)
            .is_none_or(|classification| !classification.requires_leakage_scan())
        {
            defects.push(format!(
                "suite-owned leakage lane `{lane}` is not classified as requiring a scan"
            ));
        }
    }
    if actual != expected {
        defects.push(format!(
            "leakage scan outcome set mismatch: missing=[{}] unexpected=[{}]",
            expected
                .difference(&actual)
                .cloned()
                .collect::<Vec<_>>()
                .join(","),
            actual
                .difference(&expected)
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    let mut counts = BTreeMap::new();
    for outcome in outcomes {
        if let Some(lane) = outcome.coverage.required_lane() {
            *counts.entry(lane).or_insert(0_usize) += 1;
        }
        if outcome.objects_scanned == 0 || outcome.cases_scanned == 0 {
            defects.push(format!(
                "leakage scan for `{}` was vacuous: objects={} cases={}",
                outcome.coverage.label(),
                outcome.objects_scanned,
                outcome.cases_scanned
            ));
        }
    }
    for (lane, count) in counts.into_iter().filter(|(_, count)| *count != 1) {
        defects.push(format!(
            "leakage scan for `{lane}` has {count} outcomes; expected exactly one"
        ));
    }
    defects
}

type ControlKey = (String, String, String);
type MetricKey = (String, String);
type MetricSlices = Vec<(ControlKey, BTreeSet<String>)>;

fn control_key(suite: &str, metric: &str, control_id: &str) -> ControlKey {
    (
        suite.to_string(),
        metric.to_string(),
        control_id.to_string(),
    )
}

fn display_control_key(key: &ControlKey) -> String {
    format!("{}/{}/{}", key.0, key.1, key.2)
}

fn display_control_keys(keys: impl IntoIterator<Item = ControlKey>) -> String {
    keys.into_iter()
        .map(|key| display_control_key(&key))
        .collect::<Vec<_>>()
        .join(",")
}

fn validate_execution_coverage(outcomes: &[ControlOutcome]) -> Vec<String> {
    let registrations = SUITE_CONTROLS
        .iter()
        .map(|control| {
            (
                control_key(control.suite, control.metric, control.control_id),
                control,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let expected = registrations.keys().cloned().collect::<BTreeSet<_>>();
    let actual = outcomes
        .iter()
        .map(|outcome| control_key(&outcome.suite, &outcome.metric, &outcome.control_id))
        .collect::<BTreeSet<_>>();
    let mut defects = Vec::new();

    if actual != expected {
        defects.push(format!(
            "control outcome set mismatch: missing=[{}] unexpected=[{}]",
            display_control_keys(expected.difference(&actual).cloned()),
            display_control_keys(actual.difference(&expected).cloned())
        ));
    }

    let mut counts = BTreeMap::new();
    for outcome in outcomes {
        *counts
            .entry(control_key(
                &outcome.suite,
                &outcome.metric,
                &outcome.control_id,
            ))
            .or_insert(0_usize) += 1;
    }
    for (key, count) in counts.into_iter().filter(|(_, count)| *count != 1) {
        defects.push(format!(
            "control {} has {count} outcomes; expected exactly one",
            display_control_key(&key)
        ));
    }

    let mut slices_by_metric: BTreeMap<MetricKey, MetricSlices> = BTreeMap::new();
    for outcome in outcomes {
        let key = control_key(&outcome.suite, &outcome.metric, &outcome.control_id);
        let Some(registration) = registrations.get(&key) else {
            continue;
        };
        if outcome.role != registration.role || outcome.lane != registration.lane {
            defects.push(format!(
                "control {} metadata differs from the registry",
                display_control_key(&key)
            ));
        }
        let expected_status = if registration.lane.requires_postgres() {
            ControlExecutionStatus::SkippedRequiresDatabase
        } else {
            ControlExecutionStatus::Executed
        };
        if outcome.status != expected_status {
            defects.push(format!(
                "control {} has status {:?}; expected {:?}",
                display_control_key(&key),
                outcome.status,
                expected_status
            ));
        }
        match outcome.status {
            ControlExecutionStatus::Executed => {
                let slices = outcome.slices.keys().cloned().collect::<BTreeSet<_>>();
                if slices.is_empty() {
                    defects.push(format!(
                        "executed control {} produced no slices",
                        display_control_key(&key)
                    ));
                    continue;
                }
                slices_by_metric
                    .entry((outcome.suite.clone(), outcome.metric.clone()))
                    .or_default()
                    .push((key, slices));
            }
            ControlExecutionStatus::SkippedRequiresDatabase => {
                if !outcome.slices.is_empty() {
                    defects.push(format!(
                        "skipped control {} unexpectedly produced slices",
                        display_control_key(&key)
                    ));
                }
            }
        }
    }

    for controls in slices_by_metric.into_values() {
        let required = controls
            .iter()
            .flat_map(|(_, slices)| slices.iter().cloned())
            .collect::<BTreeSet<_>>();
        for (key, slices) in controls {
            if slices != required {
                defects.push(format!(
                    "control {} slices [{}] do not match required metric slices [{}]",
                    display_control_key(&key),
                    slices.into_iter().collect::<Vec<_>>().join(","),
                    required.iter().cloned().collect::<Vec<_>>().join(",")
                ));
            }
        }
    }

    defects
}

fn repo_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from(relative), |root| root.join(relative))
}

fn write_report(path: &Path, report: &SuiteControlsReport) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut json = serde_json::to_string_pretty(report)?;
    json.push('\n');
    std::fs::write(path, json).with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered_outcomes() -> Vec<ControlOutcome> {
        SUITE_CONTROLS
            .iter()
            .map(|registration| {
                if registration.lane.requires_postgres() {
                    return skipped_database_outcome(registration);
                }
                ControlOutcome {
                    suite: registration.suite.to_string(),
                    metric: registration.metric.to_string(),
                    control_id: registration.control_id.to_string(),
                    role: registration.role,
                    lane: registration.lane,
                    status: ControlExecutionStatus::Executed,
                    seeds: match registration.role {
                        ControlRole::NegativeNull => NULL_SEEDS.to_vec(),
                        ControlRole::PositiveOracle => Vec::new(),
                    },
                    alpha: DEFAULT_CONTROL_ALPHA,
                    slices: BTreeMap::from([(
                        "required_slice".to_string(),
                        ControlSlice {
                            observed: 1.0,
                            ceiling: None,
                            floor: None,
                            seed_mean: None,
                            seed_std_dev: None,
                            degenerate: None,
                        },
                    )]),
                    violations: Vec::new(),
                }
            })
            .collect()
    }

    #[tokio::test]
    async fn report_executes_every_registered_non_database_control() {
        // Pins: the command cannot silently omit a registered suite or represent
        // an offline control with an empty placeholder.
        let report = build_report()
            .await
            .expect("hermetic suite controls should build a report");
        let expected = SUITE_CONTROLS
            .iter()
            .map(|control| control_key(control.suite, control.metric, control.control_id))
            .collect::<BTreeSet<_>>();
        let actual = report
            .controls
            .iter()
            .map(|outcome| control_key(&outcome.suite, &outcome.metric, &outcome.control_id))
            .collect::<BTreeSet<_>>();

        assert_eq!(actual, expected);
        assert_eq!(report.controls.len(), SUITE_CONTROLS.len());
        assert!(report.execution_defects.is_empty());
        let expected_leakage_lanes = SUITE_OWNED_LEAKAGE_LANES
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let actual_leakage_lanes = report
            .leakage_scans
            .iter()
            .filter_map(|outcome| outcome.coverage.required_lane())
            .collect::<BTreeSet<_>>();
        assert_eq!(actual_leakage_lanes, expected_leakage_lanes);
        assert!(report.leakage_scans.iter().any(|outcome| matches!(
            &outcome.coverage,
            LeakageCoverage::ScannerFixture { fixture_id }
                if fixture_id == FIXED_RAG_SCANNER_FIXTURE
        )));
        assert!(report.leakage_execution_defects.is_empty());
        assert!(report.leakage_scans.iter().all(LeakageOutcome::is_valid));
        for outcome in &report.controls {
            if outcome.lane.requires_postgres() {
                assert_eq!(
                    outcome.status,
                    ControlExecutionStatus::SkippedRequiresDatabase
                );
                assert!(outcome.slices.is_empty());
            } else {
                assert_eq!(outcome.status, ControlExecutionStatus::Executed);
                assert!(
                    !outcome.slices.is_empty(),
                    "{} has no slice evidence",
                    display_control_key(&control_key(
                        &outcome.suite,
                        &outcome.metric,
                        &outcome.control_id
                    ))
                );
            }
        }
    }

    #[test]
    fn execution_coverage_rejects_a_missing_registered_control() {
        // Pins: registry completeness is enforced against actual outcomes, not
        // only against declarations in the registry itself.
        let mut outcomes = registered_outcomes();
        let removed = outcomes.remove(0);
        let removed_key = control_key(&removed.suite, &removed.metric, &removed.control_id);

        assert_eq!(
            validate_execution_coverage(&outcomes),
            vec![format!(
                "control outcome set mismatch: missing=[{}] unexpected=[]",
                display_control_key(&removed_key)
            )]
        );
    }

    #[test]
    fn leakage_coverage_rejects_a_missing_required_lane() {
        // Pins: a lane classification that requires package scanning cannot be
        // present only as metadata; it needs one executed, non-vacuous outcome.
        let outcomes = SUITE_OWNED_LEAKAGE_LANES
            .iter()
            .skip(1)
            .map(|lane| LeakageOutcome {
                coverage: LeakageCoverage::RequiredLane {
                    lane: (*lane).to_string(),
                },
                corpus_id: format!("{lane}-corpus"),
                objects_scanned: 1,
                cases_scanned: 1,
                blocking_findings: Vec::new(),
                informational_findings: Vec::new(),
            })
            .collect::<Vec<_>>();

        let defects = validate_leakage_coverage(&outcomes);
        assert_eq!(defects.len(), 1, "{defects:?}");
        assert!(defects[0].contains("missing=[memory_retrieval]"));
    }

    #[test]
    fn leakage_outcome_preserves_blocking_command_evidence() {
        // Pins: command reporting retains the typed scanner finding and the run
        // gate consumes it instead of reducing leakage to a detached boolean.
        let text = "How long is the key window? The key window is one day.";
        let object = CorpusObject {
            object_id: "leak".to_string(),
            declared_kind: ArtifactKind::SourceDocument,
            content_sha256: Some(sha256_text(text)),
            provenance: Some(SourceProvenance {
                source_uri: "fixture://leak".to_string(),
                upstream_revision: "fixture-v1".to_string(),
                retrieved_at: chrono::Utc::now(),
            }),
            text: text.to_string(),
        };
        let pinned = pinned_from_objects("leaky", std::slice::from_ref(&object));
        let cases = [EvalCaseText {
            case_id: "case".to_string(),
            split: CaseSplit::GatedTest,
            question: "How long is the key window?".to_string(),
            answer: "The key window is one day.".to_string(),
        }];

        let outcome = leakage_outcome(SUITE_MEMORY_RETRIEVAL, &pinned, &[object], &cases)
            .expect("blocking scan is represented as an outcome");
        assert!(!outcome.is_valid());
        assert!(matches!(
            outcome.blocking_findings.as_slice(),
            [LeakageFinding::QuestionAnswerPairLeak { object_id, case_id, .. }]
                if object_id == "leak" && case_id == "case"
        ));
    }

    #[test]
    fn execution_coverage_rejects_empty_required_slices() {
        // Pins: an `executed` label cannot turn an empty placeholder into control evidence.
        let mut outcomes = registered_outcomes();
        let outcome = outcomes
            .iter_mut()
            .find(|outcome| !outcome.lane.requires_postgres())
            .expect("the registry has an offline control");
        let key = control_key(&outcome.suite, &outcome.metric, &outcome.control_id);
        outcome.slices.clear();

        assert_eq!(
            validate_execution_coverage(&outcomes),
            vec![format!(
                "executed control {} produced no slices",
                display_control_key(&key)
            )]
        );
    }

    #[test]
    fn execution_coverage_rejects_skipping_a_non_database_control() {
        // Pins: only the database lane may use the explicit skip outcome.
        let mut outcomes = registered_outcomes();
        let outcome = outcomes
            .iter_mut()
            .find(|outcome| !outcome.lane.requires_postgres())
            .expect("the registry has an offline control");
        let key = control_key(&outcome.suite, &outcome.metric, &outcome.control_id);
        outcome.status = ControlExecutionStatus::SkippedRequiresDatabase;
        outcome.slices.clear();

        assert_eq!(
            validate_execution_coverage(&outcomes),
            vec![format!(
                "control {} has status SkippedRequiresDatabase; expected Executed",
                display_control_key(&key)
            )]
        );
    }
}
