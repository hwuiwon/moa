//! Background backfill of semantic embeddings for the skill-reinforcement loop.
//!
//! This drives the R2 (semantic clustering + dedup) infrastructure: it populates
//! task-summary embeddings on `experience_records` and identity embeddings on
//! published Skill artifacts. It is invoked from a cron handler, never from the
//! turn or persist path, so embeddings lag their source writes by up to one tick
//! (the eventual-consistency contract the storage columns document). Provider
//! unavailability is logged and leaves rows NULL/absent for the next tick to
//! retry; it never fails the job hard.
//!
//! The pure identity/hashing helpers here are the seam R2b will reuse when it
//! embeds a probe to route against these stored vectors.

use chrono::{DateTime, Duration, Utc};
use moa_artifacts::registry::{ArtifactRegistry, NewSkillEmbedding};
use moa_core::config::EmbeddingBackfillConfig;
use moa_core::error::{MoaError, Result};
use moa_core::traits::EmbeddingProvider;
use moa_session::PostgresSessionStore;
use sha2::{Digest, Sha256};

/// Fixed dimensionality of every learning embedding.
///
/// Must match the `halfvec(1024)` columns in the session baseline migration and
/// the graph-memory `VECTOR_DIMENSION`. The tenant embedder is reused across
/// memory and learning, so a deployment configured for graph memory already
/// produces 1024-dim vectors; the driver refuses to write when the configured
/// embedder disagrees, since the storage column cannot hold another width.
pub const EMBEDDING_DIM: usize = 1024;

/// Conservative upper bound on inputs sent to the embedding provider per call.
///
/// The per-tick config caps (`experience_batch_size`, `skill_batch_size`) can
/// exceed a single provider request's input limit, so a tick's batch is split
/// into calls of at most this size. Chosen below the smallest supported
/// provider request batch.
const MAX_INPUTS_PER_CALL: usize = 64;

/// Builds the canonical identity text embedded for a skill.
///
/// Tags are sorted so the identity is independent of tag ordering, making the
/// digest stable across republishes that only reorder tags.
#[must_use]
pub fn skill_identity_text(name: &str, description: &str, tags: &[String]) -> String {
    let mut sorted: Vec<&str> = tags.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    format!("{name}\n{description}\n{}", sorted.join(","))
}

/// Computes the digest of a skill's identity text.
///
/// Stored as `moa.skill_embedding.source_hash` so the backfill can skip
/// re-embedding a republished skill whose identity text did not change.
#[must_use]
pub fn skill_identity_hash(name: &str, description: &str, tags: &[String]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(skill_identity_text(name, description, tags).as_bytes());
    hasher.finalize().to_vec()
}

/// Whether a skill needs re-embedding given its stored digest and current one.
///
/// A skill with no stored digest always needs embedding; otherwise it is
/// re-embedded only when its identity text changed.
#[must_use]
pub fn should_reembed_skill(stored_source_hash: Option<&[u8]>, current_hash: &[u8]) -> bool {
    stored_source_hash != Some(current_hash)
}

/// Embeds missing task-summary embeddings for recent experience records.
///
/// Selects up to `experience_batch_size` records created within
/// `experience_lookback_days` that lack an embedding, embeds their task
/// summaries in provider-sized calls, and persists the vectors with their model
/// provenance. Returns the number of records embedded. A provider failure logs a
/// warning and returns the count embedded so far, leaving the rest NULL for the
/// next tick.
pub async fn backfill_experience_embeddings(
    store: &PostgresSessionStore,
    provider: &dyn EmbeddingProvider,
    config: &EmbeddingBackfillConfig,
    now: DateTime<Utc>,
) -> Result<usize> {
    if provider.dimensions() != EMBEDDING_DIM {
        tracing::warn!(
            configured = provider.dimensions(),
            expected = EMBEDDING_DIM,
            "learning embedder dimension mismatch; skipping experience embedding backfill"
        );
        return Ok(0);
    }
    let model = provider.model_id().to_string();
    let model_version = provider.model_version();
    let since = now - Duration::days(config.experience_lookback_days.max(0));
    let missing = store
        .list_experience_records_missing_task_embedding(
            since,
            &model,
            model_version,
            config.experience_batch_size,
        )
        .await?;
    if missing.is_empty() {
        return Ok(0);
    }

    let mut written = 0usize;
    for chunk in missing.chunks(MAX_INPUTS_PER_CALL) {
        let inputs: Vec<String> = chunk.iter().map(|row| row.task_summary.clone()).collect();
        let vectors = match provider.embed(&inputs).await {
            Ok(vectors) => vectors,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "experience embedding provider call failed; leaving rows NULL for retry"
                );
                break;
            }
        };
        if vectors.len() != chunk.len() {
            tracing::warn!(
                returned = vectors.len(),
                requested = chunk.len(),
                "experience embedding provider returned wrong vector count; skipping batch"
            );
            break;
        }
        // Carry the exact summary that was embedded so the write can refuse a row
        // whose summary changed under it (see `set_experience_task_embeddings`).
        let pairs: Vec<(uuid::Uuid, String, Vec<f32>)> = chunk
            .iter()
            .zip(vectors)
            .map(|(row, vector)| (row.id, row.task_summary.clone(), vector))
            .collect();
        store
            .set_experience_task_embeddings(&pairs, &model, model_version)
            .await?;
        written += pairs.len();
    }
    Ok(written)
}

/// Embeds missing or stale identity embeddings for published Skill artifacts.
///
/// Selects up to `skill_batch_size` published skills whose embedding is missing,
/// whose artifact changed since it was embedded, or whose stored vector belongs
/// to a different embedder than the active one. A candidate whose stored digest
/// still matches its current identity AND is already in the active vector space
/// (an unchanged republish on the same embedder) only has its `updated_at`
/// advanced, avoiding a provider call; the rest are embedded in provider-sized
/// calls and upserted. Each write is guarded on the artifact's observed
/// `updated_at`, so an identity change racing the provider call leaves the row
/// for the next tick instead of persisting a stale vector. Returns the number of
/// skills embedded. A provider failure logs a warning and returns the count
/// embedded so far.
pub async fn backfill_skill_embeddings(
    registry: &ArtifactRegistry,
    provider: &dyn EmbeddingProvider,
    config: &EmbeddingBackfillConfig,
) -> Result<usize> {
    if provider.dimensions() != EMBEDDING_DIM {
        tracing::warn!(
            configured = provider.dimensions(),
            expected = EMBEDDING_DIM,
            "learning embedder dimension mismatch; skipping skill embedding backfill"
        );
        return Ok(0);
    }
    let model = provider.model_id().to_string();
    let model_version = provider.model_version();
    let missing = registry
        .list_skills_missing_embedding(&model, model_version, config.skill_batch_size)
        .await?;
    if missing.is_empty() {
        return Ok(0);
    }

    let mut to_embed = Vec::new();
    for candidate in &missing {
        let current_hash =
            skill_identity_hash(&candidate.name, &candidate.description, &candidate.tags);
        // Re-embed when the identity text changed, or when the stored vector was
        // produced by a different embedder than the active one (an incompatible
        // space after a model switch). Only an unchanged identity already in the
        // active space is safe to touch without a provider call.
        let in_active_space = candidate.stored_model.as_deref() == Some(model.as_str())
            && candidate.stored_model_version == Some(model_version);
        if should_reembed_skill(candidate.stored_source_hash.as_deref(), &current_hash)
            || !in_active_space
        {
            to_embed.push((candidate, current_hash));
        } else {
            // Identity unchanged since the last embed (a republish that did not
            // touch name/description/tags): advance updated_at so it stops
            // re-selecting, without spending a provider call. Guarded on the
            // observed timestamp so a concurrent identity change is not masked.
            registry
                .touch_skill_embedding(candidate.artifact_uid, candidate.artifact_updated_at)
                .await?;
        }
    }
    if to_embed.is_empty() {
        return Ok(0);
    }

    let mut written = 0usize;
    for chunk in to_embed.chunks(MAX_INPUTS_PER_CALL) {
        let inputs: Vec<String> = chunk
            .iter()
            .map(|(candidate, _)| {
                skill_identity_text(&candidate.name, &candidate.description, &candidate.tags)
            })
            .collect();
        let vectors = match provider.embed(&inputs).await {
            Ok(vectors) => vectors,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "skill embedding provider call failed; leaving skills unembedded for retry"
                );
                break;
            }
        };
        if vectors.len() != chunk.len() {
            return Err(MoaError::ProviderError(format!(
                "skill embedding provider returned {} vectors for {} inputs",
                vectors.len(),
                chunk.len()
            )));
        }
        for ((candidate, source_hash), vector) in chunk.iter().zip(vectors) {
            let applied = registry
                .set_skill_embedding(NewSkillEmbedding {
                    artifact_uid: candidate.artifact_uid,
                    revision_uid: candidate.revision_uid,
                    storage_partition_id: candidate.storage_partition_id.as_deref(),
                    user_id: candidate.user_id.as_deref(),
                    embedding: &vector,
                    model: &model,
                    model_version,
                    source_hash,
                    observed_artifact_updated_at: candidate.artifact_updated_at,
                })
                .await?;
            // A lost optimistic guard (the artifact changed during the provider
            // call) leaves the row for the next tick rather than persisting a
            // vector for an identity the artifact no longer has.
            if applied {
                written += 1;
            }
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_identity_text_is_tag_order_independent() {
        // Pins: identity text sorts tags, so reordered tags hash identically and
        // a republish that only reorders tags does not trigger a re-embed.
        let a = skill_identity_text("deploy", "ship it", &["ops".into(), "cd".into()]);
        let b = skill_identity_text("deploy", "ship it", &["cd".into(), "ops".into()]);
        assert_eq!(a, b);
        assert_eq!(
            skill_identity_hash("deploy", "ship it", &["ops".into(), "cd".into()]),
            skill_identity_hash("deploy", "ship it", &["cd".into(), "ops".into()]),
        );
    }

    #[test]
    fn identity_hash_changes_when_text_changes() {
        // Pins: a description edit changes the digest, so the backfill re-embeds
        // rather than skipping on the updated_at staleness signal alone.
        let base = skill_identity_hash("deploy", "ship it", &["cd".into()]);
        let renamed = skill_identity_hash("release", "ship it", &["cd".into()]);
        let described = skill_identity_hash("deploy", "ship it fast", &["cd".into()]);
        assert_ne!(base, renamed);
        assert_ne!(base, described);
    }

    #[test]
    fn should_reembed_only_on_digest_change() {
        // Pins: the re-embed decision skips a matching stored digest and embeds
        // when the digest is absent or differs.
        let hash = skill_identity_hash("deploy", "ship it", &["cd".into()]);
        assert!(should_reembed_skill(None, &hash));
        assert!(!should_reembed_skill(Some(&hash), &hash));
        let other = skill_identity_hash("deploy", "ship it fast", &["cd".into()]);
        assert!(should_reembed_skill(Some(&other), &hash));
    }
}
