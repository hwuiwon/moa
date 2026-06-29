//! SQL projection helpers for graph nodes.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection, Postgres, QueryBuilder, Row, postgres::PgRow};
use uuid::Uuid;

use crate::{GraphError, Result};

/// One projected row from `moa.node_index`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeIndexRow {
    /// Stable external graph-node identity.
    pub uid: Uuid,
    /// Graph node label.
    pub label: NodeLabel,
    /// Storage partition owner for tenant and contact scoped rows.
    pub storage_partition_id: Option<String>,
    /// Contact owner for contact-scoped rows.
    pub contact_id: Option<String>,
    /// Generated scope tier stored by Postgres.
    pub scope: String,
    /// Human-readable node name used for seed lookup.
    pub name: String,
    /// PII handling class for retrieval filtering.
    pub pii_class: PiiClass,
    /// End of validity for soft-deleted or superseded nodes.
    pub valid_to: Option<DateTime<Utc>>,
    /// Start of the node's application-time validity interval.
    pub valid_from: DateTime<Utc>,
    /// Projected non-routing properties used for audit hashing and previews.
    pub properties_summary: Option<serde_json::Value>,
    /// Last retrieval/access timestamp.
    pub last_accessed_at: DateTime<Utc>,
    /// Outcome-derived retrieval quality prior, centered at neutral 0.5.
    pub quality_score: f64,
}

impl<'r> FromRow<'r, PgRow> for NodeIndexRow {
    fn from_row(row: &'r PgRow) -> std::result::Result<Self, sqlx::Error> {
        let label = decode_node_label(row.try_get("label")?)?;
        let pii_class = decode_pii_class(row.try_get("pii_class")?)?;
        Ok(Self {
            uid: row.try_get("uid")?,
            label,
            storage_partition_id: row.try_get("storage_partition_id")?,
            contact_id: row.try_get("user_id")?,
            scope: row.try_get("scope")?,
            name: row.try_get("name")?,
            pii_class,
            valid_to: row.try_get("valid_to")?,
            valid_from: row.try_get("valid_from")?,
            properties_summary: row.try_get("properties_summary")?,
            last_accessed_at: row.try_get("last_accessed_at")?,
            quality_score: row.try_get("quality_score")?,
        })
    }
}

/// Supported graph node labels for graph memory nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "PascalCase")]
#[serde(rename_all = "PascalCase")]
pub enum NodeLabel {
    /// Entity node label.
    Entity,
    /// Concept node label.
    Concept,
    /// Decision node label.
    Decision,
    /// Incident node label.
    Incident,
    /// Lesson node label.
    Lesson,
    /// Fact node label.
    Fact,
    /// Source node label.
    Source,
    /// Tenant knowledge document node label.
    Document,
    /// Tenant knowledge chunk node label.
    Chunk,
    /// Tenant knowledge contact-group node label.
    ContactGroup,
}

impl NodeLabel {
    /// Returns the canonical SQL label string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Entity => "Entity",
            Self::Concept => "Concept",
            Self::Decision => "Decision",
            Self::Incident => "Incident",
            Self::Lesson => "Lesson",
            Self::Fact => "Fact",
            Self::Source => "Source",
            Self::Document => "Document",
            Self::Chunk => "Chunk",
            Self::ContactGroup => "ContactGroup",
        }
    }
}

impl FromStr for NodeLabel {
    type Err = GraphError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "Entity" => Ok(Self::Entity),
            "Concept" => Ok(Self::Concept),
            "Decision" => Ok(Self::Decision),
            "Incident" => Ok(Self::Incident),
            "Lesson" => Ok(Self::Lesson),
            "Fact" => Ok(Self::Fact),
            "Source" => Ok(Self::Source),
            "Document" => Ok(Self::Document),
            "Chunk" => Ok(Self::Chunk),
            "ContactGroup" => Ok(Self::ContactGroup),
            other => Err(GraphError::UnknownNodeLabel(other.to_string())),
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
    type Err = GraphError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "pii" => Ok(Self::Pii),
            "phi" => Ok(Self::Phi),
            "restricted" => Ok(Self::Restricted),
            other => Err(GraphError::UnknownPiiClass(other.to_string())),
        }
    }
}

/// Intent to create or supersede one graph-memory node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeWriteIntent {
    /// Stable external graph-node identity.
    pub uid: Uuid,
    /// Graph node label.
    pub label: NodeLabel,
    /// Storage partition scope for tenant and contact rows.
    pub storage_partition_id: Option<String>,
    /// Contact scope inside a tenant for contact-private rows.
    pub contact_id: Option<String>,
    /// Expected scope tier: `global`, `tenant`, or `contact`.
    pub scope: String,
    /// Human-readable node name projected into `moa.node_index`.
    pub name: String,
    /// Node properties stored in the relational projection.
    pub properties: serde_json::Value,
    /// PII handling class for retrieval filtering.
    pub pii_class: PiiClass,
    /// Optional model or extraction confidence.
    pub confidence: Option<f64>,
    /// Start of the bitemporal validity interval.
    pub valid_from: DateTime<Utc>,
    /// Optional 1024-dimension embedding to write in M08.
    pub embedding: Option<Vec<f32>>,
    /// Optional embedding model name.
    pub embedding_model: Option<String>,
    /// Optional embedding model version.
    pub embedding_model_version: Option<i32>,
    /// Retrieval-safe source text used to create the embedding, when it may be indexed for search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_text: Option<String>,
    /// Principal identifier that triggered the mutation.
    pub actor_id: String,
    /// Principal kind written to the graph changelog.
    pub actor_kind: String,
}

/// Intent to update one active graph node's stored properties in place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodePropertyUpdateIntent {
    /// Stable graph-node identity to update.
    pub uid: Uuid,
    /// Replacement node properties stored in the SQL sidecar.
    pub properties: serde_json::Value,
    /// Replacement confidence. `None` preserves the existing sidecar confidence.
    pub confidence: Option<f64>,
    /// Principal identifier that triggered the mutation.
    pub actor_id: String,
    /// Principal kind written to the graph changelog.
    pub actor_kind: String,
}

/// Intent to close one active graph node into an already-existing replacement node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExistingSupersessionIntent {
    /// Node to invalidate.
    pub old_uid: Uuid,
    /// Existing active replacement node.
    pub replacement_uid: Uuid,
    /// Application-time validity end to write on the old node.
    pub valid_to: DateTime<Utc>,
    /// Transaction-time invalidation instant to write on the old node.
    pub invalidated_at: DateTime<Utc>,
    /// Audit reason written to invalidation metadata.
    pub reason: String,
    /// Principal identifier that triggered the mutation.
    pub actor_id: String,
    /// Principal kind written to the graph changelog.
    pub actor_kind: String,
}

/// Intent to attach a vector embedding to an existing graph node.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeEmbeddingIntent {
    /// Node whose vector row should be inserted or replaced.
    pub uid: Uuid,
    /// Dense embedding vector.
    pub embedding: Vec<f32>,
    /// Embedding model identifier.
    pub embedding_model: String,
    /// Embedding model version.
    pub embedding_model_version: i32,
    /// Principal identifier that triggered the mutation.
    pub actor_id: String,
    /// Principal kind written to the graph changelog.
    pub actor_kind: String,
}

/// Looks up graph nodes by name using the `moa.node_index` full-text projection.
///
/// Results are ordered first by text rank and then by the documented memory rank:
/// `0.55 * recency_decay + 0.35 * confidence + 0.10 * normalized_reference_count`,
/// where recency decay is `1 / (1 + age_days)` and references are log-normalized up to
/// 100 references.
pub async fn lookup_seed_by_name(
    conn: &mut PgConnection,
    name: &str,
    limit: i64,
    as_of: Option<DateTime<Utc>>,
) -> Result<Vec<NodeIndexRow>> {
    if limit <= 0 {
        return Ok(Vec::new());
    }

    let mut builder = QueryBuilder::<Postgres>::new(
        r#"
        SELECT uid, label, storage_partition_id, user_id, scope, name, pii_class,
               valid_to, valid_from, properties_summary, last_accessed_at,
               COALESCE(quality_score, 0.5) AS quality_score
        FROM moa.node_index
        WHERE "#,
    );
    crate::push_validity_filter(&mut builder, None, as_of);
    builder.push(
        r#"
          AND name_tsv @@ plainto_tsquery('simple', "#,
    );
    builder.push_bind(name);
    builder.push(
        r#")
        ORDER BY (LOWER(name) = LOWER("#,
    );
    builder.push_bind(name);
    builder.push(
        r#")) DESC,
                 ts_rank(name_tsv, plainto_tsquery('simple', "#,
    );
    builder.push_bind(name);
    builder.push(
        r#")) DESC,
                 (
                   0.55 * (1.0 / (1.0 + GREATEST(EXTRACT(EPOCH FROM (now() - valid_from)) / 86400.0, 0.0))) +
                   0.35 * LEAST(GREATEST(COALESCE(confidence, 0.0), 0.0), 1.0) +
                   0.10 * (LN(LEAST(reference_count, 100)::DOUBLE PRECISION + 1.0) / LN(101.0))
                 ) DESC,
                 uid ASC
        "#,
    );
    builder.push(" LIMIT ");
    builder.push_bind(limit);

    builder
        .build_query_as::<NodeIndexRow>()
        .fetch_all(&mut *conn)
        .await
        .map_err(GraphError::from)
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

fn decode_error(error: GraphError) -> sqlx::Error {
    sqlx::Error::Decode(Box::new(error))
}
