//! Postgres provider-event persistence operations.

use super::row_mapping::*;
use super::*;

/// Persistence operations for idempotent provider event records.
#[async_trait]
pub trait KnowledgeEventRepository: Send + Sync {
    /// Records a provider webhook event idempotently.
    async fn record_provider_event(
        &self,
        event: KnowledgeProviderEventRecord,
    ) -> Result<KnowledgeProviderEventRecord>;
}

#[async_trait]
impl KnowledgeEventRepository for PostgresKnowledgeRepository {
    async fn record_provider_event(
        &self,
        event: KnowledgeProviderEventRecord,
    ) -> Result<KnowledgeProviderEventRecord> {
        let mut conn = self.begin().await?;
        let inserted = sqlx::query(
            r#"
            INSERT INTO moa.knowledge_provider_events (
                provider_event_uid, tenant_id, storage_partition_id, connection_id,
                provider, provider_event_id, event_type, status, payload
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (tenant_id, provider, provider_event_id) DO NOTHING
            RETURNING provider_event_uid, tenant_id, connection_id, provider, provider_event_id,
                      event_type, status, payload, FALSE AS duplicate
            "#,
        )
        .bind(event.provider_event_uid)
        .bind(event.tenant_id.0)
        .bind(storage_partition_id(event.tenant_id))
        .bind(event.connection_uid)
        .bind(&event.provider)
        .bind(&event.provider_event_id)
        .bind(&event.event_type)
        .bind(&event.status)
        .bind(redact_provider_metadata(event.payload))
        .fetch_optional(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;

        let row = match inserted {
            Some(row) => row,
            None => sqlx::query(
                r#"
                    SELECT provider_event_uid, tenant_id, connection_id, provider,
                           provider_event_id, event_type, status, payload, TRUE AS duplicate
                    FROM moa.knowledge_provider_events
                    WHERE tenant_id = $1 AND provider = $2 AND provider_event_id = $3
                    "#,
            )
            .bind(event.tenant_id.0)
            .bind(&event.provider)
            .bind(&event.provider_event_id)
            .fetch_one(conn.as_mut())
            .await
            .map_err(map_sqlx_error)?,
        };
        conn.commit().await.map_err(map_moa_error)?;
        provider_event_from_row(&row)
    }
}
