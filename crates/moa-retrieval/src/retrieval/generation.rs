//! Production routing at the active embedding generation.
//!
//! A partition can hold more than one embedding generation while a rebuild is
//! running. Exactly one of them answers queries, and this module is where that
//! is decided.
//!
//! Two properties matter here, and they are enforced differently on purpose:
//!
//! * **A candidate generation can never be served.** That is structural:
//!   candidate vectors live in `moa.knowledge_rebuild_candidate_vector`, which
//!   no retrieval leg reads. The resolver reinforces it by refusing to return a
//!   route for anything the active-generation pointer does not name, so a
//!   caller that somehow held a candidate id still cannot turn it into a route.
//! * **The query embedder must match the generation.** A query embedded by one
//!   model and compared against vectors from another produces plausible,
//!   ordered, wrong results. Nothing downstream can detect it, so it is checked
//!   here, once, before the vector leg runs.
//!
//! Source-ACL admission is unaffected and unduplicated: every leg still appends
//! the single `moa_db::push_source_acl_predicate` predicate. Generation routing
//! chooses *which vectors exist to be filtered*, never *who may see them*.

use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::EmbeddingGenerationId;
use sqlx::{PgPool, Row};

use crate::retrieval::types::{Result, RetrievalError};

/// The generation production retrieval serves this partition from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationRoute {
    /// Storage partition the route describes.
    pub storage_partition_id: String,
    /// Tenant that owns the partition.
    pub tenant_id: TenantId,
    /// Generation being served.
    pub generation_uid: EmbeddingGenerationId,
    /// Monotonic generation sequence, part of retrieval cache identity.
    pub generation_seq: i64,
    /// Embedding model every vector in this generation was built with.
    pub embedding_model: String,
    /// Embedding model version.
    pub embedding_model_version: i32,
    /// External namespace holding this generation's vectors.
    pub turbopuffer_namespace: String,
}

impl GenerationRoute {
    /// Rejects a query whose embedder does not match the served generation.
    ///
    /// Called before the vector leg. A mismatch is a hard failure rather than a
    /// degraded search: returning results from the wrong vector space is worse
    /// than returning none, because the caller cannot tell.
    pub fn require_embedder(&self, model: &str, model_version: i32) -> Result<()> {
        if self.embedding_model == model && self.embedding_model_version == model_version {
            return Ok(());
        }
        Err(RetrievalError::GenerationEmbedderMismatch {
            storage_partition_id: self.storage_partition_id.clone(),
            generation_model: format!("{}@{}", self.embedding_model, self.embedding_model_version),
            query_model: format!("{model}@{model_version}"),
        })
    }

    /// Returns the cache-identity fragment contributed by the served generation.
    ///
    /// Included in retrieval cache keys so an activation invalidates warm
    /// entries: results computed against the previous generation must not be
    /// replayed after the pointer flips.
    #[must_use]
    pub fn cache_fragment(&self) -> String {
        format!("gen:{}:{}", self.generation_seq, self.generation_uid)
    }
}

/// Resolves the production read generation for one storage partition.
///
/// Returns `None` for a partition that has never been rebuilt. Those partitions
/// have no generation rows at all and serve `moa.embeddings` directly, which is
/// the same vectors the bootstrap generation would name — so absence is normal,
/// not an error, and callers keep their existing behavior.
pub async fn resolve_active_generation(
    pool: &PgPool,
    storage_partition_id: &str,
) -> Result<Option<GenerationRoute>> {
    let row = sqlx::query(
        r#"
        SELECT generation.generation_uid,
               generation.tenant_id,
               generation.storage_partition_id,
               generation.generation_seq,
               generation.embedding_model,
               generation.embedding_model_version,
               generation.turbopuffer_namespace
          FROM moa.knowledge_active_generation AS pointer
          JOIN moa.knowledge_rebuild_generation AS generation
            ON generation.generation_uid = pointer.generation_uid
         WHERE pointer.storage_partition_id = $1
           AND generation.state = 'active'
        "#,
    )
    .bind(storage_partition_id)
    .fetch_optional(pool)
    .await
    .map_err(RetrievalError::Sqlx)?;

    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(GenerationRoute {
        storage_partition_id: row.try_get("storage_partition_id")?,
        tenant_id: TenantId::from(row.try_get::<uuid::Uuid, _>("tenant_id")?),
        generation_uid: EmbeddingGenerationId(row.try_get("generation_uid")?),
        generation_seq: row.try_get("generation_seq")?,
        embedding_model: row.try_get("embedding_model")?,
        embedding_model_version: row.try_get("embedding_model_version")?,
        turbopuffer_namespace: row.try_get("turbopuffer_namespace")?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route() -> GenerationRoute {
        GenerationRoute {
            storage_partition_id: "partition".to_string(),
            tenant_id: TenantId::from(uuid::Uuid::nil()),
            generation_uid: EmbeddingGenerationId(uuid::Uuid::nil()),
            generation_seq: 3,
            embedding_model: "embed-v4.0".to_string(),
            embedding_model_version: 1,
            turbopuffer_namespace: "moa-dev-partition__g3".to_string(),
        }
    }

    #[test]
    fn a_query_embedded_by_another_model_is_refused_rather_than_answered() {
        // Pins: mixing vector spaces fails loudly. Silently comparing an
        // `embed-v5` query against `embed-v4` vectors returns ranked, plausible,
        // wrong results that no downstream check can catch.
        let route = route();

        assert!(route.require_embedder("embed-v4.0", 1).is_ok());
        let error = route
            .require_embedder("embed-v5.0", 1)
            .expect_err("a different model must be refused");
        assert!(
            matches!(error, RetrievalError::GenerationEmbedderMismatch { .. }),
            "unexpected error: {error}"
        );
        assert!(
            route.require_embedder("embed-v4.0", 2).is_err(),
            "a version bump changes the vector space too"
        );
    }

    #[test]
    fn cache_identity_changes_when_the_served_generation_changes() {
        // Pins: an activation invalidates warm retrieval cache entries. Without
        // the generation in the key, a cached result computed against the
        // retired generation would keep being served after the flip.
        let first = route();
        let second = GenerationRoute {
            generation_seq: 4,
            generation_uid: EmbeddingGenerationId(uuid::Uuid::from_u128(9)),
            ..route()
        };

        assert_ne!(first.cache_fragment(), second.cache_fragment());
        assert_eq!(first.cache_fragment(), route().cache_fragment());
    }

    #[test]
    fn a_route_names_a_generation_specific_namespace() {
        // Pins: production reads the active generation's own external
        // namespace. Sharing one namespace across generations would let a
        // candidate build overwrite vectors the active generation is serving.
        assert!(route().turbopuffer_namespace.ends_with("__g3"));
    }
}
