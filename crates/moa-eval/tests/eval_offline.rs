//! Consolidated offline integration tests for the eval crate.

#[path = "memory_eval_support/common.rs"]
mod common;

#[path = "eval_offline/evaluators.rs"]
mod evaluators;
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
