-- Separate graph occurrence identity from content identity for knowledge chunks.
--
-- Before this migration a knowledge chunk's graph node uid was derived from
-- `(tenant_id, chunk_hash)`. Two documents containing the same paragraph — or
-- two versions of one document — therefore collapsed onto ONE graph node, ONE
-- embedding, and ONE citation target, so retrieval could cite the wrong source
-- and deleting one document invalidated another document's content.
--
-- After this migration the invariant is exactly one: a chunk row IS its graph
-- occurrence. `graph_node_uid` is NOT NULL and equal to `chunk_uid`, which is
-- already derived from (document version, ordinal, content seed). Content hashes
-- remain in `chunk_hash` and in node properties for dedupe and diffing; they
-- never form identity again. There is no nullable, aliased, or derivative graph
-- identity left to read or write, and no compatibility reader.
--
-- Order matters and is load bearing:
--   1. refuse to guess about sealed chunk nodes;
--   2. create one occurrence node per chunk row (cloned from the shared node
--      when there is one, synthesized from the chunk row otherwise), preserving
--      tenant, storage partition, and active/tombstoned state;
--   3. clone the occurrence-specific containment, provenance, semantic, and
--      evidence edges and rewire their chunk endpoints (entity and fact nodes
--      stay shared);
--   4. clone each current embedding beneath its occurrence uid, preserving
--      model, version, validity, tenant, and storage partition;
--   5. queue external-vector upserts for the new occurrence uids and deletions
--      for the retired shared uids;
--   6. only then rewrite `graph_node_uid`, install NOT NULL plus the equality
--      constraint and the one-occurrence unique index;
--   7. only then retire the content-hash chunk nodes.
--
-- Every step is guarded so a replay applies nothing: the clone steps are keyed
-- on `chunk_uid <> graph_node_uid`, which step 6 makes false forever.

-- 1. A restricted/PHI graph node carries one ciphertext bound to
-- `(tenant_id, data_subject_id, uid, pii_class)`. Cloning that payload under a
-- new occurrence uid would produce a row nothing can decrypt, and synthesizing a
-- replacement would silently downgrade the classification. Tenant knowledge
-- writes chunk nodes as `pii_class = 'none'`, so this is unreachable in
-- practice; if it is ever reached, the migration refuses rather than guessing.
DO $$
DECLARE
    sealed_count BIGINT;
BEGIN
    SELECT count(*)
      INTO sealed_count
      FROM moa.knowledge_chunks AS chunk
      JOIN moa.node_index AS shared
        ON shared.uid = chunk.graph_node_uid
     WHERE shared.label = 'Chunk'
       AND shared.uid <> chunk.chunk_uid
       AND shared.content_sealed IS NOT NULL;
    IF sealed_count > 0 THEN
        RAISE EXCEPTION
            'cannot split % sealed knowledge chunk node(s) into occurrences: sealed content is bound to its node uid',
            sealed_count
            USING ERRCODE = '55000';
    END IF;
END
$$;

-- 2a. One occurrence node per chunk that already references a shared node,
-- cloned so tenant, storage partition, barrier, classification, and confidence
-- are preserved exactly. A chunk that this object already tombstoned becomes an
-- invalidated occurrence even when the shared node is still alive for another
-- document.
INSERT INTO moa.node_index (
    uid, label, storage_partition_id, user_id, tenant_id, contact_id, name,
    pii_class, barrier, confidence, base_confidence, reference_count, created_at,
    valid_from, valid_to, invalidated_at, invalidated_by, invalidated_reason,
    last_accessed_at, properties_summary, data_subject_id, content_sealed
)
SELECT chunk.chunk_uid,
       shared.label,
       shared.storage_partition_id,
       shared.user_id,
       shared.tenant_id,
       shared.contact_id,
       shared.name,
       shared.pii_class,
       shared.barrier,
       shared.confidence,
       shared.base_confidence,
       shared.reference_count,
       shared.created_at,
       shared.valid_from,
       CASE
           WHEN COALESCE(chunk.metadata->>'active', 'true') = 'false'
               THEN COALESCE(shared.valid_to, now())
           ELSE shared.valid_to
       END,
       CASE
           WHEN COALESCE(chunk.metadata->>'active', 'true') = 'false'
               THEN COALESCE(shared.invalidated_at, now())
           ELSE shared.invalidated_at
       END,
       shared.invalidated_by,
       CASE
           WHEN COALESCE(chunk.metadata->>'active', 'true') = 'false'
               THEN COALESCE(shared.invalidated_reason, 'knowledge_chunk_orphaned')
           ELSE shared.invalidated_reason
       END,
       shared.last_accessed_at,
       shared.properties_summary,
       shared.data_subject_id,
       shared.content_sealed
  FROM moa.knowledge_chunks AS chunk
  JOIN moa.node_index AS shared
    ON shared.uid = chunk.graph_node_uid
 WHERE shared.label = 'Chunk'
   AND shared.uid <> chunk.chunk_uid
 ORDER BY chunk.chunk_uid
ON CONFLICT (uid) DO NOTHING;

-- 2b. Chunks that never reached the graph (NULL reference) or whose reference
-- dangles still become occurrences, so the NOT NULL equality invariant below
-- describes every row. These carry no embedding: nothing was ever computed for
-- them, and re-ingestion or a rebuild is what gives them a vector.
INSERT INTO moa.node_index (
    uid, label, storage_partition_id, tenant_id, name, pii_class, confidence,
    reference_count, created_at, valid_from, valid_to, invalidated_at,
    invalidated_reason, properties_summary, data_subject_id
)
SELECT chunk.chunk_uid,
       'Chunk',
       chunk.storage_partition_id,
       chunk.tenant_id,
       chunk.chunk_hash,
       'none',
       0.95,
       0,
       chunk.created_at,
       chunk.created_at,
       CASE WHEN COALESCE(chunk.metadata->>'active', 'true') = 'false' THEN now() END,
       CASE WHEN COALESCE(chunk.metadata->>'active', 'true') = 'false' THEN now() END,
       CASE
           WHEN COALESCE(chunk.metadata->>'active', 'true') = 'false'
               THEN 'knowledge_chunk_orphaned'
       END,
       jsonb_build_object(
           'chunk_hash', chunk.chunk_hash,
           'version_uid', chunk.document_version_id,
           'ordinal', chunk.ordinal,
           'token_count', chunk.token_count,
           'heading_path', to_jsonb(chunk.heading_path)
       ),
       chunk.tenant_id
  FROM moa.knowledge_chunks AS chunk
 WHERE NOT EXISTS (
           SELECT 1 FROM moa.node_index AS occurrence
            WHERE occurrence.uid = chunk.chunk_uid
       )
 ORDER BY chunk.chunk_uid
ON CONFLICT (uid) DO NOTHING;

-- 3a. Containment and any other edge that points AT a shared chunk node
-- (`Document -CONTAINS-> Chunk`) is occurrence-specific: it says which document
-- version holds this text. Rewire the chunk endpoint to the occurrence and leave
-- the other endpoint shared.
INSERT INTO moa.edge_index (
    uid, label, start_uid, end_uid, storage_partition_id, user_id, tenant_id,
    contact_id, properties, created_at, valid_from, valid_to
)
SELECT md5(edge.uid::TEXT || ':' || edge.start_uid::TEXT || ':' || chunk.chunk_uid::TEXT)::UUID,
       edge.label,
       edge.start_uid,
       chunk.chunk_uid,
       edge.storage_partition_id,
       edge.user_id,
       edge.tenant_id,
       edge.contact_id,
       edge.properties,
       edge.created_at,
       edge.valid_from,
       edge.valid_to
  FROM moa.knowledge_chunks AS chunk
  JOIN moa.node_index AS shared
    ON shared.uid = chunk.graph_node_uid
   AND shared.label = 'Chunk'
   AND shared.uid <> chunk.chunk_uid
  JOIN moa.edge_index AS edge
    ON edge.end_uid = shared.uid
 WHERE NOT EXISTS (
           SELECT 1 FROM moa.node_index AS other
            WHERE other.uid = edge.start_uid AND other.label = 'Chunk'
       )
 ORDER BY 1
ON CONFLICT (uid) DO NOTHING;

-- 3b. Provenance, semantic-mention, and evidence edges leaving a shared chunk
-- node (`Chunk -MENTIONED_IN-> Entity`, `Chunk -DERIVED_FROM-> Fact`) belong to
-- the occurrence that produced the evidence; the entity and fact nodes stay
-- shared on purpose.
INSERT INTO moa.edge_index (
    uid, label, start_uid, end_uid, storage_partition_id, user_id, tenant_id,
    contact_id, properties, created_at, valid_from, valid_to
)
SELECT md5(edge.uid::TEXT || ':' || chunk.chunk_uid::TEXT || ':' || edge.end_uid::TEXT)::UUID,
       edge.label,
       chunk.chunk_uid,
       edge.end_uid,
       edge.storage_partition_id,
       edge.user_id,
       edge.tenant_id,
       edge.contact_id,
       edge.properties,
       edge.created_at,
       edge.valid_from,
       edge.valid_to
  FROM moa.knowledge_chunks AS chunk
  JOIN moa.node_index AS shared
    ON shared.uid = chunk.graph_node_uid
   AND shared.label = 'Chunk'
   AND shared.uid <> chunk.chunk_uid
  JOIN moa.edge_index AS edge
    ON edge.start_uid = shared.uid
 WHERE NOT EXISTS (
           SELECT 1 FROM moa.node_index AS other
            WHERE other.uid = edge.end_uid AND other.label = 'Chunk'
       )
 ORDER BY 1
ON CONFLICT (uid) DO NOTHING;

-- 3c. Semantic chunk-to-chunk links are emitted only within one document, so
-- both endpoints are rewired and the clone is restricted to occurrence pairs
-- that share a document version. Without that restriction one shared link would
-- fan out across every document that happened to contain the same text.
INSERT INTO moa.edge_index (
    uid, label, start_uid, end_uid, storage_partition_id, user_id, tenant_id,
    contact_id, properties, created_at, valid_from, valid_to
)
SELECT md5(edge.uid::TEXT || ':' || start_chunk.chunk_uid::TEXT || ':' || end_chunk.chunk_uid::TEXT)::UUID,
       edge.label,
       start_chunk.chunk_uid,
       end_chunk.chunk_uid,
       edge.storage_partition_id,
       edge.user_id,
       edge.tenant_id,
       edge.contact_id,
       edge.properties,
       edge.created_at,
       edge.valid_from,
       edge.valid_to
  FROM moa.edge_index AS edge
  JOIN moa.knowledge_chunks AS start_chunk
    ON start_chunk.graph_node_uid = edge.start_uid
   AND start_chunk.chunk_uid <> edge.start_uid
  JOIN moa.knowledge_chunks AS end_chunk
    ON end_chunk.graph_node_uid = edge.end_uid
   AND end_chunk.chunk_uid <> edge.end_uid
   AND end_chunk.document_version_id = start_chunk.document_version_id
 WHERE EXISTS (
           SELECT 1 FROM moa.node_index AS shared_start
            WHERE shared_start.uid = edge.start_uid AND shared_start.label = 'Chunk'
       )
   AND EXISTS (
           SELECT 1 FROM moa.node_index AS shared_end
            WHERE shared_end.uid = edge.end_uid AND shared_end.label = 'Chunk'
       )
 ORDER BY 1
ON CONFLICT (uid) DO NOTHING;

-- 4. Clone the current embedding beneath every ACTIVE occurrence uid. The
-- unique index is `(storage_partition_id, uid)`, so this is one vector per
-- occurrence per partition, with model, version, validity, tenant, and partition
-- preserved. Tombstoned occurrences deliberately get no row: runtime
-- invalidation deletes the vector, and cloning a live vector onto dead text
-- would make it retrievable with nothing to hydrate.
INSERT INTO moa.embeddings (
    uid, storage_partition_id, user_id, tenant_id, contact_id, label, pii_class,
    embedding, embedding_model, embedding_model_version, valid_to, created_at
)
SELECT chunk.chunk_uid,
       shared_embedding.storage_partition_id,
       shared_embedding.user_id,
       shared_embedding.tenant_id,
       shared_embedding.contact_id,
       shared_embedding.label,
       shared_embedding.pii_class,
       shared_embedding.embedding,
       shared_embedding.embedding_model,
       shared_embedding.embedding_model_version,
       shared_embedding.valid_to,
       shared_embedding.created_at
  FROM moa.knowledge_chunks AS chunk
  JOIN moa.node_index AS shared
    ON shared.uid = chunk.graph_node_uid
   AND shared.label = 'Chunk'
   AND shared.uid <> chunk.chunk_uid
  JOIN moa.embeddings AS shared_embedding
    ON shared_embedding.uid = shared.uid
   AND shared_embedding.storage_partition_id = chunk.storage_partition_id
   AND shared_embedding.valid_to IS NULL
 WHERE COALESCE(chunk.metadata->>'active', 'true') <> 'false'
 ORDER BY chunk.chunk_uid
ON CONFLICT (storage_partition_id, uid) DO NOTHING;

-- 5a. External vector backends index by node uid, so every new occurrence uid
-- that now owns a vector needs an upsert. The `vector_backend <> 'pgvector'`
-- predicate matches the runtime enqueue exactly: pgvector-only partitions have
-- no external index to reconcile. The NOT EXISTS keeps a replay from re-queuing
-- work that is still pending.
INSERT INTO moa.vector_sync_outbox (storage_partition_id, uid, op)
SELECT chunk.storage_partition_id, chunk.chunk_uid, 'upsert'
  FROM moa.knowledge_chunks AS chunk
  JOIN moa.node_index AS shared
    ON shared.uid = chunk.graph_node_uid
   AND shared.label = 'Chunk'
   AND shared.uid <> chunk.chunk_uid
  JOIN moa.embeddings AS occurrence_embedding
    ON occurrence_embedding.uid = chunk.chunk_uid
   AND occurrence_embedding.storage_partition_id = chunk.storage_partition_id
 WHERE EXISTS (
           SELECT 1 FROM moa.storage_partition_state AS state
            WHERE state.storage_partition_id = chunk.storage_partition_id
              AND state.vector_backend <> 'pgvector'
       )
   AND NOT EXISTS (
           SELECT 1 FROM moa.vector_sync_outbox AS queued
            WHERE queued.storage_partition_id = chunk.storage_partition_id
              AND queued.uid = chunk.chunk_uid
              AND queued.op = 'upsert'
              AND queued.processed_at IS NULL
       )
 ORDER BY chunk.chunk_uid;

-- 5b. The retired shared uids must disappear from the external index too, or a
-- deleted document keeps answering queries from the vector backend.
INSERT INTO moa.vector_sync_outbox (storage_partition_id, uid, op)
SELECT shared.storage_partition_id, shared.uid, 'delete'
  FROM moa.node_index AS shared
 WHERE shared.label = 'Chunk'
   AND shared.storage_partition_id IS NOT NULL
   AND NOT EXISTS (
           SELECT 1 FROM moa.knowledge_chunks AS chunk
            WHERE chunk.chunk_uid = shared.uid
       )
   AND EXISTS (
           SELECT 1 FROM moa.storage_partition_state AS state
            WHERE state.storage_partition_id = shared.storage_partition_id
              AND state.vector_backend <> 'pgvector'
       )
   AND NOT EXISTS (
           SELECT 1 FROM moa.vector_sync_outbox AS queued
            WHERE queued.storage_partition_id = shared.storage_partition_id
              AND queued.uid = shared.uid
              AND queued.op = 'delete'
              AND queued.processed_at IS NULL
       )
 ORDER BY shared.uid;

-- 6. The single post-change contract: a chunk row is its graph occurrence.
UPDATE moa.knowledge_chunks
   SET graph_node_uid = chunk_uid,
       updated_at = now()
 WHERE graph_node_uid IS DISTINCT FROM chunk_uid;

ALTER TABLE moa.knowledge_chunks
    ALTER COLUMN graph_node_uid SET NOT NULL;

ALTER TABLE moa.knowledge_chunks
    DROP CONSTRAINT IF EXISTS knowledge_chunks_graph_node_is_occurrence;
ALTER TABLE moa.knowledge_chunks
    ADD CONSTRAINT knowledge_chunks_graph_node_is_occurrence
    CHECK (graph_node_uid = chunk_uid);

COMMENT ON COLUMN moa.knowledge_chunks.graph_node_uid IS
    'Graph occurrence uid for this chunk. Always equal to chunk_uid (database enforced): one chunk row is one graph node, one embedding, and one citation target. Content identity lives in chunk_hash and never forms graph identity.';

-- Content identity must not constrain occurrences. `knowledge_chunks_hash_uniq`
-- made a chunk's content hash unique per document version, so a document that
-- repeats a paragraph (a boilerplate line, "N/A", a repeated table row) could not
-- store its second occurrence at all: ingestion failed with a duplicate key
-- instead of storing two occurrences with their own ordinals, neighbors, and
-- citations. Occurrence uniqueness is now owned by the primary key and by
-- `knowledge_chunks_graph_node_occurrence_uniq` below; the hash keeps only its
-- lookup and diffing role.
DROP INDEX IF EXISTS moa.knowledge_chunks_hash_uniq;
CREATE INDEX IF NOT EXISTS knowledge_chunks_version_hash_idx
    ON moa.knowledge_chunks (tenant_id, document_version_id, chunk_hash);

-- The partial predicates described a nullable column that no longer exists, and
-- the `(graph_node_uid, created_at DESC)` index existed only to break newest-
-- version ties for a shared uid. A unique index replaces both: it is the storage
-- proof that one graph uid hydrates exactly one document-version occurrence.
DROP INDEX IF EXISTS moa.knowledge_chunks_graph_node_idx;
DROP INDEX IF EXISTS moa.knowledge_chunks_graph_node_active_idx;
CREATE UNIQUE INDEX IF NOT EXISTS knowledge_chunks_graph_node_occurrence_uniq
    ON moa.knowledge_chunks (graph_node_uid);
CREATE INDEX IF NOT EXISTS knowledge_chunks_tenant_graph_node_idx
    ON moa.knowledge_chunks (tenant_id, graph_node_uid);

-- 7. Retire the content-hash chunk nodes now that every reference, edge,
-- embedding, and outbox row exists. Their edges and embeddings follow through
-- ON DELETE CASCADE. A Chunk node that no chunk row claims can no longer be
-- hydrated or cited, so leaving it would only produce retrieval hits with no
-- content.
DELETE FROM moa.node_index AS shared
 WHERE shared.label = 'Chunk'
   AND NOT EXISTS (
           SELECT 1 FROM moa.knowledge_chunks AS chunk
            WHERE chunk.chunk_uid = shared.uid
       );
