//! Core evaluation types, loaders, and extension traits for MOA agent evals.

pub mod error;
pub mod evaluator;
pub mod loader;
pub mod reporter;
pub mod results;
pub mod types;

pub use error::{EvalError, Result};
pub use evaluator::Evaluator;
pub use loader::{discover_configs, discover_suites, load_agent_config, load_suite};
pub use reporter::Reporter;
pub use results::{EvalMetrics, EvalResult, EvalScore, EvalStatus, ScoreValue, TrajectoryStep};
pub use types::{
    AgentConfig, ExpectedOutput, InstructionOverride, MemoryOverride, PermissionOverride,
    SkillOverride, TestCase, TestSuite, ToolOverride,
};
