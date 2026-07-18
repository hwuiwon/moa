//! Typed evaluation contracts for durable execution runs.

pub mod calibration;
pub mod compare;
pub mod contract;
pub mod corpus;
pub mod invariants;
pub mod live;
pub mod report;
pub mod routing;
pub mod snapshot;

pub use calibration::{
    EXECUTION_CALIBRATION_ITEM_COUNT, ExecutionCalibrationArtifact, ExecutionCalibrationItem,
    ExecutionCalibrationReport, score_execution_calibration,
};
pub use compare::{
    EXECUTION_EVAL_COMPARISON_SCHEMA_VERSION, EXECUTION_MUTATION_REPORT_SCHEMA_VERSION,
    ExecutionEvalComparison, ExecutionEvalComparisonConfig, ExecutionMutationReport,
    compare_execution_eval_reports, mutation_report_from_outcomes,
};
pub use contract::{
    CompletionCheckExpectation, ContractCategoryMetrics, CoverageExpectation,
    DeliverableExpectation, ExecutionContractCase, ExecutionContractExpectations,
    ExecutionContractScore, RunInputExpectation, TextExpectation, score_contract_case,
};
pub use corpus::{
    EXECUTION_CORPUS_MANIFEST_SCHEMA_VERSION, ExecutionCorpus, ExecutionCorpusFile,
    ExecutionCorpusManifest, load_execution_corpus,
};
pub use invariants::{ExecutionInvariantResult, ExecutionInvariantSpec, evaluate_invariants};
pub use live::{
    EXECUTION_LIVE_CASE_COUNT, EXECUTION_LIVE_REPETITIONS, ExecutionLiveCostForecast,
    ExecutionLiveRunOutcome, ExecutionTaskQualityCase, aggregate_live_execution_outcomes,
    forecast_live_execution_cost,
};
pub use report::{
    EXECUTION_EVAL_REPORT_SCHEMA_VERSION, ExecutionEvalAggregateMetrics, ExecutionEvalCaseResult,
    ExecutionEvalLane, ExecutionEvalProvider, ExecutionEvalReport, ExecutionJudgeCalibrationStatus,
};
pub use routing::{
    ExecutionRoutingCase, ExecutionRoutingCaseResult, ExecutionRoutingClassifierFixture,
    ExecutionRoutingLabel, ExecutionRoutingMetrics, score_routing_cases,
};
pub use snapshot::{
    EXECUTION_EVAL_SNAPSHOT_SCHEMA_VERSION, ExecutionCapabilityCallObservation, ExecutionEvalRun,
    ExecutionEvalSnapshot, ExecutionEvalTask, ExecutionHarnessEvidence,
    ExecutionPlanningAuditSummary, ExecutionProgressSummary, ExecutionSessionEventSummary,
    ExecutionTaskKindSummary, ExecutionTaskResultClass,
};

fn route_token_total(
    usage: moa_core::types::execution_planning::ExecutionRouteUsage,
) -> Option<u64> {
    usage
        .input_tokens_uncached
        .checked_add(usage.input_tokens_cache_write)
        .and_then(|value| value.checked_add(usage.input_tokens_cache_read))
        .and_then(|value| value.checked_add(usage.output_tokens))
}
