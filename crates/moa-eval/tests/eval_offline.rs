//! Consolidated offline integration tests for the eval crate.

#[path = "memory_eval_support/common.rs"]
mod common;

#[path = "eval_offline/evaluators.rs"]
mod evaluators;
#[path = "eval_offline/execution_calibration.rs"]
mod execution_calibration;
#[path = "eval_offline/execution_compare.rs"]
mod execution_compare;
#[path = "eval_offline/execution_contract.rs"]
mod execution_contract;
#[path = "eval_offline/execution_corpus.rs"]
mod execution_corpus;
#[path = "eval_offline/execution_invariants.rs"]
mod execution_invariants;
#[path = "eval_offline/execution_live.rs"]
mod execution_live;
#[path = "eval_offline/execution_report.rs"]
mod execution_report;
#[path = "eval_offline/execution_routing.rs"]
mod execution_routing;
#[path = "eval_offline/execution_snapshot.rs"]
mod execution_snapshot;
#[path = "eval_offline/external_memory.rs"]
mod external_memory;
#[path = "eval_offline/external_memory_calibration.rs"]
mod external_memory_calibration;
#[path = "eval_offline/external_memory_controls.rs"]
mod external_memory_controls;
#[path = "eval_offline/external_memory_execution.rs"]
mod external_memory_execution;
#[path = "eval_offline/external_memory_longmemeval.rs"]
mod external_memory_longmemeval;
#[path = "eval_offline/external_memory_personamem.rs"]
mod external_memory_personamem;
#[path = "eval_offline/loader.rs"]
mod loader;
#[path = "eval_offline/memory_eval_corpus.rs"]
mod memory_eval_corpus;
#[path = "eval_offline/memory_eval_judge.rs"]
mod memory_eval_judge;
#[path = "eval_offline/memory_eval_metrics.rs"]
mod memory_eval_metrics;
