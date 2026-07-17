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
    EXECUTION_CALIBRATION_ITEM_COUNT, ExecutionCalibrationArtifactV1, ExecutionCalibrationItemV1,
    ExecutionCalibrationReportV1, score_execution_calibration,
};
pub use compare::{
    EXECUTION_EVAL_COMPARISON_SCHEMA_VERSION, EXECUTION_MUTATION_REPORT_SCHEMA_VERSION,
    ExecutionEvalComparisonConfigV1, ExecutionEvalComparisonV1, ExecutionMutationReportV1,
    compare_execution_eval_reports, mutation_report_from_outcomes,
};
pub use contract::{
    CompletionCheckExpectationV1, ContractCategoryMetricsV1, CoverageExpectationV1,
    DeliverableExpectationV1, ExecutionContractCaseV1, ExecutionContractExpectationsV1,
    ExecutionContractScoreV1, RunInputExpectationV1, TextExpectationV1, score_contract_case,
};
pub use corpus::{
    EXECUTION_CORPUS_MANIFEST_SCHEMA_VERSION, ExecutionCorpusFileV1, ExecutionCorpusManifestV1,
    ExecutionCorpusV1, load_execution_corpus,
};
pub use invariants::{ExecutionInvariantResultV1, ExecutionInvariantSpecV1, evaluate_invariants};
pub use live::{
    EXECUTION_LIVE_CASE_COUNT, EXECUTION_LIVE_REPETITIONS, ExecutionLiveCostForecastV1,
    ExecutionLiveRunOutcomeV1, ExecutionTaskQualityCaseV1, aggregate_live_execution_outcomes,
    forecast_live_execution_cost,
};
pub use report::{
    EXECUTION_EVAL_REPORT_SCHEMA_VERSION, ExecutionEvalAggregateMetricsV1,
    ExecutionEvalCaseResultV1, ExecutionEvalLaneV1, ExecutionEvalProviderV1, ExecutionEvalReportV1,
    ExecutionJudgeCalibrationStatusV1,
};
pub use routing::{
    ExecutionRoutingCaseResultV1, ExecutionRoutingCaseV1, ExecutionRoutingClassifierFixtureV1,
    ExecutionRoutingLabelV1, ExecutionRoutingMetricsV1, score_routing_cases,
};
pub use snapshot::{
    EXECUTION_EVAL_SNAPSHOT_SCHEMA_VERSION, ExecutionCapabilityCallObservationV1,
    ExecutionEvalRunV1, ExecutionEvalSnapshotV1, ExecutionEvalTaskV1, ExecutionHarnessEvidenceV1,
    ExecutionPlanningAuditSummaryV1, ExecutionProgressSummaryV1, ExecutionSessionEventSummaryV1,
    ExecutionTaskKindSummaryV1, ExecutionTaskResultClassV1,
};
