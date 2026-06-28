//! Blob storage and claim-check helpers for large session event payloads.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use moa_core::config::SessionBlobBackend;
use moa_core::{BlobStore, ClaimCheck, Event, MoaConfig, MoaError, Result, SessionId};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};

const BLOB_REF_MARKER: &str = "__moa_blob_ref";
const PREVIEW_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathSegment {
    Key(String),
    Index(usize),
}

/// Filesystem-backed blob store for local claim-check payloads.
#[derive(Debug, Clone)]
pub struct FileBlobStore {
    base_dir: PathBuf,
}

impl FileBlobStore {
    /// Creates a new blob store rooted at the provided directory.
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Creates a blob store using the configured blob directory.
    pub fn from_config(config: &MoaConfig) -> Result<Self> {
        config.session.validate_blob_backend(config.cloud.enabled)?;
        let blob_dir = match config.session.blob_dir.as_deref() {
            Some(path) if !path.trim().is_empty() => expand_local_path(Path::new(path))?,
            _ => Self::default_dir()?,
        };
        Ok(Self::new(blob_dir))
    }

    /// Returns the default shared blob directory.
    pub fn default_dir() -> Result<PathBuf> {
        expand_local_path(Path::new("~/.moa/blobs"))
    }

    /// Returns a blob directory derived from the local database path.
    pub fn default_dir_for_database_path(database_path: &Path) -> Result<PathBuf> {
        if database_path == Path::new(":memory:") {
            return Ok(std::env::temp_dir().join("moa-blobs"));
        }

        let expanded = expand_local_path(database_path)?;
        let parent = expanded.parent().ok_or_else(|| {
            MoaError::ConfigError(format!(
                "database path `{}` did not have a parent directory",
                expanded.display()
            ))
        })?;
        Ok(parent.join("blobs"))
    }

    /// Returns the configured blob root directory.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    fn session_dir(&self, session_id: &SessionId) -> PathBuf {
        self.base_dir.join(session_id.to_string())
    }

    fn blob_path(&self, session_id: &SessionId, blob_id: &str) -> PathBuf {
        self.session_dir(session_id).join(blob_id)
    }
}

/// PostgreSQL-backed blob store for durable claim-check payloads.
#[derive(Debug, Clone)]
pub struct PostgresBlobStore {
    pool: PgPool,
    table_name: String,
}

impl PostgresBlobStore {
    /// Creates a Postgres blob store using the pool's current search path.
    pub async fn new(pool: PgPool) -> Result<Self> {
        Ok(Self {
            pool,
            table_name: "session_blobs".to_string(),
        })
    }

    /// Creates a Postgres blob store bound to one configured schema.
    pub async fn new_in_schema(pool: PgPool, schema_name: &str) -> Result<Self> {
        Ok(Self {
            pool,
            table_name: format!("{}.session_blobs", quote_identifier(schema_name)),
        })
    }
}

#[async_trait]
impl BlobStore for PostgresBlobStore {
    /// Stores a blob under its SHA-256 identifier.
    async fn store(&self, session_id: &SessionId, content: &[u8]) -> Result<String> {
        let blob_id = hex::encode(Sha256::digest(content));
        sqlx::query(&format!(
            "INSERT INTO {} (session_id, blob_id, content)
             VALUES ($1, $2, $3)
             ON CONFLICT (session_id, blob_id) DO NOTHING",
            self.table_name
        ))
        .bind(session_id.0)
        .bind(&blob_id)
        .bind(content)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(blob_id)
    }

    /// Retrieves a previously stored blob.
    async fn get(&self, session_id: &SessionId, blob_id: &str) -> Result<Vec<u8>> {
        let row = sqlx::query(&format!(
            "SELECT content FROM {} WHERE session_id = $1 AND blob_id = $2",
            self.table_name
        ))
        .bind(session_id.0)
        .bind(blob_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        row.map(|row| row.get::<Vec<u8>, _>("content"))
            .ok_or_else(|| MoaError::BlobNotFound(blob_id.to_string()))
    }

    /// Deletes every blob belonging to one session.
    async fn delete_session(&self, session_id: &SessionId) -> Result<()> {
        sqlx::query(&format!(
            "DELETE FROM {} WHERE session_id = $1",
            self.table_name
        ))
        .bind(session_id.0)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
    }

    /// Returns whether the blob already exists.
    async fn exists(&self, session_id: &SessionId, blob_id: &str) -> Result<bool> {
        let exists = sqlx::query_scalar::<_, bool>(&format!(
            "SELECT EXISTS (
                SELECT 1 FROM {} WHERE session_id = $1 AND blob_id = $2
            )",
            self.table_name
        ))
        .bind(session_id.0)
        .bind(blob_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(exists)
    }
}

/// Builds the configured claim-check blob store for a session store.
pub async fn blob_store_from_config(
    config: &MoaConfig,
    pool: PgPool,
) -> Result<Arc<dyn BlobStore>> {
    match config.session.blob_backend {
        SessionBlobBackend::Local => Ok(Arc::new(FileBlobStore::from_config(config)?)),
        SessionBlobBackend::Postgres => match config.database.schema.as_deref() {
            Some(schema_name) => Ok(Arc::new(
                PostgresBlobStore::new_in_schema(pool, schema_name).await?,
            )),
            None => Ok(Arc::new(PostgresBlobStore::new(pool).await?)),
        },
        SessionBlobBackend::ObjectStore => Err(MoaError::ConfigError(
            "session.blob_backend = object_store is not implemented; use postgres for durable cloud claim-check payloads".to_string(),
        )),
    }
}

#[async_trait]
impl BlobStore for FileBlobStore {
    /// Stores a blob under its SHA-256 identifier.
    async fn store(&self, session_id: &SessionId, content: &[u8]) -> Result<String> {
        let blob_id = hex::encode(Sha256::digest(content));
        let path = self.blob_path(session_id, &blob_id);
        if !tokio::fs::try_exists(&path).await? {
            let parent = path.parent().ok_or_else(|| {
                MoaError::StorageError(format!(
                    "blob path `{}` did not have a parent directory",
                    path.display()
                ))
            })?;
            tokio::fs::create_dir_all(parent).await?;
            tokio::fs::write(&path, content).await?;
        }
        Ok(blob_id)
    }

    /// Retrieves a previously stored blob.
    async fn get(&self, session_id: &SessionId, blob_id: &str) -> Result<Vec<u8>> {
        let path = self.blob_path(session_id, blob_id);
        tokio::fs::read(&path)
            .await
            .map_err(|_| MoaError::BlobNotFound(blob_id.to_string()))
    }

    /// Deletes every blob belonging to one session.
    async fn delete_session(&self, session_id: &SessionId) -> Result<()> {
        let dir = self.session_dir(session_id);
        if tokio::fs::try_exists(&dir).await? {
            tokio::fs::remove_dir_all(dir).await?;
        }
        Ok(())
    }

    /// Returns whether the blob already exists.
    async fn exists(&self, session_id: &SessionId, blob_id: &str) -> Result<bool> {
        Ok(tokio::fs::try_exists(self.blob_path(session_id, blob_id)).await?)
    }
}

/// Serializes an event for storage, offloading large string payloads when configured.
pub(crate) async fn encode_event_for_storage(
    blob_store: &dyn BlobStore,
    session_id: &SessionId,
    event: &Event,
    threshold_bytes: usize,
) -> Result<Value> {
    let mut payload = serde_json::to_value(event)?;
    if threshold_bytes == 0 {
        return Ok(payload);
    }

    let mut candidates = Vec::new();
    collect_large_strings(&payload, &mut Vec::new(), threshold_bytes, &mut candidates);
    for (path, value) in candidates {
        let blob_id = blob_store.store(session_id, value.as_bytes()).await?;
        let replacement = json!({
            BLOB_REF_MARKER: {
                "blob_id": blob_id,
                "size": value.len(),
                "preview": preview_text(&value),
            }
        });
        replace_value_at_path(&mut payload, &path, replacement)?;
    }

    Ok(payload)
}

/// Resolves any blob references in a stored event payload and deserializes the event.
pub(crate) async fn decode_event_from_storage(
    blob_store: &dyn BlobStore,
    session_id: &SessionId,
    mut payload: Value,
) -> Result<Event> {
    let mut blob_refs = Vec::new();
    collect_blob_refs(&payload, &mut Vec::new(), &mut blob_refs)?;
    for (path, claim_check) in blob_refs {
        let bytes = blob_store.get(session_id, &claim_check.blob_id).await?;
        let value = String::from_utf8(bytes).map_err(|error| {
            MoaError::StorageError(format!(
                "blob `{}` did not contain valid UTF-8: {error}",
                claim_check.blob_id
            ))
        })?;
        replace_value_at_path(&mut payload, &path, Value::String(value))?;
    }

    serde_json::from_value(payload).map_err(Into::into)
}

fn collect_large_strings(
    value: &Value,
    path: &mut Vec<PathSegment>,
    threshold_bytes: usize,
    out: &mut Vec<(Vec<PathSegment>, String)>,
) {
    match value {
        Value::String(text) => {
            if text.len() > threshold_bytes {
                out.push((path.clone(), text.clone()));
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                path.push(PathSegment::Index(index));
                collect_large_strings(item, path, threshold_bytes, out);
                let _ = path.pop();
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                path.push(PathSegment::Key(key.clone()));
                collect_large_strings(item, path, threshold_bytes, out);
                let _ = path.pop();
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn collect_blob_refs(
    value: &Value,
    path: &mut Vec<PathSegment>,
    out: &mut Vec<(Vec<PathSegment>, ClaimCheck)>,
) -> Result<()> {
    if let Some(claim_check) = claim_check_from_value(value)? {
        out.push((path.clone(), claim_check));
        return Ok(());
    }

    match value {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                path.push(PathSegment::Index(index));
                collect_blob_refs(item, path, out)?;
                let _ = path.pop();
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                path.push(PathSegment::Key(key.clone()));
                collect_blob_refs(item, path, out)?;
                let _ = path.pop();
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }

    Ok(())
}

fn claim_check_from_value(value: &Value) -> Result<Option<ClaimCheck>> {
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    let Some(marker) = object.get(BLOB_REF_MARKER) else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_value(marker.clone()).map_err(
        |error| {
            MoaError::SerializationError(format!("failed to decode claim check marker: {error}"))
        },
    )?))
}

fn replace_value_at_path(root: &mut Value, path: &[PathSegment], replacement: Value) -> Result<()> {
    if path.is_empty() {
        *root = replacement;
        return Ok(());
    }

    match (&path[0], root) {
        (PathSegment::Key(key), Value::Object(map)) => {
            let child = map.get_mut(key).ok_or_else(|| {
                MoaError::StorageError(format!(
                    "claim-check path component `{key}` was missing from payload object"
                ))
            })?;
            replace_value_at_path(child, &path[1..], replacement)
        }
        (PathSegment::Index(index), Value::Array(items)) => {
            let child = items.get_mut(*index).ok_or_else(|| {
                MoaError::StorageError(format!(
                    "claim-check path index `{index}` was out of bounds"
                ))
            })?;
            replace_value_at_path(child, &path[1..], replacement)
        }
        (PathSegment::Key(key), other) => Err(MoaError::StorageError(format!(
            "expected object while resolving path component `{key}`, found {other:?}"
        ))),
        (PathSegment::Index(index), other) => Err(MoaError::StorageError(format!(
            "expected array while resolving path index `{index}`, found {other:?}"
        ))),
    }
}

pub(crate) fn preview_text(text: &str) -> String {
    let mut preview = String::new();
    let mut used_bytes = 0usize;

    for character in text.chars() {
        let char_bytes = character.len_utf8();
        if used_bytes + char_bytes > PREVIEW_BYTES {
            break;
        }
        preview.push(character);
        used_bytes += char_bytes;
    }

    preview
}

fn expand_local_path(path: &Path) -> Result<PathBuf> {
    if path == Path::new(":memory:") {
        return Ok(path.to_path_buf());
    }

    let raw = path.to_string_lossy();
    if let Some(stripped) = raw.strip_prefix("~/") {
        let home = std::env::var_os("HOME").ok_or(MoaError::HomeDirectoryNotFound)?;
        return Ok(PathBuf::from(home).join(stripped));
    }

    Ok(path.to_path_buf())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_blob_store_is_content_addressed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileBlobStore::new(dir.path().join("blobs"));
        let session_id = SessionId::new();

        let first = store
            .store(&session_id, b"same payload")
            .await
            .expect("store first");
        let second = store
            .store(&session_id, b"same payload")
            .await
            .expect("store second");

        assert_eq!(first, second);
        assert!(store.exists(&session_id, &first).await.expect("exists"));
    }

    #[tokio::test]
    async fn file_blob_store_reads_blob_written_by_previous_instance() {
        // Pins: claim-check payloads survive replacing the blob-store instance.
        let dir = tempfile::tempdir().expect("tempdir");
        let blob_dir = dir.path().join("blobs");
        let writer = FileBlobStore::new(blob_dir.clone());
        let session_id = SessionId::new();
        let blob_id = writer
            .store(&session_id, b"durable payload")
            .await
            .expect("store payload");

        let reader = FileBlobStore::new(blob_dir);

        assert_eq!(
            reader
                .get(&session_id, &blob_id)
                .await
                .expect("read payload"),
            b"durable payload"
        );
    }

    #[tokio::test]
    async fn file_blob_store_deletes_session_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileBlobStore::new(dir.path().join("blobs"));
        let session_id = SessionId::new();
        let blob_id = store
            .store(&session_id, b"payload")
            .await
            .expect("store payload");
        assert!(store.exists(&session_id, &blob_id).await.expect("exists"));

        store
            .delete_session(&session_id)
            .await
            .expect("delete session blobs");
        assert!(!store.exists(&session_id, &blob_id).await.expect("exists"));
    }

    #[tokio::test]
    async fn action_review_file_diff_strings_are_claim_checked_and_round_trip() {
        // Pins: action-review diff bodies use session event claim checks and still replay as full previews.
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileBlobStore::new(dir.path().join("blobs"));
        let session_id = SessionId::new();
        let review_id = uuid::Uuid::now_v7();
        let before = "old line\n".repeat(32);
        let after = "new line\n".repeat(32);
        let event = Event::ActionReviewRequested {
            review_id,
            envelope: moa_core::ActionEnvelope {
                review_id,
                tenant_id: moa_core::TenantId::from(uuid::Uuid::from_u128(1)),
                requested_by: moa_core::SessionActorRef::Identity {
                    id: uuid::Uuid::from_u128(2),
                },
                session_id: Some(session_id),
                sub_agent_id: None,
                tool_call_id: moa_core::ToolCallId(review_id),
                tool_name: "file_write".to_string(),
                normalized_input: "src/lib.rs".to_string(),
                input_summary: "write src/lib.rs".to_string(),
                risk_level: moa_core::RiskLevel::High,
                action_class: moa_core::ActionClass::LocalWrite,
                origin_kind: None,
                origin_id: None,
                origin_step_id: None,
                idempotency_key: None,
                created_at: chrono::Utc::now(),
            },
            preview: moa_core::ActionReviewPreview {
                fields: vec![moa_core::ActionReviewField {
                    label: "path".to_string(),
                    value: "src/lib.rs".to_string(),
                }],
                file_diffs: vec![moa_core::ActionReviewFileDiff {
                    path: "src/lib.rs".to_string(),
                    before,
                    after,
                    language_hint: Some("rust".to_string()),
                }],
            },
        };

        let payload = encode_event_for_storage(&store, &session_id, &event, 64)
            .await
            .expect("action-review event should encode with claim checks");
        let first_diff = payload
            .get("data")
            .and_then(|data| data.get("preview"))
            .and_then(|preview| preview.get("file_diffs"))
            .and_then(Value::as_array)
            .and_then(|diffs| diffs.first())
            .expect("encoded action-review event should include first file diff");

        assert!(
            first_diff
                .get("before")
                .and_then(|value| value.get(BLOB_REF_MARKER))
                .is_some(),
            "before diff body should be stored behind a claim check"
        );
        assert!(
            first_diff
                .get("after")
                .and_then(|value| value.get(BLOB_REF_MARKER))
                .is_some(),
            "after diff body should be stored behind a claim check"
        );

        let decoded = decode_event_from_storage(&store, &session_id, payload)
            .await
            .expect("action-review event should decode claim checks");
        assert_eq!(decoded, event);
    }
}
