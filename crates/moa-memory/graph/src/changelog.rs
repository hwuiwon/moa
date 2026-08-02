//! Append-only graph changelog outbox writer.

use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{Error, Result};

/// One append-only mutation record for `moa.graph_changelog`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangelogRecord {
    /// Storage partition boundary for tenant and contact rows.
    pub storage_partition_id: Option<String>,
    /// Contact owner for contact-private rows.
    pub contact_id: Option<String>,
    /// Expected scope tier: `global`, `tenant`, or `contact`.
    pub scope: String,
    /// Principal identifier that triggered the change.
    pub actor_id: Option<String>,
    /// Principal kind: `user`, `contact`, `agent`, `system`, `promoter`, or `admin`.
    pub actor_kind: String,
    /// Mutation operation such as `create`, `update`, or `erase`.
    pub op: String,
    /// Target kind: `node` or `edge`.
    pub target_kind: String,
    /// Graph label of the node or edge that changed.
    pub target_label: String,
    /// Stable external target identity.
    pub target_uid: Uuid,
    /// Serialized before/after payload. Erase rows must use redacted audit payloads only.
    pub payload: serde_json::Value,
    /// Redaction marker written by immutable erase events.
    pub redaction_marker: Option<String>,
    /// Sensitivity class for downstream audit handling.
    pub pii_class: String,
    /// Optional audit context such as approval token JTI or operator reason.
    pub audit_metadata: Option<serde_json::Value>,
    /// Optional parent change for supersession and invalidation chains.
    pub cause_change_id: Option<i64>,
}

/// Inserts a changelog row and returns its monotonic change id.
///
/// `moa.graph_changelog` owns the storage-partition version bump through an `AFTER INSERT` trigger, so
/// callers only need to write the immutable outbox record inside the same transaction as the graph
/// mutation.
pub async fn write_and_bump(conn: &mut PgConnection, rec: ChangelogRecord) -> Result<i64> {
    validate_scope(&rec)?;
    let row = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO moa.graph_changelog
            (storage_partition_id, user_id, actor_id, actor_kind, op, target_kind, target_label,
             target_uid, payload, redaction_marker, pii_class, audit_metadata, cause_change_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        RETURNING change_id
        "#,
    )
    .bind(rec.storage_partition_id.as_deref())
    .bind(rec.contact_id.as_deref())
    .bind(rec.actor_id.as_deref())
    .bind(&rec.actor_kind)
    .bind(&rec.op)
    .bind(&rec.target_kind)
    .bind(&rec.target_label)
    .bind(rec.target_uid)
    .bind(&rec.payload)
    .bind(rec.redaction_marker.as_deref())
    .bind(&rec.pii_class)
    .bind(&rec.audit_metadata)
    .bind(rec.cause_change_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(row)
}

/// Inserts several immutable changelog rows in one statement.
///
/// The statement-level changelog trigger increments every touched storage
/// partition once for the complete statement while this function retains one
/// append-only row per mutation.
pub(crate) async fn write_batch_and_bump(
    conn: &mut PgConnection,
    records: &[ChangelogRecord],
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    for record in records {
        validate_scope(record)?;
    }

    let storage_partition_ids = records
        .iter()
        .map(|record| record.storage_partition_id.as_deref())
        .collect::<Vec<_>>();
    let contact_ids = records
        .iter()
        .map(|record| record.contact_id.as_deref())
        .collect::<Vec<_>>();
    let actor_ids = records
        .iter()
        .map(|record| record.actor_id.as_deref())
        .collect::<Vec<_>>();
    let actor_kinds = records
        .iter()
        .map(|record| record.actor_kind.as_str())
        .collect::<Vec<_>>();
    let operations = records
        .iter()
        .map(|record| record.op.as_str())
        .collect::<Vec<_>>();
    let target_kinds = records
        .iter()
        .map(|record| record.target_kind.as_str())
        .collect::<Vec<_>>();
    let target_labels = records
        .iter()
        .map(|record| record.target_label.as_str())
        .collect::<Vec<_>>();
    let target_uids = records
        .iter()
        .map(|record| record.target_uid)
        .collect::<Vec<_>>();
    let payloads = records
        .iter()
        .map(|record| serde_json::to_string(&record.payload))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let redaction_markers = records
        .iter()
        .map(|record| record.redaction_marker.as_deref())
        .collect::<Vec<_>>();
    let pii_classes = records
        .iter()
        .map(|record| record.pii_class.as_str())
        .collect::<Vec<_>>();
    let audit_metadata = records
        .iter()
        .map(|record| {
            record
                .audit_metadata
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let cause_change_ids = records
        .iter()
        .map(|record| record.cause_change_id)
        .collect::<Vec<_>>();

    sqlx::query(
        r#"
        INSERT INTO moa.graph_changelog
            (storage_partition_id, user_id, actor_id, actor_kind, op, target_kind, target_label,
             target_uid, payload, redaction_marker, pii_class, audit_metadata, cause_change_id)
        SELECT row.storage_partition_id, row.contact_id, row.actor_id, row.actor_kind,
               row.op, row.target_kind, row.target_label, row.target_uid,
               row.payload::JSONB, row.redaction_marker, row.pii_class,
               row.audit_metadata::JSONB, row.cause_change_id
        FROM UNNEST(
            $1::TEXT[], $2::TEXT[], $3::TEXT[], $4::TEXT[], $5::TEXT[], $6::TEXT[],
            $7::TEXT[], $8::UUID[], $9::TEXT[], $10::TEXT[], $11::TEXT[], $12::TEXT[],
            $13::BIGINT[]
        ) WITH ORDINALITY AS row(
            storage_partition_id, contact_id, actor_id, actor_kind, op, target_kind,
            target_label, target_uid, payload, redaction_marker, pii_class,
            audit_metadata, cause_change_id, input_ordinal
        )
        ORDER BY row.input_ordinal
        "#,
    )
    .bind(&storage_partition_ids)
    .bind(&contact_ids)
    .bind(&actor_ids)
    .bind(&actor_kinds)
    .bind(&operations)
    .bind(&target_kinds)
    .bind(&target_labels)
    .bind(&target_uids)
    .bind(&payloads)
    .bind(&redaction_markers)
    .bind(&pii_classes)
    .bind(&audit_metadata)
    .bind(&cause_change_ids)
    .execute(conn)
    .await?;
    Ok(())
}

fn validate_scope(rec: &ChangelogRecord) -> Result<()> {
    let expected = crate::write::expected_scope_tier(
        rec.storage_partition_id.as_deref(),
        rec.contact_id.as_deref(),
    )
    .ok_or(Error::InvalidChangelogScope)?;

    if rec.scope == expected {
        Ok(())
    } else {
        Err(Error::ChangelogScopeMismatch {
            actual: rec.scope.clone(),
            expected,
        })
    }
}
