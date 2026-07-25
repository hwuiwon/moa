//! Session attachment storage for user-visible message uploads.

use async_trait::async_trait;
use moa_core::{
    traits::SessionAttachmentStore, types::channel::Attachment,
    types::contact::SessionAttachmentDisposition, types::contact::SessionAttachmentSlot,
    types::contact::SessionAttachmentUpload, types::contact::StoredSessionAttachment,
    types::identifiers::SessionAttachmentId,
};
use sha2::{Digest, Sha256};

use crate::attachment_storage::AttachmentObjectWrite;

use super::*;

#[async_trait]
impl SessionAttachmentStore for PostgresSessionStore {
    async fn put(
        &self,
        slot: &SessionAttachmentSlot,
        upload: SessionAttachmentUpload,
    ) -> Result<StoredSessionAttachment> {
        let table = self.table_name("session_attachments");
        let attachment_id = slot.attachment_id();
        let sha256 = hex::encode(Sha256::digest(&upload.content));
        let size_bytes = i64::try_from(upload.content.len()).map_err(|_| {
            MoaError::StorageError(
                "session attachment content length exceeded i64 range".to_string(),
            )
        })?;
        let object_key =
            self.attachment_store
                .object_key(slot.tenant_id, slot.session_id, attachment_id);

        // The metadata row is claimed before any object is written, so the slot's
        // digest is committed by whichever request wins the insert. A concurrent or
        // later request with different bytes then loses the insert and is rejected
        // below without ever having touched the stored object.
        let claimed = sqlx::query(&format!(
            "INSERT INTO {table} \
             (id, session_id, tenant_id, contact_id, name, mime_type, sha256, size_bytes, object_key) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (id) DO NOTHING \
             RETURNING id, session_id, tenant_id, name, mime_type, sha256, size_bytes"
        ))
        .bind(attachment_id.0)
        .bind(slot.session_id.0)
        .bind(slot.tenant_id.0)
        .bind(upload.contact_id.map(|contact_id| contact_id.0))
        .bind(&upload.name)
        .bind(&upload.mime_type)
        .bind(&sha256)
        .bind(size_bytes)
        .bind(&object_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        let Some(row) = claimed else {
            return self
                .replayed_slot_attachment(&table, slot, &upload, &sha256, size_bytes)
                .await;
        };

        // This request owns the slot, so its digest is the slot's committed content.
        // A pre-existing object can only be an orphan left by an attempt that crashed
        // before claiming the row, so it is compared and replaced only here — a request
        // that lost the claim returns above without touching object storage at all.
        if self
            .attachment_store
            .put_if_absent(&object_key, &upload.content)
            .await?
            == AttachmentObjectWrite::AlreadyPresent
        {
            let existing = self.attachment_store.get(&object_key).await?;
            if hex::encode(Sha256::digest(&existing)) != sha256 {
                tracing::warn!(
                    object_key,
                    "replaced an orphaned session attachment object left by an unclaimed upload"
                );
                self.attachment_store
                    .overwrite(&object_key, &upload.content)
                    .await?;
            }
        }

        Ok(StoredSessionAttachment {
            attachment: attachment_from_row(&row)?,
            disposition: SessionAttachmentDisposition::Created,
        })
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

impl PostgresSessionStore {
    /// Compares a retried upload against the attachment already stored in its slot.
    ///
    /// Byte-identical content with identical admitted metadata is a replay of the same
    /// message and returns the original attachment untouched. Anything else means one
    /// client message id was reused for different content, which is a typed conflict:
    /// overwriting would silently rewrite history the first message already published.
    async fn replayed_slot_attachment(
        &self,
        table: &str,
        slot: &SessionAttachmentSlot,
        upload: &SessionAttachmentUpload,
        sha256: &str,
        size_bytes: i64,
    ) -> Result<StoredSessionAttachment> {
        let attachment_id = slot.attachment_id();
        let row = sqlx::query(&format!(
            "SELECT id, session_id, tenant_id, name, mime_type, sha256, size_bytes \
             FROM {table} \
             WHERE tenant_id = $1 AND session_id = $2 AND id = $3"
        ))
        .bind(slot.tenant_id.0)
        .bind(slot.session_id.0)
        .bind(attachment_id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .ok_or_else(|| {
            // The slot id is taken but not by this tenant/session pair, so the row it
            // collides with is not readable here. Refusing is the only safe answer.
            MoaError::SessionAttachmentSlotConflict(format!(
                "attachment slot {attachment_id} for client message {} is not owned by session {}",
                slot.client_message_id, slot.session_id
            ))
        })?;

        let stored = attachment_from_row(&row)?;
        let stored_size_bytes = stored
            .size_bytes
            .and_then(|size| i64::try_from(size).ok())
            .unwrap_or(-1);
        if stored.sha256.as_deref() != Some(sha256)
            || stored.name != upload.name
            || stored.mime_type.as_deref() != Some(upload.mime_type.as_str())
            || stored_size_bytes != size_bytes
        {
            return Err(MoaError::SessionAttachmentSlotConflict(format!(
                "attachment {} of client message {} was already stored with different content or metadata",
                slot.ordinal, slot.client_message_id
            )));
        }

        Ok(StoredSessionAttachment {
            attachment: stored,
            disposition: SessionAttachmentDisposition::Replayed,
        })
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
