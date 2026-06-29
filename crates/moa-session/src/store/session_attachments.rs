//! Session attachment storage for user-visible message uploads.

use async_trait::async_trait;
use moa_core::{Attachment, SessionAttachmentId, SessionAttachmentStore};
use sha2::{Digest, Sha256};

use super::*;

#[async_trait]
impl SessionAttachmentStore for PostgresSessionStore {
    async fn put(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        contact_id: Option<ContactId>,
        name: String,
        mime_type: String,
        content: Vec<u8>,
    ) -> Result<Attachment> {
        let table = self.table_name("session_attachments");
        let id = SessionAttachmentId::new();
        let sha256 = hex::encode(Sha256::digest(&content));
        let size_bytes = i64::try_from(content.len()).map_err(|_| {
            MoaError::StorageError(
                "session attachment content length exceeded i64 range".to_string(),
            )
        })?;
        let object_key = self
            .attachment_store
            .put(tenant_id, session_id, id, content)
            .await?;

        let insert_result = sqlx::query(&format!(
            "INSERT INTO {table} \
             (id, session_id, tenant_id, contact_id, name, mime_type, sha256, size_bytes, object_key) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             RETURNING id, session_id, tenant_id, name, mime_type, sha256, size_bytes"
        ))
        .bind(id.0)
        .bind(session_id.0)
        .bind(tenant_id.0)
        .bind(contact_id.map(|contact_id| contact_id.0))
        .bind(name)
        .bind(mime_type)
        .bind(sha256)
        .bind(size_bytes)
        .bind(&object_key)
        .fetch_one(&self.pool)
        .await;

        let row = match insert_result {
            Ok(row) => row,
            Err(error) => {
                if let Err(cleanup_error) = self.attachment_store.delete(&object_key).await {
                    tracing::warn!(
                        %cleanup_error,
                        object_key,
                        "failed to clean up session attachment object after metadata insert failure"
                    );
                }
                return Err(map_sqlx_error(error));
            }
        };

        attachment_from_row(&row)
    }

    async fn get(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        attachment_id: SessionAttachmentId,
    ) -> Result<(Attachment, Vec<u8>)> {
        let table = self.table_name("session_attachments");
        let row = sqlx::query(&format!(
            "SELECT id, session_id, tenant_id, name, mime_type, sha256, size_bytes, object_key \
             FROM {table} \
             WHERE tenant_id = $1 AND session_id = $2 AND id = $3"
        ))
        .bind(tenant_id.0)
        .bind(session_id.0)
        .bind(attachment_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .ok_or(MoaError::SessionAttachmentNotFound(attachment_id))?;

        let object_key = row.col::<String>("object_key")?;
        let content = self.attachment_store.get(&object_key).await?;
        Ok((attachment_from_row(&row)?, content))
    }

    async fn list_for_session(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
    ) -> Result<Vec<Attachment>> {
        let table = self.table_name("session_attachments");
        let rows = sqlx::query(&format!(
            "SELECT id, session_id, tenant_id, name, mime_type, sha256, size_bytes \
             FROM {table} \
             WHERE tenant_id = $1 AND session_id = $2 \
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(tenant_id.0)
        .bind(session_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        rows.iter().map(attachment_from_row).collect()
    }

    async fn delete(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        attachment_id: SessionAttachmentId,
    ) -> Result<()> {
        let table = self.table_name("session_attachments");
        let object_key = sqlx::query_scalar::<_, String>(&format!(
            "DELETE FROM {table} \
             WHERE tenant_id = $1 AND session_id = $2 AND id = $3 \
             RETURNING object_key"
        ))
        .bind(tenant_id.0)
        .bind(session_id.0)
        .bind(attachment_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        if let Some(object_key) = object_key {
            self.attachment_store.delete(&object_key).await?;
        }

        Ok(())
    }

    async fn delete_for_session(&self, tenant_id: TenantId, session_id: SessionId) -> Result<()> {
        let table = self.table_name("session_attachments");
        let object_keys = sqlx::query(&format!(
            "DELETE FROM {table} \
             WHERE tenant_id = $1 AND session_id = $2 \
             RETURNING object_key"
        ))
        .bind(tenant_id.0)
        .bind(session_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .into_iter()
        .map(|row| row.col::<String>("object_key"))
        .collect::<Result<Vec<_>>>()?;

        for object_key in &object_keys {
            self.attachment_store.delete(object_key).await?;
        }

        Ok(())
    }
}

fn attachment_from_row(row: &sqlx::postgres::PgRow) -> Result<Attachment> {
    let session_id = SessionId(row.col::<uuid::Uuid>("session_id")?);
    let tenant_id = TenantId(row.col::<uuid::Uuid>("tenant_id")?);
    let attachment_id = SessionAttachmentId(row.col::<uuid::Uuid>("id")?);
    let size_bytes = row.col::<i64>("size_bytes")?;
    let size_bytes = u64::try_from(size_bytes)
        .map_err(|_| MoaError::StorageError("session attachment size was negative".to_string()))?;

    Ok(Attachment {
        id: Some(attachment_id),
        name: row.col::<String>("name")?,
        mime_type: Some(row.col::<String>("mime_type")?),
        sha256: Some(row.col::<String>("sha256")?),
        url: Some(format!(
            "/v1/sessions/{session_id}/attachments/{attachment_id}?tenant_id={tenant_id}"
        )),
        path: None,
        size_bytes: Some(size_bytes),
    })
}
