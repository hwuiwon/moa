//! Append-only graph changelog outbox writer.

use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{Error, Result};

/// One append-only mutation record for `moa.graph_changelog`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangelogRecord {
    /// Workspace scope for workspace and user rows.
    pub workspace_id: Option<String>,
    /// User scope inside a workspace for user-private rows.
    pub user_id: Option<String>,
    /// Expected scope tier: `global`, `workspace`, or `user`.
    pub scope: String,
    /// Principal identifier that triggered the change.
    pub actor_id: Option<String>,
    /// Principal kind: `user`, `agent`, `system`, `promoter`, or `admin`.
    pub actor_kind: String,
    /// Mutation operation such as `create`, `update`, or `erase`.
    pub op: String,
    /// Target kind: `node` or `edge`.
    pub target_kind: String,
    /// AGE label of the node or edge that changed.
    pub target_label: String,
    /// Stable external target identity.
    pub target_uid: Uuid,
    /// Serialized before/after payload. PHI/restricted payloads are envelope-encrypted in M21.
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
/// `moa.graph_changelog` owns the workspace-version bump through an `AFTER INSERT` trigger, so
/// callers only need to write the immutable outbox record inside the same transaction as the graph
/// mutation.
pub async fn write_and_bump(conn: &mut PgConnection, rec: ChangelogRecord) -> Result<i64> {
    validate_scope(&rec)?;
    let row = sqlx::query_scalar::<_, i64>(
        r#"
        INSERT INTO moa.graph_changelog
            (workspace_id, user_id, actor_id, actor_kind, op, target_kind, target_label,
             target_uid, payload, redaction_marker, pii_class, audit_metadata, cause_change_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        RETURNING change_id
        "#,
    )
    .bind(rec.workspace_id.as_deref())
    .bind(rec.user_id.as_deref())
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

fn validate_scope(rec: &ChangelogRecord) -> Result<()> {
    let expected = match (&rec.workspace_id, &rec.user_id) {
        (None, None) => "global",
        (Some(_), None) => "workspace",
        (Some(_), Some(_)) => "user",
        (None, Some(_)) => return Err(Error::InvalidChangelogScope),
    };

    if rec.scope == expected {
        Ok(())
    } else {
        Err(Error::ChangelogScopeMismatch {
            actual: rec.scope.clone(),
            expected,
        })
    }
}
