//! Tenant-knowledge chunk hydration for hybrid retrieval.

use std::collections::{HashMap, HashSet};

use moa_core::types::memory::RlsContext;
use moa_db::ScopedConn;
use moa_memory_graph::NodeLabel;
use moa_memory_types::MemoryScope;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::retrieval::types::{
    KnowledgeChunkHydration, KnowledgeChunkWindowPart, Result, RetrievalHit, SourceTier,
};

pub(super) async fn hydrate_knowledge_chunks(
    pool: &PgPool,
    scope: &MemoryScope,
    hits: &mut [RetrievalHit],
    assume_app_role: bool,
) -> Result<()> {
    let chunk_uids = hits
        .iter()
        .filter(|hit| hit.source_tier == SourceTier::TenantKnowledge)
        .filter(|hit| hit.node.label == NodeLabel::Chunk)
        .map(|hit| hit.uid)
        .collect::<Vec<_>>();
    if chunk_uids.is_empty() {
        return Ok(());
    }

    let mut conn = ScopedConn::begin(pool, &RlsContext::tenant(scope.tenant_id())).await?;
    if assume_app_role {
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(conn.as_mut())
            .await?;
    }
    // One graph uid hydrates exactly one document-version occurrence: chunk rows
    // store `graph_node_uid = chunk_uid` under a unique index, so there is no
    // ambiguity for a newest-version tiebreak to resolve. Collapsing candidates
    // here would silently drop a second document's occurrence of identical text.
    let rows = sqlx::query_as::<_, KnowledgeChunkRow>(
        r#"
        SELECT
            c.graph_node_uid,
            c.chunk_uid,
            c.document_version_id AS document_version_uid,
            v.object_id AS object_uid,
            c.chunk_hash,
            c.ordinal,
            c.heading_path,
            c.text,
            c.token_count,
            c.metadata,
            o.source_uri,
            o.title AS source_title,
            o.object_type
        FROM moa.knowledge_chunks c
        JOIN moa.knowledge_document_versions v
          ON v.document_version_uid = c.document_version_id
        JOIN moa.knowledge_objects o
          ON o.object_uid = v.object_id
        WHERE c.tenant_id = $1
          AND c.graph_node_uid = ANY($2)
          AND o.status = 'active'
          AND c.metadata->>'active' IS DISTINCT FROM 'false'
        ORDER BY c.graph_node_uid
        "#,
    )
    .bind(scope.tenant_id().0)
    .bind(&chunk_uids)
    .fetch_all(conn.as_mut())
    .await?;

    let mut chunks_by_graph_uid = rows
        .into_iter()
        .map(|row| {
            (
                row.graph_node_uid,
                KnowledgeChunkHydration {
                    chunk_uid: row.chunk_uid,
                    document_version_uid: row.document_version_uid,
                    object_uid: row.object_uid,
                    chunk_hash: row.chunk_hash,
                    ordinal: row.ordinal,
                    heading_path: row.heading_path,
                    text: row.text,
                    token_count: row.token_count,
                    metadata: row.metadata,
                    source_uri: row.source_uri,
                    source_title: row.source_title,
                    object_type: row.object_type,
                    context_window: Vec::new(),
                },
            )
        })
        .collect::<HashMap<_, _>>();

    hydrate_context_windows(conn.as_mut(), scope, &mut chunks_by_graph_uid).await?;
    conn.commit().await?;

    for hit in hits {
        if let Some(chunk) = chunks_by_graph_uid.remove(&hit.uid) {
            hit.knowledge_chunk = Some(chunk);
        }
    }
    Ok(())
}

/// Populates each hydrated chunk's `context_window` with its ordinal-adjacent
/// siblings (ordinal ±1, same document version) for parent-document retrieval.
///
/// Neighbors are fetched in one batched query keyed by (document version,
/// ordinal) pairs, so expansion never issues a per-chunk round trip. The matched
/// chunk itself is excluded because its own ordinal is never requested.
async fn hydrate_context_windows(
    conn: &mut sqlx::PgConnection,
    scope: &MemoryScope,
    chunks_by_graph_uid: &mut HashMap<Uuid, KnowledgeChunkHydration>,
) -> Result<()> {
    let mut wanted_pairs = HashSet::new();
    let mut version_ids = Vec::new();
    let mut ordinals = Vec::new();
    for chunk in chunks_by_graph_uid.values() {
        for neighbor_ordinal in neighbor_ordinals(chunk.ordinal) {
            if wanted_pairs.insert((chunk.document_version_uid, neighbor_ordinal)) {
                version_ids.push(chunk.document_version_uid);
                ordinals.push(neighbor_ordinal);
            }
        }
    }
    if wanted_pairs.is_empty() {
        return Ok(());
    }

    let neighbor_rows = sqlx::query_as::<_, KnowledgeChunkNeighborRow>(
        r#"
        SELECT
            c.document_version_id AS document_version_uid,
            c.ordinal,
            c.text
        FROM moa.knowledge_chunks c
        JOIN unnest($2::uuid[], $3::int4[]) AS wanted(document_version_uid, ordinal)
          ON c.document_version_id = wanted.document_version_uid
         AND c.ordinal = wanted.ordinal
        WHERE c.tenant_id = $1
          AND c.metadata->>'active' IS DISTINCT FROM 'false'
        "#,
    )
    .bind(scope.tenant_id().0)
    .bind(&version_ids)
    .bind(&ordinals)
    .fetch_all(conn)
    .await?;

    let neighbor_texts = neighbor_rows
        .into_iter()
        .map(|row| ((row.document_version_uid, row.ordinal), row.text))
        .collect::<HashMap<_, _>>();
    for chunk in chunks_by_graph_uid.values_mut() {
        chunk.context_window = neighbor_ordinals(chunk.ordinal)
            .into_iter()
            .filter_map(|neighbor_ordinal| {
                neighbor_texts
                    .get(&(chunk.document_version_uid, neighbor_ordinal))
                    .map(|text| KnowledgeChunkWindowPart {
                        ordinal: neighbor_ordinal,
                        text: text.clone(),
                    })
            })
            .collect();
    }
    Ok(())
}

/// Returns the ordinal-adjacent neighbor ordinals to hydrate for a matched
/// chunk, in ascending order and skipping negative ordinals.
fn neighbor_ordinals(ordinal: i32) -> Vec<i32> {
    [ordinal - 1, ordinal + 1]
        .into_iter()
        .filter(|neighbor_ordinal| *neighbor_ordinal >= 0)
        .collect()
}

#[derive(Debug, sqlx::FromRow)]
struct KnowledgeChunkRow {
    graph_node_uid: Uuid,
    chunk_uid: Uuid,
    document_version_uid: Uuid,
    object_uid: Uuid,
    chunk_hash: String,
    ordinal: i32,
    heading_path: Vec<String>,
    text: String,
    token_count: i32,
    metadata: Value,
    source_uri: Option<String>,
    source_title: Option<String>,
    object_type: String,
}

#[derive(Debug, sqlx::FromRow)]
struct KnowledgeChunkNeighborRow {
    document_version_uid: Uuid,
    ordinal: i32,
    text: String,
}
