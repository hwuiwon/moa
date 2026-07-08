//! Maintenance passes for graph-memory lifecycle management.

use serde_json::Value;

pub mod consolidate;
pub mod curate;
pub mod digest;
pub mod quality;

/// Extracts a string field from an optional JSON properties object.
pub(crate) fn property_string(properties: &Option<Value>, key: &str) -> Option<String> {
    properties
        .as_ref()
        .and_then(|properties| properties.get(key))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

pub use consolidate::{
    BackfillStats, ConsolidationOptions, ConsolidationOutcome, DecayStats, EntityResolutionOptions,
    EntityResolutionStats, MergeStats, Result, SweepStats, TenantConsolidationCursor,
    advance_consolidation_watermark, backfill_entities, consolidate_tenant, decay_confidence,
    decay_target_confidence, merge_duplicates, rebuild_digests, resolve_entity_duplicates,
    sweep_contradictions, tenant_changelog_version, tenants_needing_consolidation,
};
pub use curate::{
    LessonCurationOptions, LessonCurationStats, curate_skill_lessons, normalize_lesson_summary,
};
pub use digest::{
    DIGEST_RENDER_VERSION, DigestFact, DigestScopeKind, DigestStats, RenderedDigest, render_digest,
};
pub use quality::{QualityStats, beta_smoothed_quality, compute_quality_scores};
