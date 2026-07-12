//! Task-summary embedding maintenance for the Postgres session store.
//!
//! These back the background learning-embeddings backfill cron (R2 semantic
//! clustering). Embeddings are computed out-of-band and never on the turn or
//! persist path, so `experience_records.task_embedding` lags writes by up to one
//! tick and every consumer must treat NULL as "not yet embedded" rather than "no
//! match". Selection and population run on the raw maintenance pool with no
//! per-tenant RLS context, exactly like the regression monitor, so the
//! list/set methods sweep every tenant; the nearest-neighbor primitive filters
//! to one tenant explicitly because it is a per-tenant retrieval, not
//! maintenance.

use chrono::{DateTime, Utc};
use moa_core::types::experience::LearningCandidateStatus;
use moa_core::types::experience::LearningCandidateType;
use pgvector::HalfVector;

use super::*;

/// One experience row awaiting a task-summary embedding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingTaskEmbedding {
    /// Experience record identifier.
    pub id: Uuid,
    /// Task summary to embed. Never empty: rows without a summary are excluded
    /// by the selection query since there is nothing to embed. Carried back into
    /// [`PostgresSessionStore::set_experience_task_embeddings`] so the write can
    /// verify the summary is unchanged since it was read (the row's summary is
    /// mutable via `append_experience_record`'s upsert), refusing a vector that
    /// describes a summary the row no longer holds.
    pub task_summary: String,
}

/// The source experiences behind one open `Proposed` skill candidate.
///
/// The filing-time semantic dedup uses these to decide whether a newly distilled
/// experience is a near-duplicate of work already awaiting review: it compares
/// the new experience's task embedding against each open candidate's source
/// experiences' embeddings via [`PostgresSessionStore::nearest_experience_task_embeddings`].
/// `source_experience_ids` is the candidate row's origin list (its defining task),
/// not the payload's accumulated-sibling list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenProposalSource {
    /// Open candidate identifier.
    pub candidate_id: Uuid,
    /// When the candidate was first filed. Used as the deterministic tie-break
    /// (oldest wins) when one experience maps to more than one open candidate.
    pub created_at: DateTime<Utc>,
    /// Origin experiences that defined this candidate's task.
    pub source_experience_ids: Vec<Uuid>,
}

/// A nearest task-embedding neighbor within one tenant.
#[derive(Debug, Clone, PartialEq)]
pub struct ExperienceEmbeddingNeighbor {
    /// Neighbor experience record identifier.
    pub id: Uuid,
    /// Cosine distance from the probe in `[0, 2]` (0.0 = identical direction).
    pub distance: f64,
}

impl PostgresSessionStore {
    /// Lists experience records needing a task-summary embedding, newest first.
    ///
    /// Restricted to rows created at or after `created_since` (recent
    /// recurrence-relevant work) that carry a non-empty `task_summary`. A row
    /// needs (re-)embedding when it has no vector yet, or when its stored vector
    /// belongs to a different embedder than the active one
    /// (`active_model`/`active_model_version`): after an embedder switch the old
    /// vectors live in an incompatible space, so they are re-embedded rather than
    /// left to be compared against probes from the new space. Returns at most
    /// `limit` rows so one backfill tick sends a bounded batch to the embedding
    /// provider. Runs cross-tenant on the maintenance pool.
    pub async fn list_experience_records_missing_task_embedding(
        &self,
        created_since: DateTime<Utc>,
        active_model: &str,
        active_model_version: i32,
        limit: usize,
    ) -> Result<Vec<MissingTaskEmbedding>> {
        let experience_records = self.table_name("experience_records");
        let rows = sqlx::query(&format!(
            "SELECT id, task_summary FROM {experience_records} \
             WHERE task_summary IS NOT NULL \
               AND task_summary <> '' \
               AND created_at >= $1 \
               AND (task_embedding IS NULL \
                    OR task_embedding_model IS DISTINCT FROM $2 \
                    OR task_embedding_model_version IS DISTINCT FROM $3) \
             ORDER BY created_at DESC, id ASC \
             LIMIT $4"
        ))
        .bind(created_since)
        .bind(active_model)
        .bind(active_model_version)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter()
            .map(|row| {
                Ok(MissingTaskEmbedding {
                    id: row.try_get("id").map_err(map_sqlx_error)?,
                    task_summary: row.try_get("task_summary").map_err(map_sqlx_error)?,
                })
            })
            .collect()
    }

    /// Sets the task-summary embedding and its model provenance for a batch of
    /// experience records in one transaction.
    ///
    /// Each entry carries the exact `task_summary` that was embedded. The write
    /// is conditional on that summary still matching the row: `append_experience_record`
    /// can overwrite `task_summary` on a re-assessment, so a summary that changed
    /// between the backfill's read and this write would otherwise get a vector of
    /// the stale text stamped onto it — and, being non-NULL, never re-selected.
    /// A mismatched row is left untouched (its embedding stays NULL, or its old
    /// vector stands) for the next tick to re-embed against the current summary.
    ///
    /// `model_version` records which vector space the bytes belong to so a later
    /// embedder switch is detectable. A no-op for an empty batch.
    pub async fn set_experience_task_embeddings(
        &self,
        embeddings: &[(Uuid, String, Vec<f32>)],
        model: &str,
        model_version: i32,
    ) -> Result<()> {
        if embeddings.is_empty() {
            return Ok(());
        }
        let experience_records = self.table_name("experience_records");
        let mut tx = self.pool.begin().await.map_err(map_sqlx_error)?;
        for (id, task_summary, vector) in embeddings {
            sqlx::query(&format!(
                "UPDATE {experience_records} \
                 SET task_embedding = $1, \
                     task_embedding_model = $2, \
                     task_embedding_model_version = $3 \
                 WHERE id = $4 AND task_summary = $5"
            ))
            .bind(HalfVector::from_f32_slice(vector))
            .bind(model)
            .bind(model_version)
            .bind(id)
            .bind(task_summary)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
        }
        tx.commit().await.map_err(map_sqlx_error)?;
        Ok(())
    }

    /// Returns the nearest task-summary embeddings to `probe` within one tenant,
    /// ordered by ascending cosine distance.
    ///
    /// The R2 stage uses this to cluster recurring task summaries and to route a
    /// new experience to an existing cluster. `exclude_id` drops a self-match
    /// when the probe is itself a stored row. Rows without an embedding are
    /// skipped.
    ///
    /// This does not constrain neighbors to a vector space: the caller must embed
    /// `probe` with the active embedder and, after an embedder switch, the tenant
    /// may still hold vectors from the previous space until the backfill converges
    /// them. Callers that know the probe's space should prefer
    /// [`Self::nearest_task_embeddings_for_experience`] (which scopes to the
    /// representative's own model) or pass a scope; see
    /// [`Self::nearest_experience_task_embeddings_scoped`].
    pub async fn nearest_experience_task_embeddings(
        &self,
        tenant_id: &TenantId,
        probe: &[f32],
        limit: usize,
        exclude_id: Option<Uuid>,
    ) -> Result<Vec<ExperienceEmbeddingNeighbor>> {
        self.nearest_experience_task_embeddings_scoped(tenant_id, probe, limit, exclude_id, None)
            .await
    }

    /// Nearest task-summary embeddings, optionally constrained to one vector space.
    ///
    /// When `model_scope` is `Some((model, version))`, only rows embedded by that
    /// exact embedder are considered, so a probe from one space is never ranked
    /// against vectors from another (the incompatible-space hazard after an
    /// embedder switch). `None` compares against every embedded row in the tenant
    /// and is what [`Self::nearest_experience_task_embeddings`] delegates to.
    ///
    /// Filing-time callers that embed the probe with a specific embedder should
    /// pass that embedder's `(model_id, model_version)` so the ranking stays
    /// within the probe's own space while the backfill converges older vectors.
    pub async fn nearest_experience_task_embeddings_scoped(
        &self,
        tenant_id: &TenantId,
        probe: &[f32],
        limit: usize,
        exclude_id: Option<Uuid>,
        model_scope: Option<(&str, i32)>,
    ) -> Result<Vec<ExperienceEmbeddingNeighbor>> {
        let experience_records = self.table_name("experience_records");
        let mut builder = QueryBuilder::<Postgres>::new("SELECT id, (task_embedding <=> ");
        builder.push_bind(HalfVector::from_f32_slice(probe));
        // `ORDER BY distance` below resolves back to this same distance
        // expression, so the HNSW cosine index remains eligible.
        builder.push(format!(
            ") AS distance FROM {experience_records} WHERE tenant_id = "
        ));
        builder.push_bind(tenant_id.to_string());
        builder.push(" AND task_embedding IS NOT NULL");
        if let Some((model, version)) = model_scope {
            builder.push(" AND task_embedding_model = ");
            builder.push_bind(model.to_string());
            builder.push(" AND task_embedding_model_version = ");
            builder.push_bind(version);
        }
        if let Some(exclude) = exclude_id {
            builder.push(" AND id <> ");
            builder.push_bind(exclude);
        }
        builder.push(" ORDER BY distance ASC, id ASC LIMIT ");
        builder.push_bind(limit as i64);
        let rows = builder
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        rows.iter()
            .map(|row| {
                Ok(ExperienceEmbeddingNeighbor {
                    id: row.try_get("id").map_err(map_sqlx_error)?,
                    distance: row.try_get::<f64, _>("distance").map_err(map_sqlx_error)?,
                })
            })
            .collect()
    }

    /// Returns the nearest task-summary neighbors of one stored experience within
    /// its tenant, or `None` when that experience has no embedding yet.
    ///
    /// The recurrence clustering step probes each exact-fingerprint group's
    /// representative experience against the tenant to discover semantically-equal
    /// groups. Loads the representative's own embedding, then finds its neighbors
    /// within the representative's own vector space (excluding the self-match) so
    /// the same HNSW cosine index serves both paths. Scoping to the
    /// representative's stored `(model, version)` keeps clustering meaningful
    /// across an embedder switch: an old-space representative clusters only among
    /// other old-space rows until the backfill converges them, never against
    /// new-space vectors. `None` is the eventual-consistency contract in action:
    /// an unembedded representative cannot be clustered and stays in its
    /// exact-fingerprint group.
    pub async fn nearest_task_embeddings_for_experience(
        &self,
        tenant_id: &TenantId,
        experience_id: Uuid,
        limit: usize,
    ) -> Result<Option<Vec<ExperienceEmbeddingNeighbor>>> {
        let experience_records = self.table_name("experience_records");
        let row = sqlx::query(&format!(
            "SELECT task_embedding, task_embedding_model, task_embedding_model_version \
             FROM {experience_records} \
             WHERE id = $1 AND tenant_id = $2"
        ))
        .bind(experience_id)
        .bind(tenant_id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let embedding: Option<HalfVector> =
            row.try_get("task_embedding").map_err(map_sqlx_error)?;
        let Some(embedding) = embedding else {
            return Ok(None);
        };
        let model: Option<String> = row
            .try_get("task_embedding_model")
            .map_err(map_sqlx_error)?;
        let model_version: Option<i32> = row
            .try_get("task_embedding_model_version")
            .map_err(map_sqlx_error)?;
        let probe: Vec<f32> = embedding.to_vec().into_iter().map(f32::from).collect();
        // Provenance is written together with the vector, so a non-NULL embedding
        // carries a non-NULL model; scope to it, falling back to an unscoped
        // search only if a row somehow lacks provenance.
        let model_scope = model.as_deref().zip(model_version);
        let neighbors = self
            .nearest_experience_task_embeddings_scoped(
                tenant_id,
                &probe,
                limit,
                Some(experience_id),
                model_scope,
            )
            .await?;
        Ok(Some(neighbors))
    }

    /// Lists the source experiences behind a tenant's open `Proposed` skill
    /// candidates, newest candidate first.
    ///
    /// Backs the filing-time semantic dedup: before filing a new draft, the
    /// distiller checks whether the new experience is a near-duplicate of any open
    /// candidate by comparing task embeddings against these origin experiences.
    /// Only `Proposed` (still-open) skill candidates are returned; decided
    /// candidates are out of scope for dedup (the recurrence cooldown covers
    /// those).
    pub async fn list_open_skill_proposal_sources(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<OpenProposalSource>> {
        let learning_candidates = self.table_name("learning_candidates");
        let rows = sqlx::query(&format!(
            "SELECT id, created_at, source_experience_ids FROM {learning_candidates} \
             WHERE tenant_id = $1 AND candidate_type = $2 AND status = $3 \
             ORDER BY created_at DESC, id ASC"
        ))
        .bind(tenant_id.to_string())
        .bind(LearningCandidateType::Skill.as_str())
        .bind(LearningCandidateStatus::Proposed.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter()
            .map(|row| {
                Ok(OpenProposalSource {
                    candidate_id: row.try_get("id").map_err(map_sqlx_error)?,
                    created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
                    source_experience_ids: row
                        .try_get("source_experience_ids")
                        .map_err(map_sqlx_error)?,
                })
            })
            .collect()
    }
}
