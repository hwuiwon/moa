//! SQL repository helpers for hosted eval datasets.

use moa_core::wire::eval::{
    EvalDatasetListRequest, EvalDatasetListResponse, EvalDatasetRegisterRequest,
    EvalDatasetRegisterResponse, EvalDatasetSummary,
};
use moa_core::{types::identifiers::StoragePartitionId, types::identifiers::TenantId};
use serde::Deserialize;
use serde_json::Value;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use super::EvalServiceError;

/// Dataset item prepared for tenant-scoped registration.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalDatasetItemInsert {
    /// Dataset item identifier.
    pub item_id: Uuid,
    /// Tenant that owns the item.
    pub tenant_id: TenantId,
    /// Stored item scope document.
    pub scope: Value,
    /// Query text to replay.
    pub query: String,
    /// Optional expected answer.
    pub expected_answer: Option<String>,
    /// Optional expected chunk identifiers.
    pub expected_chunk_ids: Vec<Uuid>,
    /// Stored metadata document.
    pub metadata: Value,
}

#[derive(Debug, Deserialize)]
struct JsonlDatasetItem {
    item_id: Option<Uuid>,
    tenant_id: Option<TenantId>,
    scope: Option<Value>,
    query: String,
    expected_answer: Option<String>,
    expected_chunk_ids: Option<Vec<Uuid>>,
    metadata: Option<Value>,
}

/// Parses JSONL dataset items and constrains every item to the authorized tenant.
pub fn parse_dataset_items_for_tenant(
    tenant_id: TenantId,
    source_uri: Option<&str>,
    jsonl: &str,
) -> Result<Vec<EvalDatasetItemInsert>, EvalServiceError> {
    let source = source_uri.unwrap_or("<inline-jsonl>");
    jsonl
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(idx, line)| {
            let parsed: JsonlDatasetItem =
                serde_json::from_str(line).map_err(|error| EvalServiceError::InvalidDocument {
                    document_source: source.to_string(),
                    message: format!("invalid JSONL item at line {}: {error}", idx + 1),
                })?;
            if parsed.query.trim().is_empty() {
                return Err(EvalServiceError::InvalidDocument {
                    document_source: source.to_string(),
                    message: format!("dataset item at line {} has an empty query", idx + 1),
                });
            }
            if let Some(item_tenant_id) = parsed.tenant_id
                && item_tenant_id != tenant_id
            {
                return Err(EvalServiceError::DatasetTenantMismatch {
                    line: idx + 1,
                    request_tenant_id: tenant_id,
                    item_tenant_id,
                });
            }
            Ok(EvalDatasetItemInsert {
                item_id: parsed.item_id.unwrap_or_else(Uuid::now_v7),
                tenant_id,
                scope: parsed.scope.unwrap_or_else(|| serde_json::json!({})),
                query: parsed.query,
                expected_answer: parsed.expected_answer,
                expected_chunk_ids: parsed.expected_chunk_ids.unwrap_or_default(),
                metadata: parsed.metadata.unwrap_or_else(|| serde_json::json!({})),
            })
        })
        .collect()
}

pub(super) async fn register_dataset_for_tenant(
    pool: &PgPool,
    request: EvalDatasetRegisterRequest,
) -> Result<EvalDatasetRegisterResponse, EvalServiceError> {
    let storage_partition_id = StoragePartitionId::for_tenant(request.tenant_id).to_string();
    let items = parse_dataset_items_for_tenant(
        request.tenant_id,
        request.source_uri.as_deref(),
        &request.jsonl,
    )?;
    if items.is_empty() {
        return Err(EvalServiceError::InvalidDocument {
            document_source: request
                .source_uri
                .clone()
                .unwrap_or_else(|| "<inline-jsonl>".to_string()),
            message: "dataset contains no items for the authorized tenant".to_string(),
        });
    }

    let mut tx = pool.begin().await?;
    let proposed_dataset_id = Uuid::now_v7();
    let dataset_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO analytics.eval_datasets (dataset_id, name, source_path)
        VALUES ($1, $2, $3)
        ON CONFLICT (name) DO UPDATE
        SET source_path = EXCLUDED.source_path
        RETURNING dataset_id
        "#,
    )
    .bind(proposed_dataset_id)
    .bind(&request.name)
    .bind(request.source_uri.as_deref())
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "DELETE FROM analytics.eval_dataset_items WHERE dataset_id = $1 AND storage_partition_id = $2",
    )
    .bind(dataset_id)
    .bind(&storage_partition_id)
    .execute(&mut *tx)
    .await?;

    let mut item_insert = QueryBuilder::<Postgres>::new(
        r#"
        INSERT INTO analytics.eval_dataset_items (
            item_id,
            dataset_id,
            storage_partition_id,
            scope,
            query,
            expected_answer,
            expected_chunk_ids,
            metadata
        )
        "#,
    );
    item_insert.push_values(&items, |mut row, item| {
        row.push_bind(item.item_id)
            .push_bind(dataset_id)
            .push_bind(StoragePartitionId::for_tenant(item.tenant_id).to_string())
            .push_bind(sqlx::types::Json(&item.scope))
            .push_bind(&item.query)
            .push_bind(item.expected_answer.as_deref())
            .push_bind(&item.expected_chunk_ids)
            .push_bind(sqlx::types::Json(&item.metadata));
    });
    item_insert.build().execute(&mut *tx).await?;

    tx.commit().await?;
    Ok(EvalDatasetRegisterResponse {
        tenant_id: request.tenant_id,
        dataset_id,
        name: request.name,
        items: u64::try_from(items.len())
            .map_err(|_| EvalServiceError::IntegerTooLarge { field: "items" })?,
    })
}

pub(super) async fn list_datasets_for_tenant(
    pool: &PgPool,
    request: EvalDatasetListRequest,
) -> Result<EvalDatasetListResponse, EvalServiceError> {
    let storage_partition_id = StoragePartitionId::for_tenant(request.tenant_id).to_string();
    let rows = sqlx::query(
        r#"
        SELECT d.dataset_id, d.name, d.source_path, COUNT(i.item_id)::BIGINT AS items
        FROM analytics.eval_datasets d
        JOIN analytics.eval_dataset_items i
          ON i.dataset_id = d.dataset_id AND i.storage_partition_id = $1
        GROUP BY d.dataset_id, d.name, d.source_path, d.created_at
        ORDER BY d.created_at DESC
        "#,
    )
    .bind(&storage_partition_id)
    .fetch_all(pool)
    .await?;

    let mut datasets = Vec::with_capacity(rows.len());
    for row in rows {
        let items: i64 = row.try_get("items")?;
        datasets.push(EvalDatasetSummary {
            tenant_id: request.tenant_id,
            dataset_id: row.try_get("dataset_id")?,
            name: row.try_get("name")?,
            items: u64::try_from(items)
                .map_err(|_| EvalServiceError::IntegerTooLarge { field: "items" })?,
            source_uri: row.try_get("source_path")?,
        });
    }
    Ok(EvalDatasetListResponse {
        tenant_id: request.tenant_id,
        datasets,
    })
}

#[derive(Clone, Debug)]
pub(crate) struct ScopedDatasetItem {
    pub(crate) item_id: Uuid,
    pub(crate) tenant_id: TenantId,
    pub(crate) query: String,
    pub(crate) expected_answer: Option<String>,
    pub(crate) expected_chunk_ids: Vec<Uuid>,
}

pub(super) async fn load_dataset_items_for_tenant(
    pool: &PgPool,
    tenant_id: &TenantId,
    dataset_id: Uuid,
    limit: Option<usize>,
) -> Result<Vec<ScopedDatasetItem>, EvalServiceError> {
    let storage_partition_id = StoragePartitionId::for_tenant(*tenant_id).to_string();
    let limit = i64::try_from(limit.unwrap_or(1000))
        .map_err(|_| EvalServiceError::IntegerTooLarge { field: "limit" })?;
    let rows = sqlx::query(
        r#"
        SELECT item_id, storage_partition_id, query, expected_answer, expected_chunk_ids
        FROM analytics.eval_dataset_items
        WHERE dataset_id = $1 AND storage_partition_id = $2
        ORDER BY created_at ASC, item_id ASC
        LIMIT $3
        "#,
    )
    .bind(dataset_id)
    .bind(&storage_partition_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let row_storage_partition_id: String = row.try_get("storage_partition_id")?;
            let tenant_id = Uuid::parse_str(&row_storage_partition_id)
                .map(TenantId::from)
                .map_err(|error| EvalServiceError::InvalidDocument {
                    document_source: "analytics.eval_dataset_items".to_string(),
                    message: format!("stored dataset item tenant id is invalid: {error}"),
                })?;
            Ok(ScopedDatasetItem {
                item_id: row.try_get("item_id")?,
                tenant_id,
                query: row.try_get("query")?,
                expected_answer: row.try_get("expected_answer")?,
                expected_chunk_ids: row.try_get("expected_chunk_ids")?,
            })
        })
        .collect()
}
