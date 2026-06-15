//! Production-safe evaluation contracts and scoring helpers.

pub mod engine;
pub mod error;
pub mod evaluator;
pub mod evaluators;
pub mod loader;
pub mod plan;
pub mod replay;
pub mod results;
pub mod types;

pub use engine::{EngineOptions, EvalRun, RunSummary};
pub use error::{EvalError, Result};
pub use evaluator::Evaluator;
pub use evaluators::{
    EvaluatorOptions, OutputMatchEvaluator, ThresholdEvaluator, ToolSuccessEvaluator,
    TrajectoryMatchEvaluator, build_evaluators, evaluate_run, score_is_failure,
};
pub use loader::{discover_configs, discover_suites, load_agent_config, load_suite};
pub use plan::{EvalPlan, build_eval_plan};
pub use replay::{ReplayConfig, token_f1};
pub use results::{EvalMetrics, EvalResult, EvalScore, EvalStatus, ScoreValue, TrajectoryStep};
pub use types::{
    AgentConfig, ExpectedOutput, InstructionOverride, LongConversationMode,
    LongSessionInterleaving, LongTestCase, MemoryOverride, PermissionOverride,
    SecondaryLongSession, SkillOverride, TestCase, TestCaseKind, TestSuite, ToolOverride,
};
