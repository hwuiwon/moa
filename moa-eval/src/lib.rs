//! Core evaluation types, loaders, and extension traits for MOA agent evals.

pub mod collector;
pub mod engine;
pub mod error;
pub mod evaluator;
pub mod evaluators;
pub mod loader;
pub mod plan;
pub mod reporter;
pub mod reporters;
pub mod results;
pub mod setup;
pub mod types;

pub use collector::TrajectoryCollector;
pub use engine::{EngineOptions, EvalEngine, EvalRun, RunSummary};
pub use error::{EvalError, Result};
pub use evaluator::Evaluator;
pub use evaluators::{
    EvaluatorOptions, OutputMatchEvaluator, ThresholdEvaluator, ToolSuccessEvaluator,
    TrajectoryMatchEvaluator, build_evaluators, evaluate_run, score_is_failure,
};
pub use loader::{discover_configs, discover_suites, load_agent_config, load_suite};
pub use plan::EvalPlan;
pub use reporter::Reporter;
pub use reporters::JsonReporter;
#[cfg(feature = "langfuse")]
pub use reporters::LangfuseReporter;
pub use reporters::{ReporterOptions, TerminalReporter, build_reporters};
pub use results::{EvalMetrics, EvalResult, EvalScore, EvalStatus, ScoreValue, TrajectoryStep};
pub use setup::{AgentEnvironment, build_agent_environment};
pub use types::{
    AgentConfig, ExpectedOutput, InstructionOverride, MemoryOverride, PermissionOverride,
    SkillOverride, TestCase, TestSuite, ToolOverride,
};
