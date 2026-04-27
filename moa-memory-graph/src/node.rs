//! SQL projection helpers for AGE graph nodes.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection, Row, postgres::PgRow};
use uuid::Uuid;

use crate::{Error, Result};

/// One projected row from `moa.node_index`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeIndexRow {
    /// Stable external graph-node identity, mirrored from the AGE `uid` property.
    pub uid: Uuid,
    /// AGE vertex label.
    pub label: NodeLabel,
    /// Workspace owner for workspace and user scoped rows.
    pub workspace_id: Option<String>,
    /// User owner for user scoped rows.
    pub user_id: Option<String>,
    /// Generated scope tier stored by Postgres.
    pub scope: String,
    /// Human-readable node name used for seed lookup.
    pub name: String,
    /// PII handling class for retrieval filtering.
    pub pii_class: PiiClass,
    /// End of validity for soft-deleted or superseded nodes.
    pub valid_to: Option<DateTime<Utc>>,
    /// Last retrieval/access timestamp.
    pub last_accessed_at: DateTime<Utc>,
}

impl<'r> FromRow<'r, PgRow> for NodeIndexRow {
    fn from_row(row: &'r PgRow) -> std::result::Result<Self, sqlx::Error> {
        let label = decode_node_label(row.try_get("label")?)?;
        let pii_class = decode_pii_class(row.try_get("pii_class")?)?;
        Ok(Self {
            uid: row.try_get("uid")?,
            label,
            workspace_id: row.try_get("workspace_id")?,
            user_id: row.try_get("user_id")?,
            scope: row.try_get("scope")?,
            name: row.try_get("name")?,
            pii_class,
            valid_to: row.try_get("valid_to")?,
            last_accessed_at: row.try_get("last_accessed_at")?,
        })
    }
}

/// Supported AGE vertex labels for graph memory nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum NodeLabel {
    /// Entity vertex label.
    Entity,
    /// Concept vertex label.
    Concept,
    /// Decision vertex label.
    Decision,
    /// Incident vertex label.
    Incident,
    /// Lesson vertex label.
    Lesson,
    /// Fact vertex label.
    Fact,
    /// Source vertex label.
    Source,
}

impl NodeLabel {
    /// Returns the canonical SQL and AGE label string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Entity => "Entity",
            Self::Concept => "Concept",
            Self::Decision => "Decision",
            Self::Incident => "Incident",
            Self::Lesson => "Lesson",
            Self::Fact => "Fact",
            Self::Source => "Source",
        }
    }
}

impl FromStr for NodeLabel {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "Entity" => Ok(Self::Entity),
            "Concept" => Ok(Self::Concept),
            "Decision" => Ok(Self::Decision),
            "Incident" => Ok(Self::Incident),
            "Lesson" => Ok(Self::Lesson),
            "Fact" => Ok(Self::Fact),
            "Source" => Ok(Self::Source),
            other => Err(Error::UnknownNodeLabel(other.to_string())),
        }
    }
}

/// PII class attached to graph nodes for retrieval filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum PiiClass {
    /// No sensitive data is known on the node.
    None,
    /// Personally identifiable information.
    Pii,
    /// Protected health information.
    Phi,
    /// Restricted data that needs explicit policy handling.
    Restricted,
}

impl PiiClass {
    /// Returns the canonical SQL string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pii => "pii",
            Self::Phi => "phi",
            Self::Restricted => "restricted",
        }
    }
}

impl FromStr for PiiClass {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "pii" => Ok(Self::Pii),
            "phi" => Ok(Self::Phi),
            "restricted" => Ok(Self::Restricted),
            other => Err(Error::UnknownPiiClass(other.to_string())),
        }
    }
}

/// Looks up graph nodes by name using the `moa.node_index` full-text projection.
pub async fn lookup_seed_by_name(
    conn: &mut PgConnection,
    name: &str,
    limit: i64,
) -> Result<Vec<NodeIndexRow>> {
    if limit <= 0 {
        return Ok(Vec::new());
    }

    sqlx::query_as::<_, NodeIndexRow>(
        r#"
        SELECT uid, label, workspace_id, user_id, scope, name, pii_class,
               valid_to, last_accessed_at
        FROM moa.node_index
        WHERE valid_to IS NULL
          AND name_tsv @@ plainto_tsquery('simple', $1)
        ORDER BY ts_rank(name_tsv, plainto_tsquery('simple', $1)) DESC,
                 last_accessed_at DESC
        LIMIT $2
        "#,
    )
    .bind(name)
    .bind(limit)
    .fetch_all(&mut *conn)
    .await
    .map_err(Error::from)
}

/// Updates `last_accessed_at` for projected graph node rows.
pub async fn bump_last_accessed(conn: &mut PgConnection, uids: &[Uuid]) -> Result<()> {
    if uids.is_empty() {
        return Ok(());
    }

    sqlx::query("UPDATE moa.node_index SET last_accessed_at = now() WHERE uid = ANY($1)")
        .bind(uids)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

fn decode_node_label(value: String) -> std::result::Result<NodeLabel, sqlx::Error> {
    NodeLabel::from_str(&value).map_err(decode_error)
}

fn decode_pii_class(value: String) -> std::result::Result<PiiClass, sqlx::Error> {
    PiiClass::from_str(&value).map_err(decode_error)
}

fn decode_error(error: Error) -> sqlx::Error {
    sqlx::Error::Decode(Box::new(error))
}
