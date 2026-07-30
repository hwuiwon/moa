//! Production-safe evaluation contracts and scoring helpers.

pub mod admission;
pub mod assertion;
pub mod conversation_cost;
pub mod decision;
pub mod engine;
pub mod error;
pub mod evaluator;
pub mod evaluators;
pub mod evidence;
pub mod loader;
pub mod metric;
pub mod plan;
pub mod reliability;
pub mod replay;
pub mod resource_report;
pub mod results;
pub mod types;

pub use admission::{
    AdmissionError, AdmittedRun, EVAL_ADMISSION_VERSION, EvalAdmissionLimits, EvalAdmissionPolicy,
};
pub use conversation_cost::{ConversationCost, TurnCost};
pub use engine::{EngineOptions, EvalRun, RunSummary};
pub use error::{Error, Result};
pub use evaluator::Evaluator;
pub use evaluators::{
    EvaluatorOptions, OutputMatchEvaluator, ThresholdEvaluator, ToolSuccessEvaluator,
    TrajectoryMatchEvaluator, build_evaluators, evaluate_run, score_is_failure,
};
pub use loader::{load_agent_config, load_suite};
pub use plan::{EvalPlan, build_eval_plan_with_estimator, estimate_run_cost_range};
pub use replay::{ReplayConfig, token_f1};
pub use resource_report::{RunResourceReport, usage_from_metrics};
pub use results::{EvalMetrics, EvalResult, EvalScore, EvalScoreValue, EvalStatus, TrajectoryStep};
pub use types::{
    ActionPolicyOverride, ActionPolicyRuleOverride, AgentConfig, ExpectedOutput,
    InstructionOverride, LongConversationMode, LongSessionInterleaving, LongTestCase,
    MemoryOverride, SecondaryLongSession, SuiteOracle, TestCase, TestCaseKind, TestSuite,
    ToolOverride,
};
