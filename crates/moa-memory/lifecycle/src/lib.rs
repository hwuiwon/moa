//! Maintenance passes for graph-memory lifecycle management.

pub mod consolidate;

pub use consolidate::{
    BackfillStats, ConsolidationOptions, ConsolidationOutcome, DecayStats, MergeStats, Result,
    SweepStats, backfill_entities, consolidate_workspace, decay_confidence, merge_duplicates,
    sweep_contradictions,
};
