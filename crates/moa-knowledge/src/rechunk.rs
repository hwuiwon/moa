//! Rechunk staging and its single atomic activation boundary.
//!
//! Rechunking a document changes its chunk boundaries, and everything derived
//! from a chunk changes with it: the graph nodes that represent occurrences,
//! the edges that connect them, the embeddings, the source-ACL fingerprints
//! carried onto each occurrence, the occurrence identity itself, and the
//! provenance that ties a chunk back to the parse it came from.
//!
//! Applying those piecemeal is what makes rechunking dangerous. Between two
//! partial writes a reader can see new chunk text under an old occurrence
//! identity, or a graph edge pointing at a chunk that no longer exists, or an
//! ACL snapshot that described a different span of the document. None of those
//! states raise an error; they just answer wrongly.
//!
//! So rechunk stages all six members first, refuses to activate until every one
//! of them is present for every affected document version, and then applies the
//! whole set — including the generation pointer flip — inside one scoped
//! transaction. A failure anywhere rolls the entire boundary back, and the
//! partition keeps serving the chunks it already had.

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::Utc;
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::{EmbeddingGenerationId, RechunkStagingMember, RlsContext};
use moa_core::types::security::SensitivityClass;
use moa_crypto::KeyManagementProvider;
use moa_db::ScopedConn;
use moa_memory_graph::{NodeLabel, NodeWriteIntent, PostgresGraphStore};
use moa_memory_vector::rebuild::activate_generation_in_conn;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::{Error, Result};

/// Actor recorded on graph changelog rows written by a rechunk activation.
const RECHUNK_ACTOR: &str = "knowledge-rechunk";
/// Actor kind recorded on graph changelog rows written by a rechunk activation.
const RECHUNK_ACTOR_KIND: &str = "system";

/// One replacement chunk staged for activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedChunk {
    /// Chunk identity, which is also its graph occurrence identity.
    pub chunk_uid: Uuid,
    /// Content hash of the chunk body.
    pub chunk_hash: String,
    /// Source block hashes the chunk spans.
    #[serde(default)]
    pub block_hashes: Vec<String>,
    /// Heading path from the parsed document.
    #[serde(default)]
    pub heading_path: Vec<String>,
    /// Chunk body text.
    pub text: String,
    /// Position within the document version.
    pub ordinal: i32,
    /// Token count of the chunk body.
    pub token_count: i32,
}

/// One graph node staged by a rechunk.
///
/// Carries no embedding. Vectors come from the candidate generation the same
/// activation promotes, so a staged node cannot introduce a vector under a
/// different embedding identity than the generation it lands with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedGraphNode {
    /// Stable node identity.
    pub uid: Uuid,
    /// Graph vertex label.
    pub label: String,
    /// Human-readable node name.
    pub name: String,
    /// Node properties.
    pub properties: serde_json::Value,
}

/// One graph edge staged by a rechunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedGraphEdge {
    /// Stable edge identity.
    pub uid: Uuid,
    /// Source node.
    pub from_uid: Uuid,
    /// Target node.
    pub to_uid: Uuid,
    /// Edge label.
    pub label: String,
}

/// Graph delta staged for one document version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedGraphDelta {
    /// Replacement occurrence nodes.
    #[serde(default)]
    pub nodes: Vec<StagedGraphNode>,
    /// Replacement occurrence edges.
    #[serde(default)]
    pub edges: Vec<StagedGraphEdge>,
}

/// Source-ACL state staged for one document version.
///
/// Only keyed fingerprints cross this boundary. A provider principal — an
/// email, a group name, a directory id — must never be written to durable
/// rebuild state, so the staged shape holds fingerprint hex and the snapshot
/// identity that produced it, nothing that can be read back as an identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedAclSnapshot {
    /// Immutable snapshot the fingerprints were captured from.
    pub snapshot_uid: Uuid,
    /// Provider revision the snapshot describes.
    pub provider_revision: String,
    /// Allow-entry principal fingerprints, hex encoded.
    #[serde(default)]
    pub allow_fingerprints: Vec<String>,
    /// Deny-entry principal fingerprints, hex encoded.
    #[serde(default)]
    pub deny_fingerprints: Vec<String>,
}

/// Occurrence identity staged for one document version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedOccurrenceIdentity {
    /// Occurrence keys, one per replacement chunk, in chunk order.
    #[serde(default)]
    pub occurrence_keys: Vec<StagedOccurrenceKey>,
}

/// One chunk's occurrence identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedOccurrenceKey {
    /// Chunk this identity belongs to.
    pub chunk_uid: Uuid,
    /// Stable occurrence key distinguishing this occurrence from an identical
    /// span in another document.
    pub occurrence_key: String,
}

/// Provenance staged for one document version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedProvenance {
    /// Parser that produced the replacement chunks.
    pub parser_provider: String,
    /// Content hash of the parsed document the chunks came from.
    pub content_hash: String,
    /// Chunker identity, so a later rebuild can tell which boundaries produced
    /// this state.
    pub chunker: String,
}

/// A complete staged rechunk for one document version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedDocumentRechunk {
    /// Document version being replaced.
    pub document_version_uid: Uuid,
    /// Replacement chunks.
    pub chunks: Vec<StagedChunk>,
    /// Replacement graph delta.
    pub graph_delta: StagedGraphDelta,
    /// Candidate embedding uids covering the replacement chunks.
    pub embedding_uids: Vec<Uuid>,
    /// Carried-forward source-ACL fingerprints.
    pub acl_snapshot: StagedAclSnapshot,
    /// Occurrence identity for the replacement chunks.
    pub occurrence_identity: StagedOccurrenceIdentity,
    /// Provenance for the replacement chunks.
    pub provenance: StagedProvenance,
}

/// What one rechunk activation replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RechunkActivation {
    /// Document versions whose state was replaced.
    pub document_versions: u64,
    /// Replacement chunk rows written.
    pub chunks: u64,
    /// Graph nodes written.
    pub graph_nodes: u64,
    /// Graph edges written.
    pub graph_edges: u64,
    /// Active-generation pointer version after the flip.
    pub pointer_version: i64,
}

/// Durable staging for rechunk operations.
#[derive(Clone)]
pub struct RechunkStagingRepository {
    pool: PgPool,
    scope: RlsContext,
    kms: Arc<dyn KeyManagementProvider>,
}

impl RechunkStagingRepository {
    /// Creates a rechunk staging repository bound to one tenant scope.
    #[must_use]
    pub fn new(pool: PgPool, scope: RlsContext, kms: Arc<dyn KeyManagementProvider>) -> Self {
        Self { pool, scope, kms }
    }

    fn tenant_id(&self) -> TenantId {
        self.scope.tenant_id()
    }

    fn storage_partition_id(&self) -> String {
        self.scope.storage_partition_id().to_string()
    }

    async fn begin(&self) -> Result<ScopedConn<'_>> {
        ScopedConn::begin_as_app(&self.pool, &self.scope, true)
            .await
            .map_err(|error| Error::Repository(error.to_string()))
    }

    /// Stages a complete rechunk for one document version.
    ///
    /// Writes all six members together. Staging them individually would let a
    /// caller believe a version was ready when one member had silently failed,
    /// which is precisely the partial state the completeness gate exists to
    /// catch — so the gate is enforced, but the API does not invite the mistake.
    pub async fn stage_document(
        &self,
        generation_uid: EmbeddingGenerationId,
        staged: &StagedDocumentRechunk,
    ) -> Result<()> {
        let members = [
            (
                RechunkStagingMember::Chunk,
                serde_json::to_value(&staged.chunks),
            ),
            (
                RechunkStagingMember::GraphDelta,
                serde_json::to_value(&staged.graph_delta),
            ),
            (
                RechunkStagingMember::Embedding,
                serde_json::to_value(&staged.embedding_uids),
            ),
            (
                RechunkStagingMember::AclSnapshot,
                serde_json::to_value(&staged.acl_snapshot),
            ),
            (
                RechunkStagingMember::OccurrenceIdentity,
                serde_json::to_value(&staged.occurrence_identity),
            ),
            (
                RechunkStagingMember::Provenance,
                serde_json::to_value(&staged.provenance),
            ),
        ];

        let mut conn = self.begin().await?;
        for (member, payload) in members {
            let payload = payload
                .map_err(|error| Error::Repository(format!("encode staged {member}: {error}")))?;
            stage_member_in(
                conn.as_mut(),
                generation_uid,
                self.tenant_id(),
                &self.storage_partition_id(),
                staged.document_version_uid,
                member,
                &payload,
            )
            .await?;
        }
        conn.commit()
            .await
            .map_err(|error| Error::Repository(error.to_string()))
    }

    /// Returns document versions staged for one generation.
    pub async fn staged_document_versions(
        &self,
        generation_uid: EmbeddingGenerationId,
    ) -> Result<Vec<Uuid>> {
        let mut conn = self.begin().await?;
        let versions: Vec<Uuid> = sqlx::query_scalar(
            r#"
            SELECT DISTINCT document_version_uid
              FROM moa.knowledge_rechunk_staging
             WHERE generation_uid = $1
             ORDER BY document_version_uid
            "#,
        )
        .bind(generation_uid.0)
        .fetch_all(conn.as_mut())
        .await
        .map_err(map_sqlx)?;
        conn.commit().await.map_err(map_moa)?;
        Ok(versions)
    }

    /// Returns members missing for one staged document version.
    pub async fn missing_members(
        &self,
        generation_uid: EmbeddingGenerationId,
        document_version_uid: Uuid,
    ) -> Result<Vec<RechunkStagingMember>> {
        let mut conn = self.begin().await?;
        let missing =
            missing_members_in(conn.as_mut(), generation_uid, document_version_uid).await?;
        conn.commit().await.map_err(map_moa)?;
        Ok(missing)
    }

    /// Applies every staged document version and flips the generation pointer.
    ///
    /// One scoped transaction covers document/chunk, graph node and edge,
    /// vector, changelog, outbox, and the pointer. Nothing is visible until the
    /// commit, and a failure at any point leaves the partition exactly as it
    /// was — still serving the previous chunks under the previous generation.
    pub async fn activate(
        &self,
        generation_uid: EmbeddingGenerationId,
        expected_pointer_version: i64,
    ) -> Result<RechunkActivation> {
        let storage_partition_id = self.storage_partition_id();
        let mut conn = self.begin().await?;
        let result = self
            .activate_in(
                conn.as_mut(),
                &storage_partition_id,
                generation_uid,
                expected_pointer_version,
            )
            .await;
        match result {
            Ok(activation) => {
                conn.commit().await.map_err(map_moa)?;
                Ok(activation)
            }
            Err(error) => {
                conn.rollback().await.map_err(map_moa)?;
                Err(error)
            }
        }
    }

    async fn activate_in(
        &self,
        conn: &mut PgConnection,
        storage_partition_id: &str,
        generation_uid: EmbeddingGenerationId,
        expected_pointer_version: i64,
    ) -> Result<RechunkActivation> {
        let versions: Vec<Uuid> = sqlx::query_scalar(
            r#"
            SELECT DISTINCT document_version_uid
              FROM moa.knowledge_rechunk_staging
             WHERE generation_uid = $1
             ORDER BY document_version_uid
            "#,
        )
        .bind(generation_uid.0)
        .fetch_all(&mut *conn)
        .await
        .map_err(map_sqlx)?;

        if versions.is_empty() {
            return Err(Error::RechunkStagingIncomplete {
                document_version_uid: Uuid::nil(),
                missing: "no document version is staged for this generation".to_string(),
            });
        }

        // Every completeness check runs before the first write, so a rechunk
        // that is short one member never touches a served row at all.
        for version in &versions {
            let missing = missing_members_in(&mut *conn, generation_uid, *version).await?;
            if !missing.is_empty() {
                return Err(Error::RechunkStagingIncomplete {
                    document_version_uid: *version,
                    missing: missing
                        .iter()
                        .map(|member| member.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                });
            }
        }

        let graph = PostgresGraphStore::scoped_for_app_role(
            self.pool.clone(),
            self.scope.clone(),
            self.kms.clone(),
        );
        let mut chunk_rows = 0_u64;
        let mut graph_nodes = 0_u64;
        let mut graph_edges = 0_u64;

        for version in &versions {
            let staged = load_staged_document(&mut *conn, generation_uid, *version).await?;
            chunk_rows += replace_chunks_in(
                &mut *conn,
                *version,
                &staged.chunks,
                &staged.occurrence_identity,
                &staged.provenance,
            )
            .await?;
            apply_acl_snapshot_in(&mut *conn, *version, &staged.acl_snapshot).await?;
            let (nodes, edges) =
                apply_graph_delta_in(&graph, &mut *conn, self.scope.clone(), &staged.graph_delta)
                    .await?;
            graph_nodes += nodes;
            graph_edges += edges;
        }

        // The pointer flip and the candidate-vector promotion (which also
        // enqueues the external-backend outbox rows) join this transaction
        // rather than following it, so no reader ever sees new chunks under the
        // old generation or the reverse.
        let pointer = activate_generation_in_conn(
            &mut *conn,
            storage_partition_id,
            generation_uid,
            expected_pointer_version,
        )
        .await
        .map_err(|error| Error::Repository(error.to_string()))?;

        Ok(RechunkActivation {
            document_versions: versions.len() as u64,
            chunks: chunk_rows,
            graph_nodes,
            graph_edges,
            pointer_version: pointer.pointer_version,
        })
    }

    /// Discards a generation's staged rechunk state.
    pub async fn discard(&self, generation_uid: EmbeddingGenerationId) -> Result<u64> {
        let mut conn = self.begin().await?;
        let removed =
            sqlx::query("DELETE FROM moa.knowledge_rechunk_staging WHERE generation_uid = $1")
                .bind(generation_uid.0)
                .execute(conn.as_mut())
                .await
                .map_err(map_sqlx)?;
        conn.commit().await.map_err(map_moa)?;
        Ok(removed.rows_affected())
    }
}

async fn stage_member_in(
    conn: &mut PgConnection,
    generation_uid: EmbeddingGenerationId,
    tenant_id: TenantId,
    storage_partition_id: &str,
    document_version_uid: Uuid,
    member: RechunkStagingMember,
    payload: &serde_json::Value,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_rechunk_staging
            (staging_uid, generation_uid, tenant_id, storage_partition_id,
             document_version_uid, member, payload)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (generation_uid, document_version_uid, member) DO UPDATE
            SET payload = EXCLUDED.payload,
                created_at = now()
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(generation_uid.0)
    .bind(tenant_id.0)
    .bind(storage_partition_id)
    .bind(document_version_uid)
    .bind(member.as_str())
    .bind(payload)
    .execute(conn)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

async fn missing_members_in(
    conn: &mut PgConnection,
    generation_uid: EmbeddingGenerationId,
    document_version_uid: Uuid,
) -> Result<Vec<RechunkStagingMember>> {
    let present: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT member
          FROM moa.knowledge_rechunk_staging
         WHERE generation_uid = $1
           AND document_version_uid = $2
        "#,
    )
    .bind(generation_uid.0)
    .bind(document_version_uid)
    .fetch_all(conn)
    .await
    .map_err(map_sqlx)?;
    let present = present.into_iter().collect::<BTreeSet<_>>();
    Ok(RechunkStagingMember::ALL
        .into_iter()
        .filter(|member| !present.contains(member.as_str()))
        .collect())
}

async fn load_staged_document(
    conn: &mut PgConnection,
    generation_uid: EmbeddingGenerationId,
    document_version_uid: Uuid,
) -> Result<StagedDocumentRechunk> {
    let rows = sqlx::query(
        r#"
        SELECT member, payload
          FROM moa.knowledge_rechunk_staging
         WHERE generation_uid = $1
           AND document_version_uid = $2
        "#,
    )
    .bind(generation_uid.0)
    .bind(document_version_uid)
    .fetch_all(conn)
    .await
    .map_err(map_sqlx)?;

    let mut chunks = None;
    let mut graph_delta = None;
    let mut embedding_uids = None;
    let mut acl_snapshot = None;
    let mut occurrence_identity = None;
    let mut provenance = None;

    for row in rows {
        let member: String = row.try_get("member").map_err(map_sqlx)?;
        let payload: serde_json::Value = row.try_get("payload").map_err(map_sqlx)?;
        let member = RechunkStagingMember::parse(&member)
            .map_err(|error| Error::Repository(error.to_string()))?;
        match member {
            RechunkStagingMember::Chunk => chunks = Some(decode_member(member, payload)?),
            RechunkStagingMember::GraphDelta => {
                graph_delta = Some(decode_member(member, payload)?);
            }
            RechunkStagingMember::Embedding => {
                embedding_uids = Some(decode_member(member, payload)?);
            }
            RechunkStagingMember::AclSnapshot => {
                acl_snapshot = Some(decode_member(member, payload)?);
            }
            RechunkStagingMember::OccurrenceIdentity => {
                occurrence_identity = Some(decode_member(member, payload)?);
            }
            RechunkStagingMember::Provenance => {
                provenance = Some(decode_member(member, payload)?);
            }
        }
    }

    let missing = |member: RechunkStagingMember| Error::RechunkStagingIncomplete {
        document_version_uid,
        missing: member.as_str().to_string(),
    };
    Ok(StagedDocumentRechunk {
        document_version_uid,
        chunks: chunks.ok_or_else(|| missing(RechunkStagingMember::Chunk))?,
        graph_delta: graph_delta.ok_or_else(|| missing(RechunkStagingMember::GraphDelta))?,
        embedding_uids: embedding_uids.ok_or_else(|| missing(RechunkStagingMember::Embedding))?,
        acl_snapshot: acl_snapshot.ok_or_else(|| missing(RechunkStagingMember::AclSnapshot))?,
        occurrence_identity: occurrence_identity
            .ok_or_else(|| missing(RechunkStagingMember::OccurrenceIdentity))?,
        provenance: provenance.ok_or_else(|| missing(RechunkStagingMember::Provenance))?,
    })
}

fn decode_member<T: for<'de> Deserialize<'de>>(
    member: RechunkStagingMember,
    payload: serde_json::Value,
) -> Result<T> {
    serde_json::from_value(payload)
        .map_err(|error| Error::Repository(format!("decode staged {member}: {error}")))
}

/// Replaces one document version's chunk rows inside the caller's transaction.
///
/// `graph_node_uid` is written from `chunk_uid` because one chunk row is one
/// graph occurrence; the occurrence key staged alongside it lands in the chunk
/// metadata so the identity travels with the row rather than being recomputed
/// from text that has just changed. Provenance goes into the same metadata for
/// the same reason.
async fn replace_chunks_in(
    conn: &mut PgConnection,
    document_version_uid: Uuid,
    chunks: &[StagedChunk],
    occurrence_identity: &StagedOccurrenceIdentity,
    provenance: &StagedProvenance,
) -> Result<u64> {
    sqlx::query("DELETE FROM moa.knowledge_chunks WHERE document_version_id = $1")
        .bind(document_version_uid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx)?;

    let mut written = 0_u64;
    for chunk in chunks {
        let occurrence_key = occurrence_identity
            .occurrence_keys
            .iter()
            .find(|key| key.chunk_uid == chunk.chunk_uid)
            .map(|key| key.occurrence_key.clone())
            .ok_or_else(|| Error::RechunkStagingIncomplete {
                document_version_uid,
                missing: format!("occurrence identity for chunk {}", chunk.chunk_uid),
            })?;
        let metadata = serde_json::json!({
            "occurrence_key": occurrence_key,
            "provenance": {
                "parser_provider": provenance.parser_provider,
                "content_hash": provenance.content_hash,
                "chunker": provenance.chunker,
            },
        });
        let result = sqlx::query(
            r#"
            INSERT INTO moa.knowledge_chunks (
                chunk_uid, tenant_id, storage_partition_id, document_version_id,
                graph_node_uid, chunk_hash, block_hashes, heading_path, text, ordinal,
                token_count, metadata
            )
            SELECT $2, version.tenant_id, version.storage_partition_id,
                   version.document_version_uid, $2, $3, $4::TEXT[], $5::TEXT[], $6, $7, $8, $9
              FROM moa.knowledge_document_versions AS version
             WHERE version.document_version_uid = $1
            "#,
        )
        .bind(document_version_uid)
        .bind(chunk.chunk_uid)
        .bind(&chunk.chunk_hash)
        .bind(&chunk.block_hashes)
        .bind(&chunk.heading_path)
        .bind(&chunk.text)
        .bind(chunk.ordinal)
        .bind(chunk.token_count)
        .bind(&metadata)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx)?;
        if result.rows_affected() == 0 {
            return Err(Error::Repository(format!(
                "rechunk staged chunk {} for a document version that no longer exists",
                chunk.chunk_uid
            )));
        }
        written += result.rows_affected();
    }
    Ok(written)
}

/// Repoints the document version's object at the carried-forward ACL snapshot.
///
/// Rechunking does not change who may see the document, so the object keeps the
/// admission decision it already had. Writing it explicitly inside the boundary
/// means a rechunk cannot leave an object whose snapshot pointer described the
/// pre-rechunk chunk set.
async fn apply_acl_snapshot_in(
    conn: &mut PgConnection,
    document_version_uid: Uuid,
    snapshot: &StagedAclSnapshot,
) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE moa.knowledge_objects AS object
           SET current_acl_snapshot_id = $2,
               updated_at = now()
          FROM moa.knowledge_document_versions AS version
         WHERE version.document_version_uid = $1
           AND object.object_uid = version.object_id
           AND EXISTS (
               SELECT 1
                 FROM moa.knowledge_source_acl_snapshots AS staged
                WHERE staged.snapshot_uid = $2
                  AND staged.provider_revision = $3
           )
        "#,
    )
    .bind(document_version_uid)
    .bind(snapshot.snapshot_uid)
    .bind(&snapshot.provider_revision)
    .execute(conn)
    .await
    .map_err(map_sqlx)?;
    Ok(())
}

/// Writes the staged graph nodes and edges inside the caller's transaction.
///
/// Routed through `PostgresGraphStore`'s in-connection writers rather than raw
/// SQL so the changelog rows, the storage-partition version bump, and the
/// sealed-content rules are the same ones ordinary graph writes get. Staged
/// nodes carry no embedding: the vectors arrive with the generation this
/// activation promotes.
async fn apply_graph_delta_in(
    graph: &PostgresGraphStore,
    conn: &mut PgConnection,
    scope: RlsContext,
    delta: &StagedGraphDelta,
) -> Result<(u64, u64)> {
    let storage_partition_id = scope.storage_partition_id().to_string();
    let contact_id = scope.contact_id().map(|contact_id| contact_id.to_string());
    let mut nodes = 0_u64;
    for node in &delta.nodes {
        let label = node.label.parse::<NodeLabel>().map_err(|_| {
            Error::Repository(format!(
                "staged graph node label `{}` is unknown",
                node.label
            ))
        })?;
        graph
            .create_node_in_conn(
                &mut *conn,
                NodeWriteIntent {
                    barrier: None,
                    uid: node.uid,
                    data_subject_id: scope.tenant_id().0,
                    label,
                    storage_partition_id: Some(storage_partition_id.clone()),
                    contact_id: contact_id.clone(),
                    scope: scope.tier_str().to_string(),
                    name: node.name.clone(),
                    properties: node.properties.clone(),
                    pii_class: SensitivityClass::None,
                    confidence: None,
                    valid_from: Utc::now(),
                    embedding: None,
                    embedding_model: None,
                    embedding_model_version: None,
                    embedding_text: None,
                    actor_id: RECHUNK_ACTOR.to_string(),
                    actor_kind: RECHUNK_ACTOR_KIND.to_string(),
                },
            )
            .await
            .map_err(|error| Error::Repository(format!("rechunk graph node write: {error}")))?;
        nodes += 1;
    }

    let mut edges = 0_u64;
    for edge in &delta.edges {
        let written = sqlx::query(
            r#"
            INSERT INTO moa.edge_index
                (uid, storage_partition_id, user_id, label, start_uid, end_uid, valid_from)
            VALUES ($1, $2, $3, $4, $5, $6, now())
            ON CONFLICT (uid) DO NOTHING
            "#,
        )
        .bind(edge.uid)
        .bind(&storage_partition_id)
        .bind(contact_id.as_deref())
        .bind(&edge.label)
        .bind(edge.from_uid)
        .bind(edge.to_uid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx)?;
        edges += written.rows_affected();
    }
    Ok((nodes, edges))
}

fn map_sqlx(error: sqlx::Error) -> Error {
    Error::Repository(error.to_string())
}

fn map_moa(error: moa_core::error::MoaError) -> Error {
    Error::Repository(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_acl_state_carries_only_keyed_fingerprints() {
        // Pins: the durable rechunk boundary never holds a provider principal.
        // The staged ACL shape has fields for fingerprint hex and a snapshot
        // identity and nothing that decodes back to an email, group, or
        // directory id. A field added here that could is the regression.
        let staged = StagedAclSnapshot {
            snapshot_uid: Uuid::nil(),
            provider_revision: "rev-7".to_string(),
            allow_fingerprints: vec!["01ab".to_string()],
            deny_fingerprints: Vec::new(),
        };

        let encoded = serde_json::to_value(&staged).expect("staged acl encodes");
        let object = encoded.as_object().expect("staged acl is an object");
        let mut keys = object.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "allow_fingerprints".to_string(),
                "deny_fingerprints".to_string(),
                "provider_revision".to_string(),
                "snapshot_uid".to_string(),
            ],
            "the staged ACL shape gained a field; confirm it cannot carry a principal"
        );
    }

    #[test]
    fn staged_graph_nodes_cannot_carry_an_embedding() {
        // Pins: staged graph nodes have no embedding field, so a rechunk cannot
        // introduce a vector under an embedding identity different from the
        // generation the same activation promotes.
        let node = StagedGraphNode {
            uid: Uuid::nil(),
            label: "Chunk".to_string(),
            name: "chunk".to_string(),
            properties: serde_json::json!({}),
        };

        let encoded = serde_json::to_value(&node).expect("staged node encodes");
        let object = encoded.as_object().expect("staged node is an object");
        assert!(!object.contains_key("embedding"));
        assert!(!object.contains_key("embedding_model"));
    }
}
