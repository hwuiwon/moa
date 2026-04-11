//! Core evaluation types, loaders, and extension traits for MOA agent evals.

pub mod collector;
pub mod engine;
pub mod error;
pub mod evaluator;
pub mod loader;
pub mod plan;
pub mod reporter;
pub mod results;
pub mod setup;
pub mod types;

pub use collector::TrajectoryCollector;
pub use engine::{EngineOptions, EvalEngine, EvalRun, RunSummary};
pub use error::{EvalError, Result};
pub use evaluator::Evaluator;
pub use loader::{discover_configs, discover_suites, load_agent_config, load_suite};
pub use plan::EvalPlan;
pub use reporter::Reporter;
pub use results::{EvalMetrics, EvalResult, EvalScore, EvalStatus, ScoreValue, TrajectoryStep};
pub use setup::{AgentEnvironment, build_agent_environment};
pub use types::{
    AgentConfig, ExpectedOutput, InstructionOverride, MemoryOverride, PermissionOverride,
    SkillOverride, TestCase, TestSuite, ToolOverride,
};
