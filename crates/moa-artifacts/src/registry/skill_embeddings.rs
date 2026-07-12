//! Skill-identity embedding maintenance for the artifact registry.
//!
//! These back the background learning-embeddings backfill cron (R2 semantic
//! clustering + dedup). Each row in `moa.skill_embedding` holds the embedding of
//! one published Skill artifact's identity text (name + description + tags),
//! keyed by `artifact_uid`. Embeddings are computed out-of-band and never on the
//! publish path, so a freshly published skill has no embedding until the next
//! cron tick; every consumer must tolerate a missing row.
//!
//! The list/set methods run on the raw maintenance pool with no per-tenant RLS
//! context, sweeping every tenant, exactly like the regression monitor. The
//! nearest-neighbor primitive filters to one storage partition explicitly
//! because it is a per-tenant retrieval used at filing time, not maintenance.

use pgvector::HalfVector;

use super::*;

/// One published Skill artifact whose identity embedding is missing or stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSkillEmbedding {
    /// Artifact identity the embedding is keyed by.
    pub artifact_uid: Uuid,
    /// Currently-published revision whose identity is embedded (provenance).
    pub revision_uid: Uuid,
    /// Storage partition owning the artifact (NULL for global-scope skills).
    pub storage_partition_id: Option<String>,
    /// Owning contact/user, when the artifact is contact-scoped.
    pub user_id: Option<String>,
    /// Skill name.
    pub name: String,
    /// Skill description.
    pub description: String,
    /// Skill tags.
    pub tags: Vec<String>,
    /// Digest of the identity text last embedded, when a row already exists. The
    /// driver skips the provider call when this equals the current identity
    /// digest (a republish that did not change the identity text).
    pub stored_source_hash: Option<Vec<u8>>,
    /// Embedder that produced the stored vector, when a row already exists. The
    /// driver re-embeds (rather than touches) when this differs from the active
    /// embedder, since the old vector lives in an incompatible space.
    pub stored_model: Option<String>,
    /// Model version of the stored vector, paired with [`Self::stored_model`].
    pub stored_model_version: Option<i32>,
    /// The artifact's `updated_at` observed when this candidate was selected.
    /// Carried into the write as an optimistic token: the embedding is persisted
    /// only while the artifact still shows this timestamp, so an identity change
    /// that races the (slow) provider call cannot stamp a stale vector with a
    /// newer `updated_at` that would hide it from the next tick.
    pub artifact_updated_at: DateTime<Utc>,
}

/// A nearest skill-identity embedding within one storage partition.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillEmbeddingNeighbor {
    /// Neighbor skill artifact identity.
    pub artifact_uid: Uuid,
    /// Cosine distance from the probe in `[0, 2]` (0.0 = identical direction).
    pub distance: f64,
}

/// Values written for one skill-identity embedding.
#[derive(Debug, Clone)]
pub struct NewSkillEmbedding<'a> {
    /// Artifact identity the embedding is keyed by.
    pub artifact_uid: Uuid,
    /// Published revision current when the identity was embedded.
    pub revision_uid: Uuid,
    /// Storage partition owning the artifact (NULL for global-scope skills).
    pub storage_partition_id: Option<&'a str>,
    /// Owning contact/user, when the artifact is contact-scoped.
    pub user_id: Option<&'a str>,
    /// Embedding of the identity text.
    pub embedding: &'a [f32],
    /// Embedding model identifier.
    pub model: &'a str,
    /// Embedding model version (which vector space the bytes belong to).
    pub model_version: i32,
    /// Digest of the exact identity text embedded.
    pub source_hash: &'a [u8],
    /// The artifact's `updated_at` observed when the identity was selected for
    /// embedding. The write is applied only while the artifact still shows this
    /// timestamp, so an identity change concurrent with the provider call leaves
    /// the row for the next tick instead of persisting a mismatched vector.
    pub observed_artifact_updated_at: DateTime<Utc>,
}

/// Deletes one skill's identity embedding inside the caller's transaction.
///
/// Called on rollback so a stale identity vector never outlives the metadata it
/// was built from: on an improved-skill rollback the row is dropped and the
/// out-of-band backfill re-embeds from the restored identity; on a created-skill
/// rollback the artifact identity is retired, so the row is dropped for good.
/// Consumers already tolerate a missing row, so a briefly absent embedding is
/// strictly safer than serving the regressed one.
pub(crate) async fn delete_skill_embedding_in_tx(
    conn: &mut PgConnection,
    artifact_uid: Uuid,
) -> Result<()> {
    sqlx::query("DELETE FROM moa.skill_embedding WHERE artifact_uid = $1")
        .bind(artifact_uid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(())
}

impl ArtifactRegistry {
    /// Lists published Skill artifacts whose identity embedding is missing or
    /// stale, most recently changed first, capped at `limit`.
    ///
    /// A row is returned when it has no `moa.skill_embedding` row, when the
    /// artifact was updated (identity text changed, or republished) after its
    /// embedding was last written, or when its stored vector was produced by a
    /// different embedder than the active one (`active_model`/`active_model_version`).
    /// The last case matters after an embedder switch: the old vectors live in an
    /// incompatible space and must be re-embedded rather than compared against
    /// probes from the new one. Staleness uses `artifact.updated_at`, which
    /// `ensure_artifact` and `publish_revision` bump; the driver additionally
    /// compares `stored_source_hash` so an unchanged republish on the same
    /// embedder never re-embeds. Runs cross-tenant on the maintenance pool.
    pub async fn list_skills_missing_embedding(
        &self,
        active_model: &str,
        active_model_version: i32,
        limit: usize,
    ) -> Result<Vec<MissingSkillEmbedding>> {
        let rows = sqlx::query(
            r#"
            SELECT a.artifact_uid,
                   a.storage_partition_id,
                   a.user_id,
                   a.name,
                   a.description,
                   a.tags,
                   a.updated_at AS artifact_updated_at,
                   r.revision_uid,
                   se.source_hash AS stored_source_hash,
                   se.embedding_model AS stored_model,
                   se.embedding_model_version AS stored_model_version
            FROM moa.artifact a
            JOIN LATERAL (
                SELECT r2.revision_uid
                FROM moa.artifact_revision r2
                WHERE r2.artifact_uid = a.artifact_uid
                  AND r2.status = 'published'
                  AND r2.valid_to IS NULL
                ORDER BY r2.version DESC
                LIMIT 1
            ) r ON TRUE
            LEFT JOIN moa.skill_embedding se ON se.artifact_uid = a.artifact_uid
            WHERE a.kind = 'skill'
              AND a.valid_to IS NULL
              AND (se.artifact_uid IS NULL
                   OR se.updated_at < a.updated_at
                   OR se.embedding_model IS DISTINCT FROM $1
                   OR se.embedding_model_version IS DISTINCT FROM $2)
            ORDER BY a.updated_at DESC, a.artifact_uid ASC
            LIMIT $3
            "#,
        )
        .bind(active_model)
        .bind(active_model_version)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter()
            .map(|row| {
                Ok(MissingSkillEmbedding {
                    artifact_uid: row.try_get("artifact_uid").map_err(map_sqlx_error)?,
                    revision_uid: row.try_get("revision_uid").map_err(map_sqlx_error)?,
                    storage_partition_id: row
                        .try_get("storage_partition_id")
                        .map_err(map_sqlx_error)?,
                    user_id: row.try_get("user_id").map_err(map_sqlx_error)?,
                    name: row.try_get("name").map_err(map_sqlx_error)?,
                    description: row.try_get("description").map_err(map_sqlx_error)?,
                    tags: row.try_get("tags").map_err(map_sqlx_error)?,
                    stored_source_hash: row
                        .try_get("stored_source_hash")
                        .map_err(map_sqlx_error)?,
                    stored_model: row.try_get("stored_model").map_err(map_sqlx_error)?,
                    stored_model_version: row
                        .try_get("stored_model_version")
                        .map_err(map_sqlx_error)?,
                    artifact_updated_at: row
                        .try_get("artifact_updated_at")
                        .map_err(map_sqlx_error)?,
                })
            })
            .collect()
    }

    /// Inserts or refreshes one skill-identity embedding, keyed by artifact.
    ///
    /// The write is guarded by `observed_artifact_updated_at`: it applies only
    /// while the artifact still shows the `updated_at` seen when the identity was
    /// selected. `set_skill_embedding` follows a slow provider call, so the
    /// artifact's identity can change in between; without the guard the stale
    /// vector would land stamped with a fresh `updated_at`, hiding it from the
    /// staleness selection forever. On a lost guard the row is left for the next
    /// tick (still selected as NULL or stale). Returns whether the write applied.
    pub async fn set_skill_embedding(&self, input: NewSkillEmbedding<'_>) -> Result<bool> {
        let affected = sqlx::query(
            r#"
            INSERT INTO moa.skill_embedding
                (artifact_uid, storage_partition_id, user_id, revision_uid,
                 embedding, embedding_model, embedding_model_version, source_hash,
                 created_at, updated_at)
            SELECT $1, $2, $3, $4, $5, $6, $7, $8, now(), now()
            FROM moa.artifact a
            WHERE a.artifact_uid = $1 AND a.updated_at = $9
            ON CONFLICT (artifact_uid) DO UPDATE SET
                storage_partition_id = EXCLUDED.storage_partition_id,
                user_id = EXCLUDED.user_id,
                revision_uid = EXCLUDED.revision_uid,
                embedding = EXCLUDED.embedding,
                embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                source_hash = EXCLUDED.source_hash,
                updated_at = now()
            "#,
        )
        .bind(input.artifact_uid)
        .bind(input.storage_partition_id)
        .bind(input.user_id)
        .bind(input.revision_uid)
        .bind(HalfVector::from_f32_slice(input.embedding))
        .bind(input.model)
        .bind(input.model_version)
        .bind(input.source_hash)
        .bind(input.observed_artifact_updated_at)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        Ok(affected > 0)
    }

    /// Advances an existing embedding's `updated_at` without re-embedding.
    ///
    /// The driver calls this when a candidate surfaced as stale (its artifact was
    /// republished) but the identity digest is unchanged and the stored vector is
    /// already in the active space, so the row stops re-selecting on the next tick
    /// without a wasted provider call. Guarded by `observed_artifact_updated_at`
    /// like [`Self::set_skill_embedding`], so a concurrent identity change is not
    /// masked by a bumped timestamp. Returns whether a row was touched.
    pub async fn touch_skill_embedding(
        &self,
        artifact_uid: Uuid,
        observed_artifact_updated_at: DateTime<Utc>,
    ) -> Result<bool> {
        let affected = sqlx::query(
            "UPDATE moa.skill_embedding se SET updated_at = now() \
             FROM moa.artifact a \
             WHERE se.artifact_uid = $1 \
               AND a.artifact_uid = $1 \
               AND a.updated_at = $2",
        )
        .bind(artifact_uid)
        .bind(observed_artifact_updated_at)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        Ok(affected > 0)
    }

    /// Resolves a skill artifact identity to its current published name.
    ///
    /// The filing-time semantic router matches an experience against
    /// [`Self::nearest_skill_embeddings`], which returns an `artifact_uid`; this
    /// turns that identity back into the skill name the improver operates on.
    /// Returns `None` when the artifact is not a live skill (deleted or never a
    /// skill), so a stale embedding row never routes to a missing skill.
    pub async fn published_skill_name_for_artifact(
        &self,
        artifact_uid: Uuid,
    ) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT name FROM moa.artifact \
             WHERE artifact_uid = $1 AND kind = 'skill' AND valid_to IS NULL",
        )
        .bind(artifact_uid)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        row.map(|row| row.try_get("name").map_err(map_sqlx_error))
            .transpose()
    }

    /// Returns the nearest skill-identity embeddings to `probe` within one
    /// storage partition, ordered by ascending cosine distance.
    ///
    /// The R2 filing-time dedup uses this to route a draft to an existing skill
    /// instead of proposing a duplicate. `exclude_artifact_uid` drops a
    /// self-match when the probe is an existing skill.
    ///
    /// This does not constrain neighbors to a vector space; after an embedder
    /// switch the partition may still hold vectors from the previous space until
    /// the backfill converges them. Filing-time callers that embed the probe with
    /// a specific embedder should prefer [`Self::nearest_skill_embeddings_scoped`]
    /// with that embedder's `(model_id, model_version)`.
    pub async fn nearest_skill_embeddings(
        &self,
        storage_partition_id: &str,
        probe: &[f32],
        limit: usize,
        exclude_artifact_uid: Option<Uuid>,
    ) -> Result<Vec<SkillEmbeddingNeighbor>> {
        self.nearest_skill_embeddings_scoped(
            storage_partition_id,
            probe,
            limit,
            exclude_artifact_uid,
            None,
        )
        .await
    }

    /// Nearest skill-identity embeddings, optionally constrained to one vector space.
    ///
    /// When `model_scope` is `Some((model, version))`, only skills embedded by
    /// that exact embedder are considered, so a probe from one space is never
    /// ranked against vectors from another (the incompatible-space hazard after an
    /// embedder switch). `None` compares against every embedding in the partition
    /// and is what [`Self::nearest_skill_embeddings`] delegates to.
    pub async fn nearest_skill_embeddings_scoped(
        &self,
        storage_partition_id: &str,
        probe: &[f32],
        limit: usize,
        exclude_artifact_uid: Option<Uuid>,
        model_scope: Option<(&str, i32)>,
    ) -> Result<Vec<SkillEmbeddingNeighbor>> {
        let mut builder =
            sqlx::QueryBuilder::<sqlx::Postgres>::new("SELECT artifact_uid, (embedding <=> ");
        builder.push_bind(HalfVector::from_f32_slice(probe));
        // `ORDER BY distance` below resolves back to this distance expression,
        // so the HNSW cosine index remains eligible.
        builder.push(") AS distance FROM moa.skill_embedding WHERE storage_partition_id = ");
        builder.push_bind(storage_partition_id.to_string());
        if let Some((model, version)) = model_scope {
            builder.push(" AND embedding_model = ");
            builder.push_bind(model.to_string());
            builder.push(" AND embedding_model_version = ");
            builder.push_bind(version);
        }
        if let Some(exclude) = exclude_artifact_uid {
            builder.push(" AND artifact_uid <> ");
            builder.push_bind(exclude);
        }
        builder.push(" ORDER BY distance ASC, artifact_uid ASC LIMIT ");
        builder.push_bind(limit as i64);
        let rows = builder
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        rows.iter()
            .map(|row| {
                Ok(SkillEmbeddingNeighbor {
                    artifact_uid: row.try_get("artifact_uid").map_err(map_sqlx_error)?,
                    distance: row.try_get::<f64, _>("distance").map_err(map_sqlx_error)?,
                })
            })
            .collect()
    }
}
