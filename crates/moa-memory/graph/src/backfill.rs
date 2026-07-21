//! Resumable sealed-content backfill for graph memory.

use moa_core::types::security::SensitivityClass;
use moa_crypto::{EncryptionContext, EncryptionRequest, KeyManagementProvider};
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::{
    GraphError, Result,
    write::{
        REDACTED_NAME_PLACEHOLDER, SEALED_CONTENT_VERSION, SealedNodeContent, is_sealed_class,
    },
};

/// Process-wide advisory lock that serializes backfill finalization.
const FINALIZATION_LOCK: i64 = 0x4d4f_415f_5345_414c;

/// Aggregate work performed by one complete backfill invocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SealedContentBackfillReport {
    /// Rows whose node or embedding state changed across committed batches.
    pub rows_claimed: u64,
    /// Restricted/PHI rows converted to the one-payload sealed state.
    pub rows_sealed: u64,
    /// Rows whose authoritative subject sidecar was populated.
    pub subjects_set: u64,
    /// Pgvector rows removed before finalization.
    pub embeddings_deleted: u64,
    /// Bounded worker transactions committed.
    pub batches_committed: u64,
}

#[derive(Debug)]
struct Candidate {
    uid: Uuid,
    tenant_id: Uuid,
    contact_id: Option<Uuid>,
    storage_partition_id: Option<String>,
    data_subject_id: Option<Uuid>,
    pii_class: SensitivityClass,
    name: String,
    properties: Value,
    content_sealed: Option<Vec<u8>>,
    base_confidence: Option<f64>,
}

/// Converts every historical graph row to the sealed-content invariants.
///
/// Each worker transaction claims at most `batch_size` rows with `SKIP LOCKED`,
/// so independent Kubernetes jobs can cooperate safely. A worker that finds no
/// immediately claimable row takes the finalization advisory lock, performs a
/// non-skipping residue check, and validates the deferred constraints only when
/// every concurrent batch has committed.
pub async fn backfill_memory_sealed_content(
    pool: &PgPool,
    kms: &dyn KeyManagementProvider,
    batch_size: u32,
) -> Result<SealedContentBackfillReport> {
    if batch_size == 0 {
        return Err(GraphError::Backfill(
            "backfill batch size must be greater than zero".to_string(),
        ));
    }

    let mut report = SealedContentBackfillReport::default();
    loop {
        let mut tx = pool.begin().await?;
        let candidates = claim_candidates(tx.as_mut(), batch_size).await?;
        if candidates.is_empty() {
            if finalize_if_complete(tx.as_mut()).await? {
                tx.commit().await?;
                return Ok(report);
            }
            tx.commit().await?;
            tokio::time::sleep(super::write::full_jitter_delay(
                5,
                250,
                report.batches_committed.min(u64::from(u32::MAX)) as u32,
                rand::random(),
            ))
            .await;
            continue;
        }

        let prepared = prepare_candidates(kms, &candidates).await?;
        let mut applied = 0_u64;
        for (candidate, sealed) in candidates.iter().zip(prepared) {
            let expected_subject = candidate.contact_id.unwrap_or(candidate.tenant_id);
            if candidate
                .data_subject_id
                .is_some_and(|actual| actual != expected_subject)
            {
                return Err(GraphError::DataSubjectMismatch {
                    actual: candidate.data_subject_id.unwrap_or(expected_subject),
                    expected: expected_subject,
                });
            }

            let clean_properties = without_confidence_anchor(&candidate.properties);
            let base_confidence = candidate.base_confidence.or_else(|| {
                candidate
                    .properties
                    .get("base_confidence")
                    .and_then(Value::as_f64)
            });
            let node_needs_update = candidate.data_subject_id.is_none()
                || candidate.properties.get("base_confidence").is_some()
                || if is_sealed_class(candidate.pii_class) {
                    candidate.name != REDACTED_NAME_PLACEHOLDER
                        || candidate.properties != serde_json::json!({ "redacted": true })
                        || candidate.content_sealed.is_none()
                } else {
                    candidate.content_sealed.is_some()
                };
            let (name, properties, content_sealed) = if is_sealed_class(candidate.pii_class) {
                (
                    REDACTED_NAME_PLACEHOLDER.to_string(),
                    serde_json::json!({ "redacted": true }),
                    sealed,
                )
            } else {
                (candidate.name.clone(), clean_properties, None)
            };

            if node_needs_update {
                let updated = sqlx::query_scalar::<_, Uuid>(
                    r#"
                    UPDATE moa.node_index
                       SET data_subject_id = $2,
                           name = $3,
                           properties_summary = $4,
                           content_sealed = $5,
                           base_confidence = $6
                     WHERE uid = $1
                       AND data_subject_id IS NOT DISTINCT FROM $7
                       AND content_sealed IS NOT DISTINCT FROM $8
                       AND name = $9
                       AND properties_summary IS NOT DISTINCT FROM $10
                     RETURNING uid
                    "#,
                )
                .bind(candidate.uid)
                .bind(expected_subject)
                .bind(name)
                .bind(properties)
                .bind(content_sealed.as_deref())
                .bind(base_confidence)
                .bind(candidate.data_subject_id)
                .bind(candidate.content_sealed.as_deref())
                .bind(&candidate.name)
                .bind(&candidate.properties)
                .fetch_optional(tx.as_mut())
                .await?;
                if updated.is_none() {
                    continue;
                }
            }

            if node_needs_update && candidate.data_subject_id.is_none() {
                report.subjects_set += 1;
            }
            if is_sealed_class(candidate.pii_class) {
                let deleted = sqlx::query("DELETE FROM moa.embeddings WHERE uid = $1")
                    .bind(candidate.uid)
                    .execute(tx.as_mut())
                    .await?
                    .rows_affected();
                if !node_needs_update && deleted == 0 {
                    continue;
                }
                if node_needs_update {
                    report.rows_sealed += u64::from(candidate.content_sealed.is_none());
                }
                report.embeddings_deleted += deleted;
                if let Some(storage_partition_id) = candidate.storage_partition_id.as_deref() {
                    sqlx::query(
                        "INSERT INTO moa.vector_sync_outbox (storage_partition_id, uid, op) VALUES ($1, $2, 'delete')",
                    )
                    .bind(storage_partition_id)
                    .bind(candidate.uid)
                    .execute(tx.as_mut())
                    .await?;
                }
            } else if !node_needs_update {
                continue;
            }
            applied += 1;
        }

        report.rows_claimed += applied;
        report.batches_committed += u64::from(applied > 0);
        tx.commit().await?;
    }
}

async fn claim_candidates(conn: &mut PgConnection, batch_size: u32) -> Result<Vec<Candidate>> {
    let rows = sqlx::query(
        r#"
        SELECT node.uid, node.tenant_id, node.contact_id, node.storage_partition_id,
               node.data_subject_id, node.pii_class, node.name, node.properties_summary,
               node.content_sealed, node.base_confidence
          FROM moa.node_index AS node
         WHERE node.data_subject_id IS NULL
            OR node.data_subject_id IS DISTINCT FROM COALESCE(node.contact_id, node.tenant_id)
            OR node.properties_summary ? 'base_confidence'
            OR (
                node.pii_class IN ('phi', 'restricted')
                AND (
                    node.name <> '[RESTRICTED]'
                    OR node.properties_summary IS DISTINCT FROM '{"redacted": true}'::jsonb
                    OR node.content_sealed IS NULL
                    OR EXISTS (SELECT 1 FROM moa.embeddings AS embedding WHERE embedding.uid = node.uid)
                )
            )
            OR (node.pii_class NOT IN ('phi', 'restricted') AND node.content_sealed IS NOT NULL)
         ORDER BY node.uid
         LIMIT $1
         FOR UPDATE OF node SKIP LOCKED
        "#,
    )
    .bind(i64::from(batch_size))
    .fetch_all(conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            let pii_class: String = row.try_get("pii_class")?;
            Ok(Candidate {
                uid: row.try_get("uid")?,
                tenant_id: row.try_get("tenant_id")?,
                contact_id: row.try_get("contact_id")?,
                storage_partition_id: row.try_get("storage_partition_id")?,
                data_subject_id: row.try_get("data_subject_id")?,
                pii_class: pii_class.parse()?,
                name: row.try_get("name")?,
                properties: row.try_get("properties_summary")?,
                content_sealed: row.try_get("content_sealed")?,
                base_confidence: row.try_get("base_confidence")?,
            })
        })
        .collect()
}

async fn prepare_candidates(
    kms: &dyn KeyManagementProvider,
    candidates: &[Candidate],
) -> Result<Vec<Option<Vec<u8>>>> {
    let mut prepared = vec![None; candidates.len()];
    let mut groups: BTreeMap<(Uuid, Uuid), Vec<(usize, EncryptionRequest)>> = BTreeMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        if !is_sealed_class(candidate.pii_class) || candidate.content_sealed.is_some() {
            prepared[index] = candidate.content_sealed.clone();
            continue;
        }
        if !candidate.properties.is_object() {
            return Err(GraphError::Backfill(format!(
                "node {} has non-object properties",
                candidate.uid
            )));
        }
        let subject = candidate.contact_id.unwrap_or(candidate.tenant_id);
        let payload = serde_json::to_vec(&SealedNodeContent {
            version: SEALED_CONTENT_VERSION,
            name: candidate.name.clone(),
            properties: without_confidence_anchor(&candidate.properties),
        })?;
        let context = EncryptionContext::new(
            candidate.tenant_id,
            subject,
            candidate.uid.to_string(),
            candidate.pii_class.as_str(),
        );
        groups
            .entry((candidate.tenant_id, subject))
            .or_default()
            .push((index, EncryptionRequest::new(payload, context)));
    }

    for group in groups.into_values() {
        let requests = group
            .iter()
            .map(|(_, request)| request.clone())
            .collect::<Vec<_>>();
        let ciphertexts = moa_crypto::encrypt_batch(kms, &requests).await?;
        for ((index, _), ciphertext) in group.into_iter().zip(ciphertexts) {
            prepared[index] = Some(ciphertext.to_bytes());
        }
    }
    Ok(prepared)
}

fn without_confidence_anchor(properties: &Value) -> Value {
    let mut properties = properties.clone();
    if let Some(object) = properties.as_object_mut() {
        object.remove("base_confidence");
    }
    properties
}

async fn finalize_if_complete(conn: &mut PgConnection) -> Result<bool> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(FINALIZATION_LOCK)
        .execute(&mut *conn)
        .await?;
    let residue: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
              FROM moa.node_index AS node
             WHERE node.data_subject_id IS NULL
                OR node.data_subject_id IS DISTINCT FROM COALESCE(node.contact_id, node.tenant_id)
                OR node.properties_summary ? 'base_confidence'
                OR (
                    node.pii_class IN ('phi', 'restricted')
                    AND (
                        node.name <> '[RESTRICTED]'
                        OR node.properties_summary IS DISTINCT FROM '{"redacted": true}'::jsonb
                        OR node.content_sealed IS NULL
                        OR EXISTS (SELECT 1 FROM moa.embeddings AS embedding WHERE embedding.uid = node.uid)
                    )
                )
                OR (node.pii_class NOT IN ('phi', 'restricted') AND node.content_sealed IS NOT NULL)
        )
        "#,
    )
    .fetch_one(&mut *conn)
    .await?;
    if residue {
        return Ok(false);
    }

    for constraint in [
        "node_index_data_subject_required",
        "node_index_data_subject_scope",
        "node_index_sealed_content_state",
    ] {
        sqlx::query(&format!(
            "ALTER TABLE moa.node_index VALIDATE CONSTRAINT {constraint}"
        ))
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query("ALTER TABLE moa.embeddings VALIDATE CONSTRAINT embeddings_unsealed_content_only")
        .execute(conn)
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::without_confidence_anchor;
    use serde_json::json;

    #[test]
    fn confidence_anchor_is_removed_without_mutating_other_content() {
        assert_eq!(
            without_confidence_anchor(&json!({"base_confidence": 0.8, "fact": "kept"})),
            json!({"fact": "kept"})
        );
    }
}
