//! Maintenance passes for graph-memory lifecycle management.

pub mod consolidate;
pub mod digest;
pub mod quality;

pub use consolidate::{
    BackfillStats, ConsolidationOptions, ConsolidationOutcome, DecayStats, MergeStats, Result,
    SweepStats, backfill_entities, consolidate_tenant, decay_confidence, merge_duplicates,
    rebuild_digests, sweep_contradictions,
};
pub use digest::{
    DIGEST_RENDER_VERSION, DigestFact, DigestScopeKind, DigestStats, RenderedDigest, render_digest,
};
pub use quality::{QualityStats, beta_smoothed_quality, compute_quality_scores};
