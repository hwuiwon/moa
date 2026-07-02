//! Consolidated offline integration harness for `moa-memory-pii`.

#[path = "memory_pii_offline/classify_smoke.rs"]
mod classify_smoke;
#[path = "memory_pii_offline/heuristic.rs"]
mod heuristic;
#[path = "memory_pii_offline/openai_filter_offline.rs"]
mod openai_filter_offline;
