//! Workspace vector-backend promotion helpers.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use pgvector::HalfVector;
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::{Error, Result, VectorItem, VectorQuery, VectorStore, pii_rank, validate_dimension};

/// Number of embedding rows copied to Turbopuffer per batch.
pub const PROMOTION_BATCH_SIZE: i64 = 256;
/// Minimum average top-K overlap required before flipping backend state.
pub const PROMOTION_OVERLAP_THRESHOLD: f64 = 0.95;
const VALIDATION_K: usize = 10;

/// Options controlling one workspace vector promotion.
#[derive(Debug, Clone)]
pub struct PromotionOptions {
    /// Workspace to promote.
    pub workspace_id: String,
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
    /// Workspace promoted.
    pub workspace_id: String,
    /// Number of embedding rows copied to the target backend.
    pub copied: usize,
    /// Average top-K overlap observed during validation.
    pub validation_overlap: f64,
    /// New workspace vector backend.
    pub vector_backend: String,
    /// New workspace backend state.
    pub vector_backend_state: String,
}

/// Summary returned after promoting one workspace-scoped node to global scope.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePromotionReport {
    /// Stable graph node UID preserved for citations.
    pub uid: Uuid,
    /// Workspace the node was promoted from.
    pub source_workspace_id: String,
    /// Validity timestamp assigned to the promoted global row.
    pub valid_from: DateTime<Utc>,
}

/// Workspace promotion engine.
pub struct WorkspacePromotion {
    pool: PgPool,
    source: Arc<dyn VectorStore>,
    target: Arc<dyn VectorStore>,
}

/// Promotes one workspace-scoped graph node to global scope while preserving its UID.
///
/// The current graph sidecar stores one row per UID, so promotion moves the active row to global
/// scope in place and records a workspace supersession changelog row followed by a global create
/// changelog row. Citation references continue to resolve through the same UID.
pub async fn promote_workspace_node_to_global(
    pool: &PgPool,
    workspace_id: &str,
    uid: Uuid,
) -> Result<NodePromotionReport> {
    let mut tx = pool.begin().await?;
    let global_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
            SELECT 1 FROM moa.node_index
             WHERE uid = $1 AND workspace_id IS NULL AND user_id IS NULL AND valid_to IS NULL
        )",
    )
    .bind(uid)
    .fetch_one(&mut *tx)
    .await?;
    if global_exists {
        return Err(Error::PromotionUidCollision { uid });
    }

    let row = sqlx::query(
        r#"
        SELECT node.label,
               node.pii_class,
               node.properties_summary,
               embedding.embedding_model,
               embedding.embedding_model_version
          FROM moa.node_index AS node
          LEFT JOIN moa.embeddings AS embedding
            ON embedding.uid = node.uid
           AND embedding.workspace_id IS NOT DISTINCT FROM node.workspace_id
           AND embedding.user_id IS NOT DISTINCT FROM node.user_id
         WHERE node.uid = $1
           AND node.workspace_id = $2
           AND node.user_id IS NULL
           AND node.valid_to IS NULL
         FOR UPDATE OF node
        "#,
    )
    .bind(uid)
    .bind(workspace_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| Error::InvalidPromotionState {
        state: "missing_workspace_node".to_string(),
        operation: "promote workspace node to global",
    })?;

    let label: String = row.try_get("label")?;
    let pii_class: String = row.try_get("pii_class")?;
    let properties: serde_json::Value = row
        .try_get::<Option<serde_json::Value>, _>("properties_summary")?
        .unwrap_or_else(|| json!({}));
    let now = Utc::now();
    let old_change = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO moa.graph_changelog
            (workspace_id, actor_kind, op, target_kind, target_label, target_uid, payload,
             pii_class)
        VALUES ($1, 'promoter', 'supersede', 'node', $2, $3, $4, $5)
        RETURNING change_id
        "#,
    )
    .bind(workspace_id)
    .bind(&label)
    .bind(uid)
    .bind(json!({
        "valid_to": now.to_rfc3339(),
        "replacement_uid": uid,
        "promotion_scope": "global",
    }))
    .bind(&pii_class)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        UPDATE moa.node_index
           SET workspace_id = NULL,
               user_id = NULL,
               valid_from = $3,
               properties_summary = jsonb_set(
                   COALESCE(properties_summary, '{}'::jsonb),
                   '{promoted_from_workspace}',
                   to_jsonb($2::TEXT),
                   true
               )
         WHERE uid = $1
           AND workspace_id = $2
           AND user_id IS NULL
           AND valid_to IS NULL
        "#,
    )
    .bind(uid)
    .bind(workspace_id)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        UPDATE moa.embeddings
           SET workspace_id = NULL,
               user_id = NULL
         WHERE uid = $1
           AND workspace_id = $2
           AND user_id IS NULL
           AND valid_to IS NULL
        "#,
    )
    .bind(uid)
    .bind(workspace_id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        r#"
        INSERT INTO moa.graph_changelog
            (workspace_id, actor_kind, op, target_kind, target_label, target_uid, payload,
             pii_class, cause_change_id)
        VALUES (NULL, 'promoter', 'create', 'node', $1, $2, $3, $4, $5)
        "#,
    )
    .bind(&label)
    .bind(uid)
    .bind(json!({ "after": properties, "promoted_from_workspace": workspace_id }))
    .bind(&pii_class)
    .bind(old_change)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(NodePromotionReport {
        uid,
        source_workspace_id: workspace_id.to_string(),
        valid_from: now,
    })
}

impl WorkspacePromotion {
    /// Creates a workspace promotion engine.
    #[must_use]
    pub fn new(pool: PgPool, source: Arc<dyn VectorStore>, target: Arc<dyn VectorStore>) -> Self {
        Self {
            pool,
            source,
            target,
        }
    }

    /// Promotes one workspace to Turbopuffer with validation and a dual-read window.
    pub async fn promote(&self, options: PromotionOptions) -> Result<PromotionReport> {
        if options.target_backend != "turbopuffer" {
            return Err(Error::TurbopufferConfig(format!(
                "unsupported promotion target `{}`",
                options.target_backend
            )));
        }
        set_migrating(&self.pool, &options.workspace_id).await?;
        let copied = self.copy_workspace(&options.workspace_id).await?;
        let validation_overlap = self
            .validate_workspace(&options.workspace_id, options.validate_percent)
            .await?;

        if validation_overlap < PROMOTION_OVERLAP_THRESHOLD {
            rollback_promotion(&self.pool, &options.workspace_id).await?;
            return Err(Error::PromotionValidationFailed {
                overlap: validation_overlap,
                required: PROMOTION_OVERLAP_THRESHOLD,
            });
        }

        set_dual_read(
            &self.pool,
            &options.workspace_id,
            options.dual_read_hours.max(1),
        )
        .await?;
        Ok(PromotionReport {
            workspace_id: options.workspace_id,
            copied,
            validation_overlap,
            vector_backend: "turbopuffer".to_string(),
            vector_backend_state: "dual_read".to_string(),
        })
    }

    /// Copies all pgvector embeddings for one workspace to the target backend.
    pub async fn copy_workspace(&self, workspace_id: &str) -> Result<usize> {
        let mut copied = 0;
        let mut last_uid = Uuid::nil();

        loop {
            let mut tx = self.pool.begin().await?;
            let rows = fetch_embedding_batch(&mut tx, workspace_id, last_uid).await?;
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
    pub async fn validate_workspace(
        &self,
        workspace_id: &str,
        validate_percent: u32,
    ) -> Result<f64> {
        let pct = validate_percent.clamp(1, 100);
        let rows = fetch_validation_sample(&self.pool, workspace_id, pct).await?;
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

/// Rolls a workspace promotion back to pgvector during the dual-read window.
pub async fn rollback_promotion(pool: &PgPool, workspace_id: &str) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO moa.workspace_state
            (workspace_id, vector_backend, vector_backend_state, dual_read_until, changelog_version)
        VALUES ($1, 'pgvector', 'steady', NULL, 1)
        ON CONFLICT (workspace_id) DO UPDATE
            SET vector_backend = 'pgvector',
                vector_backend_state = 'steady',
                dual_read_until = NULL,
                changelog_version = moa.workspace_state.changelog_version + 1,
                updated_at = now()
        "#,
    )
    .bind(workspace_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Finalizes a successful promotion after the dual-read window.
pub async fn finalize_promotion(pool: &PgPool, workspace_id: &str) -> Result<()> {
    let state = sqlx::query_scalar::<_, String>(
        "SELECT vector_backend_state FROM moa.workspace_state WHERE workspace_id = $1",
    )
    .bind(workspace_id)
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
        UPDATE moa.workspace_state
           SET vector_backend = 'turbopuffer',
               vector_backend_state = 'steady',
               dual_read_until = NULL,
               changelog_version = changelog_version + 1,
               updated_at = now()
         WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn set_migrating(pool: &PgPool, workspace_id: &str) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO moa.workspace_state
            (workspace_id, vector_backend, vector_backend_state, dual_read_until, changelog_version)
        VALUES ($1, 'pgvector', 'migrating', NULL, 1)
        ON CONFLICT (workspace_id) DO UPDATE
            SET vector_backend = 'pgvector',
                vector_backend_state = 'migrating',
                dual_read_until = NULL,
                changelog_version = moa.workspace_state.changelog_version + 1,
                updated_at = now()
        "#,
    )
    .bind(workspace_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn set_dual_read(pool: &PgPool, workspace_id: &str, dual_read_hours: u32) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE moa.workspace_state
           SET vector_backend = 'turbopuffer',
               vector_backend_state = 'dual_read',
               dual_read_until = now() + ($2::INT * INTERVAL '1 hour'),
               changelog_version = changelog_version + 1,
               updated_at = now()
         WHERE workspace_id = $1
        "#,
    )
    .bind(workspace_id)
    .bind(i32::try_from(dual_read_hours).unwrap_or(i32::MAX))
    .execute(pool)
    .await?;
    Ok(())
}

async fn fetch_embedding_batch(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workspace_id: &str,
    last_uid: Uuid,
) -> Result<Vec<EmbeddingRow>> {
    let rows = sqlx::query(
        r#"
        SELECT uid, workspace_id, user_id, label, pii_class, embedding,
               embedding_model, embedding_model_version, valid_to
          FROM moa.embeddings
         WHERE workspace_id = $1
           AND uid > $2
         ORDER BY uid
         LIMIT $3
         FOR UPDATE SKIP LOCKED
        "#,
    )
    .bind(workspace_id)
    .bind(last_uid)
    .bind(PROMOTION_BATCH_SIZE)
    .fetch_all(&mut **tx)
    .await?;
    rows.into_iter().map(EmbeddingRow::from_row).collect()
}

async fn fetch_validation_sample(
    pool: &PgPool,
    workspace_id: &str,
    validate_percent: u32,
) -> Result<Vec<EmbeddingRow>> {
    let rows = sqlx::query(
        r#"
        SELECT uid, workspace_id, user_id, label, pii_class, embedding,
               embedding_model, embedding_model_version, valid_to
          FROM moa.embeddings
         WHERE workspace_id = $1
           AND valid_to IS NULL
           AND abs(hashtext(uid::TEXT)) % 100 < $2
         ORDER BY uid
        "#,
    )
    .bind(workspace_id)
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
    workspace_id: Option<String>,
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
            workspace_id: row.try_get("workspace_id")?,
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
            workspace_id: self.workspace_id.clone(),
            user_id: self.user_id.clone(),
            label: self.label.clone(),
            pii_class: self.pii_class.clone(),
            embedding,
            embedding_model: self.embedding_model.clone(),
            embedding_model_version: self.embedding_model_version,
            valid_to: self.valid_to,
        })
    }

    fn to_vector_query(&self, k: usize) -> Result<VectorQuery> {
        let embedding = self.embedding_f32();
        validate_dimension(&embedding)?;
        Ok(VectorQuery {
            workspace_id: self.workspace_id.clone(),
            embedding,
            k,
            label_filter: Some(vec![self.label.clone()]),
            max_pii_class: "restricted".to_string(),
            include_global: false,
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
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::{VECTOR_DIMENSION, VectorMatch};

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

    #[tokio::test]
    async fn validation_rejects_low_overlap() {
        let source = Arc::new(StaticVectorStore::new(vec![Uuid::now_v7()]));
        let target = Arc::new(StaticVectorStore::new(vec![Uuid::now_v7()]));
        let promotion = WorkspacePromotion::new(
            PgPool::connect_lazy("postgres://localhost/moa").expect("lazy pool"),
            source,
            target,
        );
        let overlap = WorkspacePromotion::validate_matches(
            promotion.source.as_ref(),
            promotion.target.as_ref(),
            &[VectorQuery {
                workspace_id: Some("w".to_string()),
                embedding: basis_vector(0),
                k: 1,
                label_filter: None,
                max_pii_class: "restricted".to_string(),
                include_global: false,
            }],
        )
        .await
        .expect("validate matches");
        assert_eq!(overlap, 0.0);
    }

    impl WorkspacePromotion {
        async fn validate_matches(
            source: &dyn VectorStore,
            target: &dyn VectorStore,
            queries: &[VectorQuery],
        ) -> Result<f64> {
            let mut total = 0.0;
            for query in queries {
                let source_hits = source.knn(query).await?;
                let target_hits = target.knn(query).await?;
                total += top_k_overlap(&source_hits, &target_hits, query.k);
            }
            Ok(total / queries.len().max(1) as f64)
        }
    }

    struct StaticVectorStore {
        uids: Vec<Uuid>,
        upserts: Mutex<Vec<VectorItem>>,
    }

    impl StaticVectorStore {
        fn new(uids: Vec<Uuid>) -> Self {
            Self {
                uids,
                upserts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl VectorStore for StaticVectorStore {
        fn backend(&self) -> &'static str {
            "static"
        }

        fn dimension(&self) -> usize {
            VECTOR_DIMENSION
        }

        async fn upsert(&self, items: &[VectorItem]) -> Result<()> {
            self.upserts
                .lock()
                .expect("upserts lock")
                .extend_from_slice(items);
            Ok(())
        }

        async fn knn(&self, _query: &VectorQuery) -> Result<Vec<VectorMatch>> {
            Ok(self
                .uids
                .iter()
                .map(|uid| VectorMatch {
                    uid: *uid,
                    score: 1.0,
                })
                .collect())
        }

        async fn delete(&self, _uids: &[Uuid]) -> Result<()> {
            Ok(())
        }
    }

    fn basis_vector(index: usize) -> Vec<f32> {
        let mut embedding = vec![0.0; VECTOR_DIMENSION];
        embedding[index % VECTOR_DIMENSION] = 1.0;
        embedding
    }
}
