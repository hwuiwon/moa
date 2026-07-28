//! SQL projection helpers for graph nodes.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use moa_core::types::{memory::InformationBarrierId, security::SensitivityClass};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection, Postgres, QueryBuilder, Row, postgres::PgRow};
use uuid::Uuid;

use crate::{Error, Result};

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
    pub pii_class: SensitivityClass,
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

/// Column projection for `moa.node_index` rows.
///
/// Must stay in sync with [`NodeIndexRow::from_row`]; every column read there
/// is selected here.
pub(crate) const NODE_INDEX_COLUMNS: &str = "uid, label, storage_partition_id, user_id, scope, name, pii_class, \
     valid_to, valid_from, properties_summary, last_accessed_at, \
     COALESCE(quality_score, 0.5) AS quality_score";

/// Extra `moa.node_index` columns a [`SealedNodeRow`] needs on top of
/// [`NODE_INDEX_COLUMNS`] to open restricted/PHI content at the read boundary:
/// the sealed ciphertext plus the `(tenant_id, data_subject_id)` identity used
/// to rebuild the encryption context. Must stay in sync with
/// [`SealedNodeRow::from_row`].
pub(crate) const SEALED_NODE_INDEX_EXTRA_COLUMNS: &str =
    "content_sealed, tenant_id, data_subject_id";

/// One `moa.node_index` row plus the sealed-content columns needed to decrypt it.
///
/// The read path decodes into this at the SQL boundary, then a single
/// centralized store helper opens any restricted/PHI content and yields a
/// plaintext [`NodeIndexRow`]; the sealed blobs never leave the graph crate.
pub(crate) struct SealedNodeRow {
    /// The projected node row. For a sealed row, `name` and `properties_summary`
    /// hold the redaction placeholder until decryption populates the real values.
    pub(crate) row: NodeIndexRow,
    /// One envelope ciphertext containing versioned `{name, properties}` content.
    pub(crate) content_sealed: Option<Vec<u8>>,
    /// Owning tenant id used in the encryption context.
    pub(crate) tenant_id: Option<Uuid>,
    /// Authoritative subject sidecar used to select the KEK.
    pub(crate) data_subject_id: Option<Uuid>,
}

impl<'r> FromRow<'r, PgRow> for SealedNodeRow {
    fn from_row(row: &'r PgRow) -> std::result::Result<Self, sqlx::Error> {
        Ok(Self {
            row: NodeIndexRow::from_row(row)?,
            content_sealed: row.try_get("content_sealed")?,
            tenant_id: row.try_get("tenant_id")?,
            data_subject_id: row.try_get("data_subject_id")?,
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
            "Document" => Ok(Self::Document),
            "Chunk" => Ok(Self::Chunk),
            "ContactGroup" => Ok(Self::ContactGroup),
            other => Err(Error::UnknownNodeLabel(other.to_string())),
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
    /// Authoritative encryption and erasure subject.
    ///
    /// Contact-owned data uses the contact UUID; tenant-owned non-contact data
    /// uses the tenant UUID. This field is mandatory and is never inferred from
    /// dynamic properties or sealed content.
    pub data_subject_id: Uuid,
    /// Expected scope tier: `global`, `tenant`, or `contact`.
    pub scope: String,
    /// Human-readable node name projected into `moa.node_index`.
    pub name: String,
    /// Node properties stored in the relational projection.
    pub properties: serde_json::Value,
    /// PII handling class for retrieval filtering.
    pub pii_class: SensitivityClass,
    /// Optional information-barrier / need-to-know tag persisted to
    /// `moa.node_index.barrier`.
    ///
    /// `None` (the common case) leaves the node unrestricted under the existing
    /// three tiers. A `Some(tag)` node is retrievable only by a caller whose
    /// `moa.cleared_barriers` clearance set contains the tag (fail-closed
    /// need-to-know, enforced by the `rd_barrier_need_to_know` RLS policy). The
    /// tag is a classification label, not sensitive content, so it is stored in
    /// plaintext alongside `pii_class` and never sealed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barrier: Option<InformationBarrierId>,
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

/// Intent to replace one active graph node's complete mutable content in place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeContentUpdateIntent {
    /// Stable graph-node identity to update.
    pub uid: Uuid,
    /// Replacement human-readable name.
    pub name: String,
    /// Replacement node properties.
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

/// Intent to reinforce one active node that ingestion re-observed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NodeReinforcementIntent {
    /// Node whose confidence should be boosted.
    pub uid: Uuid,
    /// Confidence increment applied toward the cap.
    pub step: f64,
    /// Ceiling for the boost; higher existing confidences are preserved.
    pub cap: f64,
}

/// Intent to close one active graph node without a replacement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeExpiryIntent {
    /// Node to invalidate.
    pub uid: Uuid,
    /// Application-time validity end to write on the node.
    pub valid_to: DateTime<Utc>,
    /// Transaction-time invalidation instant to write on the node.
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
pub async fn lookup_seed_by_name<'e, E>(
    executor: E,
    name: &str,
    limit: i64,
    as_of: Option<DateTime<Utc>>,
) -> Result<Vec<NodeIndexRow>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    // Direct-executor callers (the lexical store, offline tests) receive rows
    // without decryption; restricted/PHI content is excluded from `name_tsv`, so
    // a real name query never matches those rows. Store-scoped read paths use
    // [`lookup_seed_candidates`] and decrypt centrally instead.
    Ok(lookup_seed_candidates(executor, name, limit, as_of)
        .await?
        .into_iter()
        .filter(|sealed| !sealed.row.pii_class.is_sealed())
        .map(|sealed| sealed.row)
        .collect())
}

/// Looks up graph nodes by name, returning the sealed-content projection so the
/// store can open restricted/PHI content at the read boundary. See
/// [`lookup_seed_by_name`] for the ranking contract.
pub(crate) async fn lookup_seed_candidates<'e, E>(
    executor: E,
    name: &str,
    limit: i64,
    as_of: Option<DateTime<Utc>>,
) -> Result<Vec<SealedNodeRow>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    if limit <= 0 {
        return Ok(Vec::new());
    }

    let mut builder = QueryBuilder::<Postgres>::new(format!(
        "\n        SELECT {NODE_INDEX_COLUMNS}, {SEALED_NODE_INDEX_EXTRA_COLUMNS}\n        FROM moa.node_index\n        WHERE "
    ));
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
        .build_query_as::<SealedNodeRow>()
        .fetch_all(executor)
        .await
        .map_err(Error::from)
}

/// Looks up graph seed nodes for several names in one round trip.
///
/// Returns one result vector per input name, in the same order as `names`. Each
/// inner vector is the same ranked, `limit`-bounded seed set that
/// [`lookup_seed_by_name`] returns for that name (names with no matches yield an
/// empty vector rather than being dropped). The per-name full-text match,
/// ranking, and limit are evaluated in a single `CROSS JOIN LATERAL` so callers
/// can replace an N+1 loop of [`lookup_seed_by_name`] calls with one query.
///
/// The ranking mirrors [`lookup_seed_by_name`]; the parity test
/// `lookup_seeds_batch_matches_single_name_lookups` pins the two in sync.
pub async fn lookup_seeds_by_names<'e, E>(
    executor: E,
    names: &[&str],
    limit: i64,
    as_of: Option<DateTime<Utc>>,
) -> Result<Vec<Vec<NodeIndexRow>>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    // See [`lookup_seed_by_name`]: this direct-executor wrapper drops the sealed
    // projection. Store-scoped read paths call [`lookup_seed_candidate_batches`].
    Ok(lookup_seed_candidate_batches(executor, names, limit, as_of)
        .await?
        .into_iter()
        .map(|bucket| {
            bucket
                .into_iter()
                .filter(|sealed| !sealed.row.pii_class.is_sealed())
                .map(|sealed| sealed.row)
                .collect()
        })
        .collect())
}

/// Batched seed lookup returning the sealed-content projection per name so the
/// store can open restricted/PHI content centrally. Mirrors the ranking of
/// [`lookup_seeds_by_names`].
pub(crate) async fn lookup_seed_candidate_batches<'e, E>(
    executor: E,
    names: &[&str],
    limit: i64,
    as_of: Option<DateTime<Utc>>,
) -> Result<Vec<Vec<SealedNodeRow>>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    if names.is_empty() || limit <= 0 {
        return Ok(names.iter().map(|_| Vec::new()).collect());
    }

    let owned_names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
    let mut builder =
        QueryBuilder::<Postgres>::new("SELECT queries.ord AS ord, seeds.*\n        FROM unnest(");
    builder.push_bind(owned_names);
    builder.push(
        r#"::text[]) WITH ORDINALITY AS queries(term, ord)
        CROSS JOIN LATERAL (
            SELECT "#,
    );
    builder.push(NODE_INDEX_COLUMNS);
    builder.push(", ");
    builder.push(SEALED_NODE_INDEX_EXTRA_COLUMNS);
    builder.push(
        r#"
            FROM moa.node_index
            WHERE "#,
    );
    crate::push_validity_filter(&mut builder, None, as_of);
    builder.push(
        r#"
              AND name_tsv @@ plainto_tsquery('simple', queries.term)
            ORDER BY (LOWER(name) = LOWER(queries.term)) DESC,
                     ts_rank(name_tsv, plainto_tsquery('simple', queries.term)) DESC,
                     (
                       0.55 * (1.0 / (1.0 + GREATEST(EXTRACT(EPOCH FROM (now() - valid_from)) / 86400.0, 0.0))) +
                       0.35 * LEAST(GREATEST(COALESCE(confidence, 0.0), 0.0), 1.0) +
                       0.10 * (LN(LEAST(reference_count, 100)::DOUBLE PRECISION + 1.0) / LN(101.0))
                     ) DESC,
                     uid ASC
            LIMIT "#,
    );
    builder.push_bind(limit);
    builder.push(
        r#"
        ) AS seeds
        ORDER BY queries.ord
        "#,
    );

    let rows = builder
        .build()
        .fetch_all(executor)
        .await
        .map_err(Error::from)?;

    let mut results: Vec<Vec<SealedNodeRow>> = names.iter().map(|_| Vec::new()).collect();
    for row in &rows {
        let ord: i64 = row.try_get("ord")?;
        let index = usize::try_from(ord - 1).map_err(|error| {
            Error::GraphQuery(format!(
                "seed lookup returned invalid ordinality `{ord}`: {error}"
            ))
        })?;
        let Some(bucket) = results.get_mut(index) else {
            return Err(Error::GraphQuery(format!(
                "seed lookup returned out-of-range ordinality `{ord}` for {} names",
                names.len()
            )));
        };
        bucket.push(SealedNodeRow::from_row(row).map_err(Error::from)?);
    }
    Ok(results)
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

fn decode_pii_class(value: String) -> std::result::Result<SensitivityClass, sqlx::Error> {
    SensitivityClass::from_str(&value).map_err(decode_error)
}

fn decode_error(error: impl std::error::Error + Send + Sync + 'static) -> sqlx::Error {
    sqlx::Error::Decode(Box::new(error))
}
