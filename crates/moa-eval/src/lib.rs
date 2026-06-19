//! Offline evaluation harnesses and internal-improvement runners for MOA.

pub mod collector;
pub mod engine;
pub mod golden;
pub mod kernel;
pub mod long_conversation;
pub mod memory_eval;
pub mod pentest;
pub mod reporter;
pub mod reporters;
pub mod setup;

pub use collector::TrajectoryCollector;
pub use engine::EvalEngine;
pub use moa_eval_core::{
    ActionPolicyOverride, AgentConfig, EngineOptions, EvalError, EvalMetrics, EvalPlan, EvalResult,
    EvalRun, EvalScore, EvalStatus, Evaluator, EvaluatorOptions, ExpectedOutput,
    InstructionOverride, LongConversationMode, LongSessionInterleaving, LongTestCase,
    MemoryOverride, OutputMatchEvaluator, ReplayConfig, Result, RunSummary, ScoreValue,
    SecondaryLongSession, SkillOverride, TestCase, TestCaseKind, TestSuite, ThresholdEvaluator,
    ToolOverride, ToolSuccessEvaluator, TrajectoryMatchEvaluator, TrajectoryStep, build_eval_plan,
    build_evaluators, discover_configs, discover_suites, evaluate_run, load_agent_config,
    load_suite, score_is_failure, token_f1,
};
pub use moa_eval_core::{error, evaluator, evaluators, loader, plan, replay, results, types};
pub use reporter::Reporter;
pub use reporters::JsonReporter;
#[cfg(feature = "langfuse")]
pub use reporters::LangfuseReporter;
pub use reporters::{ReporterOptions, TerminalReporter, build_reporters};
pub use setup::{AgentEnvironment, EvalLineageHandle, build_agent_environment};
