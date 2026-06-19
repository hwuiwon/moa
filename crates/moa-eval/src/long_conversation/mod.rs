//! Long-conversation evaluation harness primitives.

pub mod budgets;
pub mod cache_metrics;
pub mod memory_metrics;
pub mod provider_recorded;
pub mod score_card;
pub mod scripted_user;
pub mod transcript_runner;

pub use budgets::{BudgetResult, BudgetViolation, Budgets};
pub use cache_metrics::{
    CompiledRequest, TurnUsage, compute_input_cached_ratio, compute_prefix_stability,
    compute_stable_prefix_bytes,
};
pub use memory_metrics::{
    ConsolidationOutcomes, MemoryScenario, compute_planted_fact_recall,
    count_consolidation_outcomes, count_pages_written,
};
pub use provider_recorded::{RecordedProviderError, RecordedScriptedProvider};
pub use score_card::{
    CacheScores, ContextScores, CostScores, FunctionalScores, LatencyScores, MemoryScores,
    MetricRow, SafetyScores, ScoreCard, ToolScores,
};
pub use scripted_user::{
    ScriptedUserError, ScriptedUserScript, ScriptedUserTurn, ScriptedUserUtterance,
};
pub use transcript_runner::{LongRunReport, run_scenario_with_provider};
