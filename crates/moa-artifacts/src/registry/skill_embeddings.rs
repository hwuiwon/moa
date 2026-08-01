//! Skill-identity embedding maintenance for the artifact registry.
//!
//! These back the background learning-embeddings backfill cron (R2 semantic
//! clustering + dedup). Each row in `moa.skill_embedding` holds the embedding of
//! one serving Skill artifact's identity text (name + description + tags),
//! keyed by `artifact_uid`. Embeddings are computed out-of-band and never on the
//! activation path, so a freshly activated skill has no embedding until the next
//! cron tick; every consumer must tolerate a missing row.
//!
//! The list/set methods run on the raw maintenance pool with no per-tenant RLS
//! context, sweeping every tenant, exactly like the regression monitor. The
//! nearest-neighbor primitive filters to one storage partition explicitly
//! because it is a per-tenant retrieval used at filing time, not maintenance.

use pgvector::HalfVector;

use super::*;

/// One serving Skill artifact whose identity embedding is missing or stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSkillEmbedding {
    /// Artifact identity the embedding is keyed by.
    pub artifact_uid: Uuid,
    /// Currently serving revision whose identity is embedded (provenance).
    pub revision_uid: Uuid,
    /// Tenant storage partition owning the serving skill.
    pub storage_partition_id: String,
    /// Skill name.
    pub name: String,
    /// Skill description.
    pub description: String,
    /// Skill tags.
    pub tags: Vec<String>,
    /// Digest of the identity text last embedded, when a row already exists. The
    /// driver skips the provider call when this equals the current identity
    /// digest (a reactivation that did not change the identity text).
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

/// A nearest skill-identity embedding resolved to its current live name.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedSkillEmbeddingNeighbor {
    /// Current name of the nearest live skill.
    pub skill_name: String,
    /// Cosine distance from the probe in `[0, 2]` (0.0 = identical direction).
    pub distance: f64,
}

/// Values written for one skill-identity embedding.
#[derive(Debug, Clone)]
pub struct NewSkillEmbedding<'a> {
    /// Artifact identity the embedding is keyed by.
    pub artifact_uid: Uuid,
    /// Serving revision current when the identity was embedded.
    pub revision_uid: Uuid,
    /// Tenant storage partition owning the serving skill.
    pub storage_partition_id: &'a str,
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
/// out-of-band backfill remains empty until a separately reviewed replacement
/// activates; on a created-skill rollback the artifact identity is retired, so
/// the row is dropped for good. Consumers already tolerate a missing row, so an
/// absent embedding is strictly safer than advertising the regressed one.
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
    /// Lists serving Skill artifacts whose identity embedding is missing or
    /// stale, most recently changed first, capped at `limit`.
    ///
    /// A row is returned when it has no `moa.skill_embedding` row, when the
    /// artifact was updated (identity text changed, or reactivated) after its
    /// embedding was last written, or when its stored vector was produced by a
    /// different embedder than the active one (`active_model`/`active_model_version`).
    /// Only skills with a serving pointer are considered.
    /// The last case matters after an embedder switch: the old vectors live in an
    /// incompatible space and must be re-embedded rather than compared against
    /// probes from the new one. Staleness uses `artifact.updated_at`, which
    /// `ensure_artifact` and every activation bump; the driver additionally
    /// compares `stored_source_hash` so an unchanged reactivation on the same
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
                   a.name,
                   a.description,
                   a.tags,
                   a.updated_at AS artifact_updated_at,
                   r.revision_uid,
                   se.source_hash AS stored_source_hash,
                   se.embedding_model AS stored_model,
                   se.embedding_model_version AS stored_model_version
            FROM moa.artifact a
            -- Serving is the type-owned pointer, so only the revision a tenant
            -- actually serves gets an identity embedding. A draft or an
            -- unevaluated candidate must not be advertised by ranking.
            JOIN moa.artifact_serving_pointer p ON p.artifact_uid = a.artifact_uid
            JOIN moa.artifact_revision r ON r.revision_uid = p.revision_uid
                AND r.valid_to IS NULL
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
    /// The write is guarded by the exact serving revision and
    /// `observed_artifact_updated_at`. `set_skill_embedding` follows a slow
    /// provider call, so the pointer or identity can change in between; without
    /// both guards a stale vector could land stamped with a fresh `updated_at`,
    /// hiding it from the staleness selection forever. On a lost guard the row is
    /// left for the next tick (still selected as NULL or stale). Returns whether
    /// the write applied.
    pub async fn set_skill_embedding(&self, input: NewSkillEmbedding<'_>) -> Result<bool> {
        let affected = sqlx::query(
            r#"
            WITH locked_artifact AS MATERIALIZED (
                SELECT a.artifact_uid
                FROM moa.artifact a
                WHERE a.artifact_uid = $1
                  AND a.storage_partition_id = $2
                  AND a.updated_at = $8
                FOR UPDATE OF a
            ), current AS MATERIALIZED (
                SELECT locked_artifact.artifact_uid
                FROM locked_artifact
                JOIN moa.artifact_serving_pointer p
                  ON p.artifact_uid = locked_artifact.artifact_uid
                WHERE p.revision_uid = $3
                FOR UPDATE OF p
            )
            INSERT INTO moa.skill_embedding
                (artifact_uid, storage_partition_id, user_id, revision_uid,
                 embedding, embedding_model, embedding_model_version, source_hash,
                 created_at, updated_at)
            SELECT $1, $2, NULL, $3, $4, $5, $6, $7, now(), now()
            FROM current
            ON CONFLICT (artifact_uid) DO UPDATE SET
                storage_partition_id = EXCLUDED.storage_partition_id,
                user_id = NULL,
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
    /// reactivated) but the identity digest is unchanged and the stored vector is
    /// already in the active space, so the row stops re-selecting on the next tick
    /// without a wasted provider call. Guarded by `observed_artifact_updated_at`
    /// like [`Self::set_skill_embedding`], so a concurrent pointer or identity
    /// change is not masked by a bumped timestamp. Returns whether a row was
    /// touched.
    pub async fn touch_skill_embedding(
        &self,
        artifact_uid: Uuid,
        revision_uid: Uuid,
        observed_artifact_updated_at: DateTime<Utc>,
    ) -> Result<bool> {
        let affected = sqlx::query(
            "WITH locked_artifact AS MATERIALIZED ( \
                 SELECT a.artifact_uid \
                 FROM moa.artifact a \
                 WHERE a.artifact_uid = $1 \
                   AND a.updated_at = $3 \
                 FOR UPDATE OF a \
             ), current AS MATERIALIZED ( \
                 SELECT locked_artifact.artifact_uid \
                 FROM locked_artifact \
                 JOIN moa.artifact_serving_pointer p \
                   ON p.artifact_uid = locked_artifact.artifact_uid \
                 WHERE p.revision_uid = $2 \
                 FOR UPDATE OF p \
             ) \
             UPDATE moa.skill_embedding se SET updated_at = now() \
             FROM current \
             WHERE se.artifact_uid = current.artifact_uid \
               AND se.revision_uid = $2",
        )
        .bind(artifact_uid)
        .bind(revision_uid)
        .bind(observed_artifact_updated_at)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        Ok(affected > 0)
    }

    /// Resolves a skill artifact identity to its current live name.
    ///
    /// The filing-time semantic router matches an experience against
    /// [`Self::nearest_skill_embeddings`], which returns an `artifact_uid`; this
    /// turns that identity back into the skill name the improver operates on.
    /// Returns `None` when the artifact is not a live skill (deleted or never a
    /// skill), so a stale embedding row never routes to a missing skill.
    pub async fn live_skill_name_for_artifact(&self, artifact_uid: Uuid) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT a.name FROM moa.artifact a \
             JOIN moa.artifact_serving_pointer p ON p.artifact_uid = a.artifact_uid \
             WHERE a.artifact_uid = $1 \
               AND a.kind = 'skill' \
               AND a.valid_to IS NULL",
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
            sqlx::QueryBuilder::<sqlx::Postgres>::new("SELECT se.artifact_uid, (se.embedding <=> ");
        builder.push_bind(HalfVector::from_f32_slice(probe));
        // `ORDER BY distance` below resolves back to this distance expression,
        // so the HNSW cosine index remains eligible.
        builder.push(
            ") AS distance FROM moa.skill_embedding se \
             JOIN moa.artifact_serving_pointer p \
               ON p.artifact_uid = se.artifact_uid \
              AND p.revision_uid = se.revision_uid \
             WHERE se.storage_partition_id = ",
        );
        builder.push_bind(storage_partition_id.to_string());
        if let Some((model, version)) = model_scope {
            builder.push(" AND se.embedding_model = ");
            builder.push_bind(model.to_string());
            builder.push(" AND se.embedding_model_version = ");
            builder.push_bind(version);
        }
        if let Some(exclude) = exclude_artifact_uid {
            builder.push(" AND se.artifact_uid <> ");
            builder.push_bind(exclude);
        }
        builder.push(" ORDER BY distance ASC, se.artifact_uid ASC LIMIT ");
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

    /// Nearest skill-identity embeddings to `probe`, resolved to their current
    /// live skill names, within one storage partition and optionally one
    /// vector space, ordered by ascending cosine distance.
    ///
    /// This is the read the skill-manifest ranker uses to score a tenant's
    /// skills by semantic relevance to the turn query in a single round-trip: it
    /// folds the artifact-to-name resolution that
    /// [`Self::live_skill_name_for_artifact`] performs per row into the
    /// neighbor query, so ranking never fans out one query per candidate. Only
    /// live skill artifacts (`kind = 'skill'`, not tombstoned) are returned, so a
    /// stale embedding for a deleted skill never scores a name the ranker cannot
    /// show. `model_scope` constrains neighbors to one embedder's vector space
    /// exactly like [`Self::nearest_skill_embeddings_scoped`], and the
    /// per-partition predicate scopes the scan to the tenant's small skill set.
    pub async fn nearest_named_skill_embeddings_scoped(
        &self,
        storage_partition_id: &str,
        probe: &[f32],
        limit: usize,
        model_scope: Option<(&str, i32)>,
    ) -> Result<Vec<NamedSkillEmbeddingNeighbor>> {
        let mut builder =
            sqlx::QueryBuilder::<sqlx::Postgres>::new("SELECT a.name, (se.embedding <=> ");
        builder.push_bind(HalfVector::from_f32_slice(probe));
        // `ORDER BY distance` below resolves back to this distance expression,
        // keeping the HNSW cosine index eligible; the join only resolves the name.
        builder.push(
            ") AS distance FROM moa.skill_embedding se \
             JOIN moa.artifact a ON a.artifact_uid = se.artifact_uid \
             JOIN moa.artifact_serving_pointer p \
               ON p.artifact_uid = se.artifact_uid \
              AND p.revision_uid = se.revision_uid \
             WHERE se.storage_partition_id = ",
        );
        builder.push_bind(storage_partition_id.to_string());
        builder.push(" AND a.kind = 'skill' AND a.valid_to IS NULL");
        if let Some((model, version)) = model_scope {
            builder.push(" AND se.embedding_model = ");
            builder.push_bind(model.to_string());
            builder.push(" AND se.embedding_model_version = ");
            builder.push_bind(version);
        }
        builder.push(" ORDER BY distance ASC, a.name ASC LIMIT ");
        builder.push_bind(limit as i64);
        let rows = builder
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        rows.iter()
            .map(|row| {
                Ok(NamedSkillEmbeddingNeighbor {
                    skill_name: row.try_get("name").map_err(map_sqlx_error)?,
                    distance: row.try_get::<f64, _>("distance").map_err(map_sqlx_error)?,
                })
            })
            .collect()
    }
}
