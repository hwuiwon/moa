//! Query and resolution signals used to rank available skills.

use std::collections::HashMap;

use moa_artifacts::registry::ArtifactRegistry;
use moa_core::{
    error::Result, events::Event, types::context::WorkingContext, types::events_stream::EventRange,
    types::experience::AttributionSubjectType, types::experience::TaskStrategySuccessRate,
    types::identifiers::StoragePartitionId, types::memory::SkillMetadata,
    types::segment_assessment::SkillResolutionRate,
};

use crate::learning::experience::task_fingerprint_for_context;
use crate::pipeline::memory::extract_search_keywords;

use super::tier1_metadata::{
    ResolvedSkillBudget, embedding_relevance_from_distance, manifest_may_truncate,
};
use super::{RECENT_EVENT_LIMIT, SkillInjector};

impl SkillInjector {
    pub(super) async fn query_keywords(&self, ctx: &WorkingContext) -> Result<Vec<String>> {
        if let Some(message) = ctx.last_user_message() {
            let keywords = extract_search_keywords(message);
            if !keywords.is_empty() {
                return Ok(keywords);
            }
        }

        if !ctx.recent_events().is_empty() {
            return Ok(extract_query_keywords_from_events(ctx.recent_events()));
        }

        let Some(session_store) = &self.session_store else {
            return Ok(Vec::new());
        };
        let events = session_store
            .get_events(ctx.session_id, EventRange::recent(RECENT_EVENT_LIMIT))
            .await?;
        Ok(extract_query_keywords_from_events(&events))
    }

    pub(super) async fn skill_resolution_rates(
        &self,
        ctx: &WorkingContext,
    ) -> Result<HashMap<String, f64>> {
        let Some(segment_store) = &self.segment_store else {
            return Ok(HashMap::new());
        };
        let tenant_id = ctx.tenant_id.to_string();
        let rates = segment_store
            .list_skill_resolution_rates(&tenant_id)
            .await?;
        Ok(skill_resolution_rate_map(&rates))
    }

    pub(super) async fn task_strategy_success_rates(
        &self,
        ctx: &WorkingContext,
    ) -> Result<HashMap<String, TaskStrategySuccessRate>> {
        let Some(segment_store) = &self.segment_store else {
            return Ok(HashMap::new());
        };
        let Some(fingerprint) = task_fingerprint_for_context(ctx) else {
            return Ok(HashMap::new());
        };
        let tenant_id = ctx.tenant_id.to_string();
        let rates = segment_store
            .list_task_strategy_success_rates(&tenant_id, &fingerprint.hash)
            .await?;
        Ok(task_strategy_success_rate_map(&rates))
    }

    /// Maps each candidate skill name to its query-relevance similarity in `[0, 1]`.
    ///
    /// The probe is the turn query embedded once and matched against the tenant's
    /// stored skill-identity embeddings in a single nearest-neighbor query, so at
    /// most one embed call and one lookup run per manifest build. It short-circuits
    /// to an empty map — the ranker's pure lexical fallback — whenever ranking
    /// cannot change the emitted manifest (the set fits without truncation) or the
    /// semantic signal is unavailable: no embedder, a non-registry source, an
    /// embedder whose width cannot probe the stored vectors, no query text, or a
    /// failed embed or lookup. A skill absent from the returned map (no embedding
    /// row yet) falls back to keyword overlap inside the ranker.
    pub(super) async fn skill_embedding_similarities(
        &self,
        ctx: &WorkingContext,
        candidates: &[SkillMetadata],
        budget: &ResolvedSkillBudget,
        max_visible: Option<usize>,
    ) -> HashMap<String, f64> {
        if !manifest_may_truncate(candidates, budget, max_visible) {
            return HashMap::new();
        }
        let Some(embedder) = self.embedder.as_deref() else {
            return HashMap::new();
        };
        let Some(pool) = self.registry_pool() else {
            return HashMap::new();
        };
        // A mismatched embedder cannot probe the stored halfvec space, so degrade
        // to lexical ranking rather than issuing a guaranteed-failing vector query.
        if embedder.dimensions() != moa_memory_vector::VECTOR_DIMENSION {
            tracing::warn!(
                configured = embedder.dimensions(),
                expected = moa_memory_vector::VECTOR_DIMENSION,
                "skill-manifest embedder dimension mismatch; ranking by lexical overlap"
            );
            return HashMap::new();
        }
        let Some(query) = manifest_query_text(ctx) else {
            return HashMap::new();
        };
        let probe = match embedder.embed(std::slice::from_ref(&query)).await {
            Ok(mut vectors) => match vectors.pop() {
                Some(probe) => probe,
                None => return HashMap::new(),
            },
            Err(error) => {
                tracing::warn!(
                    %error,
                    "skill-manifest query embedding failed; ranking by lexical overlap"
                );
                return HashMap::new();
            }
        };
        let registry = ArtifactRegistry::new(pool.clone());
        let partition = StoragePartitionId::for_tenant(ctx.tenant_id);
        let model_scope = Some((embedder.model_id(), embedder.model_version()));
        let neighbors = match registry
            .nearest_named_skill_embeddings_scoped(
                partition.as_str(),
                &probe,
                candidates.len(),
                model_scope,
            )
            .await
        {
            Ok(neighbors) => neighbors,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "skill-embedding nearest-neighbor lookup failed; ranking by lexical overlap"
                );
                return HashMap::new();
            }
        };
        neighbors
            .into_iter()
            .map(|neighbor| {
                (
                    neighbor.skill_name,
                    embedding_relevance_from_distance(neighbor.distance),
                )
            })
            .collect()
    }
}

/// The raw query text embedded as the skill-manifest relevance probe.
///
/// Mirrors [`SkillInjector::query_keywords`] source precedence — the active user
/// message, then the most recent in-context user or queued event — but returns
/// the text verbatim for embedding instead of its extracted keywords. Returns
/// `None` when no query text is in context, so the caller degrades to lexical
/// ranking.
fn manifest_query_text(ctx: &WorkingContext) -> Option<String> {
    if let Some(message) = ctx.last_user_message() {
        let trimmed = message.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    ctx.recent_events()
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
                let trimmed = text.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            }
            _ => None,
        })
}

fn skill_resolution_rate_map(rates: &[SkillResolutionRate]) -> HashMap<String, f64> {
    rates
        .iter()
        .map(|rate| {
            (
                rate.skill_name.clone(),
                rate.resolution_rate.clamp(0.0, 1.0),
            )
        })
        .collect()
}

fn task_strategy_success_rate_map(
    rates: &[TaskStrategySuccessRate],
) -> HashMap<String, TaskStrategySuccessRate> {
    rates
        .iter()
        .filter(|rate| rate.subject_type == AttributionSubjectType::Skill)
        .map(|rate| (rate.subject_id.clone(), rate.clone()))
        .collect()
}

fn extract_query_keywords_from_events(
    events: &[moa_core::types::events_stream::EventRecord],
) -> Vec<String> {
    events
        .iter()
        .rev()
        .find_map(|record| match &record.event {
            Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
                Some(extract_search_keywords(text))
            }
            _ => None,
        })
        .unwrap_or_default()
}
