//! Vector partition backend promotion helpers.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use pgvector::HalfVector;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::{Error, Result, VectorItem, VectorQuery, VectorStore, pii_rank, validate_dimension};

/// Number of embedding rows copied to Turbopuffer per batch.
pub const PROMOTION_BATCH_SIZE: i64 = 256;
/// Minimum average top-K overlap required before flipping backend state.
pub const PROMOTION_OVERLAP_THRESHOLD: f64 = 0.95;
const VALIDATION_K: usize = 10;

/// Options controlling one vector storage-partition promotion.
#[derive(Debug, Clone)]
pub struct PromotionOptions {
    /// Storage partition to promote.
    pub storage_partition_id: String,
    /// Target vector backend. M27 supports only `turbopuffer`.
    pub target_backend: String,
    /// Percentage of existing vectors sampled for validation.
    pub validate_percent: u32,
    /// Dual-read window after successful validation.
    pub dual_read_hours: u32,
}

/// Summary returned after a promotion attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct PromotionReport {
    /// Storage partition promoted.
    pub storage_partition_id: String,
    /// Number of embedding rows copied to the target backend.
    pub copied: usize,
    /// Average top-K overlap observed during validation.
    pub validation_overlap: f64,
    /// New storage-partition vector backend.
    pub vector_backend: String,
    /// New storage-partition backend state.
    pub vector_backend_state: String,
}

/// Vector storage-partition promotion engine.
pub struct VectorPartitionPromotion {
    pool: PgPool,
    source: Arc<dyn VectorStore>,
    target: Arc<dyn VectorStore>,
}

impl VectorPartitionPromotion {
    /// Creates a vector storage-partition promotion engine.
    #[must_use]
    pub fn new(pool: PgPool, source: Arc<dyn VectorStore>, target: Arc<dyn VectorStore>) -> Self {
        Self {
            pool,
            source,
            target,
        }
    }

    /// Promotes one storage partition to Turbopuffer with validation and a dual-read window.
    pub async fn promote(&self, options: PromotionOptions) -> Result<PromotionReport> {
        if options.target_backend != "turbopuffer" {
            return Err(Error::TurbopufferConfig(format!(
                "unsupported promotion target `{}`",
                options.target_backend
            )));
        }
        set_migrating(&self.pool, &options.storage_partition_id).await?;
        let copied = self
            .copy_storage_partition(&options.storage_partition_id)
            .await?;
        let validation_overlap = self
            .validate_storage_partition(&options.storage_partition_id, options.validate_percent)
            .await?;

        if validation_overlap < PROMOTION_OVERLAP_THRESHOLD {
            rollback_promotion(&self.pool, &options.storage_partition_id).await?;
            return Err(Error::PromotionValidationFailed {
                overlap: validation_overlap,
                required: PROMOTION_OVERLAP_THRESHOLD,
            });
        }

        set_dual_read(
            &self.pool,
            &options.storage_partition_id,
            options.dual_read_hours.max(1),
        )
        .await?;
        Ok(PromotionReport {
            storage_partition_id: options.storage_partition_id,
            copied,
            validation_overlap,
            vector_backend: "turbopuffer".to_string(),
            vector_backend_state: "dual_read".to_string(),
        })
    }

    /// Copies all pgvector embeddings for one storage partition to the target backend.
    pub async fn copy_storage_partition(&self, storage_partition_id: &str) -> Result<usize> {
        let mut copied = 0;
        let mut last_uid = Uuid::nil();

        loop {
            let mut tx = self.pool.begin().await?;
            let rows = fetch_embedding_batch(&mut tx, storage_partition_id, last_uid).await?;
            tx.commit().await?;
            if rows.is_empty() {
                break;
            }

            let items = rows
                .iter()
                .map(EmbeddingRow::to_vector_item)
                .collect::<Result<Vec<_>>>()?;
            self.target.upsert(&items).await?;
            last_uid = rows.last().map(|row| row.uid).unwrap_or(last_uid);
            copied += rows.len();
        }

        Ok(copied)
    }

    /// Validates copied vectors by comparing source and target top-K overlap.
    pub async fn validate_storage_partition(
        &self,
        storage_partition_id: &str,
        validate_percent: u32,
    ) -> Result<f64> {
        let pct = validate_percent.clamp(1, 100);
        let rows = fetch_validation_sample(&self.pool, storage_partition_id, pct).await?;
        if rows.is_empty() {
            return Ok(1.0);
        }

        let mut total = 0.0;
        for row in &rows {
            let query = row.to_vector_query(VALIDATION_K)?;
            let source_hits = self.source.knn(&query).await?;
            let target_hits = self.target.knn(&query).await?;
            total += top_k_overlap(&source_hits, &target_hits, VALIDATION_K);
        }

        Ok(total / rows.len() as f64)
    }
}

/// Rolls a vector partition promotion back to pgvector during the dual-read window.
pub async fn rollback_promotion(pool: &PgPool, storage_partition_id: &str) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, vector_backend, vector_backend_state, dual_read_until, changelog_version)
        VALUES ($1, 'pgvector', 'steady', NULL, 1)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET vector_backend = 'pgvector',
                vector_backend_state = 'steady',
                dual_read_until = NULL,
                changelog_version = moa.storage_partition_state.changelog_version + 1,
                updated_at = now()
        "#,
    )
    .bind(storage_partition_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Finalizes a successful promotion after the dual-read window.
pub async fn finalize_promotion(pool: &PgPool, storage_partition_id: &str) -> Result<()> {
    let state = sqlx::query_scalar::<_, String>(
        "SELECT vector_backend_state FROM moa.storage_partition_state WHERE storage_partition_id = $1",
    )
    .bind(storage_partition_id)
    .fetch_optional(pool)
    .await?
    .unwrap_or_else(|| "steady".to_string());
    if state != "dual_read" {
        return Err(Error::InvalidPromotionState {
            state,
            operation: "finalize promotion",
        });
    }

    sqlx::query(
        r#"
        UPDATE moa.storage_partition_state
           SET vector_backend = 'turbopuffer',
               vector_backend_state = 'steady',
               dual_read_until = NULL,
               changelog_version = changelog_version + 1,
               updated_at = now()
         WHERE storage_partition_id = $1
        "#,
    )
    .bind(storage_partition_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn set_migrating(pool: &PgPool, storage_partition_id: &str) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, vector_backend, vector_backend_state, dual_read_until, changelog_version)
        VALUES ($1, 'pgvector', 'migrating', NULL, 1)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET vector_backend = 'pgvector',
                vector_backend_state = 'migrating',
                dual_read_until = NULL,
                changelog_version = moa.storage_partition_state.changelog_version + 1,
                updated_at = now()
        "#,
    )
    .bind(storage_partition_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn set_dual_read(
    pool: &PgPool,
    storage_partition_id: &str,
    dual_read_hours: u32,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE moa.storage_partition_state
           SET vector_backend = 'turbopuffer',
               vector_backend_state = 'dual_read',
               dual_read_until = now() + ($2::INT * INTERVAL '1 hour'),
               changelog_version = changelog_version + 1,
               updated_at = now()
         WHERE storage_partition_id = $1
        "#,
    )
    .bind(storage_partition_id)
    .bind(i32::try_from(dual_read_hours).unwrap_or(i32::MAX))
    .execute(pool)
    .await?;
    Ok(())
}

async fn fetch_embedding_batch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    storage_partition_id: &str,
    last_uid: Uuid,
) -> Result<Vec<EmbeddingRow>> {
    let rows = sqlx::query(
        r#"
        SELECT uid, storage_partition_id, user_id, label, pii_class, embedding,
               embedding_model, embedding_model_version, valid_to
          FROM moa.embeddings
         WHERE storage_partition_id = $1
           AND uid > $2
         ORDER BY uid
         LIMIT $3
         FOR UPDATE SKIP LOCKED
        "#,
    )
    .bind(storage_partition_id)
    .bind(last_uid)
    .bind(PROMOTION_BATCH_SIZE)
    .fetch_all(&mut **tx)
    .await?;
    rows.into_iter().map(EmbeddingRow::from_row).collect()
}

async fn fetch_validation_sample(
    pool: &PgPool,
    storage_partition_id: &str,
    validate_percent: u32,
) -> Result<Vec<EmbeddingRow>> {
    let rows = sqlx::query(
        r#"
        SELECT uid, storage_partition_id, user_id, label, pii_class, embedding,
               embedding_model, embedding_model_version, valid_to
          FROM moa.embeddings
         WHERE storage_partition_id = $1
           AND valid_to IS NULL
           AND abs(hashtext(uid::TEXT)) % 100 < $2
         ORDER BY uid
        "#,
    )
    .bind(storage_partition_id)
    .bind(i32::try_from(validate_percent).unwrap_or(100))
    .fetch_all(pool)
    .await?;
    rows.into_iter().map(EmbeddingRow::from_row).collect()
}

fn top_k_overlap(
    source_hits: &[crate::VectorMatch],
    target_hits: &[crate::VectorMatch],
    k: usize,
) -> f64 {
    let source = source_hits
        .iter()
        .take(k)
        .map(|hit| hit.uid)
        .collect::<HashSet<_>>();
    let target = target_hits
        .iter()
        .take(k)
        .map(|hit| hit.uid)
        .collect::<HashSet<_>>();
    let denom = source.len().max(target.len()).max(1).min(k);
    source.intersection(&target).count() as f64 / denom as f64
}

#[derive(Debug, Clone)]
struct EmbeddingRow {
    uid: Uuid,
    user_id: Option<String>,
    label: String,
    pii_class: String,
    embedding: HalfVector,
    embedding_model: String,
    embedding_model_version: i32,
    valid_to: Option<DateTime<Utc>>,
}

impl EmbeddingRow {
    fn from_row(row: sqlx::postgres::PgRow) -> Result<Self> {
        Ok(Self {
            uid: row.try_get("uid")?,
            user_id: row.try_get("user_id")?,
            label: row.try_get("label")?,
            pii_class: row.try_get("pii_class")?,
            embedding: row.try_get("embedding")?,
            embedding_model: row.try_get("embedding_model")?,
            embedding_model_version: row.try_get("embedding_model_version")?,
            valid_to: row.try_get("valid_to")?,
        })
    }

    fn to_vector_item(&self) -> Result<VectorItem> {
        let embedding = self.embedding_f32();
        validate_dimension(&embedding)?;
        pii_rank(&self.pii_class)?;
        Ok(VectorItem {
            uid: self.uid,
            user_id: self.user_id.clone(),
            label: self.label.clone(),
            pii_class: self.pii_class.clone(),
            embedding,
            embedding_model: self.embedding_model.clone(),
            embedding_model_version: self.embedding_model_version,
            search_text: None,
            valid_to: self.valid_to,
        })
    }

    fn to_vector_query(&self, k: usize) -> Result<VectorQuery> {
        let embedding = self.embedding_f32();
        validate_dimension(&embedding)?;
        Ok(VectorQuery {
            embedding,
            k,
            label_filter: Some(vec![self.label.clone()]),
            max_pii_class: "restricted".to_string(),
            include_global: false,
            as_of: None,
        })
    }

    fn embedding_f32(&self) -> Vec<f32> {
        self.embedding
            .to_vec()
            .into_iter()
            .map(|value| value.to_f32())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VectorMatch;

    #[test]
    fn dual_read_overlap_is_average_intersection_ratio() {
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let c = Uuid::now_v7();
        let source = vec![
            VectorMatch { uid: a, score: 1.0 },
            VectorMatch { uid: b, score: 0.9 },
        ];
        let target = vec![
            VectorMatch { uid: a, score: 1.0 },
            VectorMatch { uid: c, score: 0.9 },
        ];

        assert_eq!(top_k_overlap(&source, &target, 2), 0.5);
    }

    // The end-to-end `validate_storage_partition` overlap path (which samples
    // real `moa.embeddings` rows via `fetch_validation_sample` and contrasts the
    // pgvector source backend against the promotion target) is exercised by
    // `promotion_validate_storage_partition_scores_real_backend_overlap_db_memory`
    // in `tests/pgvector_store_db_memory.rs`, which drives the production method
    // against a seeded Postgres partition rather than a fixed-return stub.
}
