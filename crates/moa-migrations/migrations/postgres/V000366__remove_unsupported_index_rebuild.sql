-- Remove the unsupported index-rebuild design introduced by V000351.
--
-- Production retrieval never routed query embeddings through the active
-- generation's model. Exposing a model-switch operation would therefore serve
-- new-model queries against old-model vectors while the candidate was built.
-- The safe contract needs provider routing that does not exist yet, so remove
-- the dead schema instead of preserving a misleading operator surface.

DROP TABLE IF EXISTS moa.knowledge_rechunk_staging;
DROP TABLE IF EXISTS moa.knowledge_rebuild_candidate_vector;
DROP TABLE IF EXISTS moa.knowledge_active_generation;
DROP TABLE IF EXISTS
    moa.knowledge_rebuild_operation,
    moa.knowledge_rebuild_generation
CASCADE;

DROP FUNCTION IF EXISTS moa.knowledge_rechunk_staged_members();

ALTER TABLE moa.storage_partition_state
    DROP COLUMN IF EXISTS reembed_state,
    ALTER COLUMN embedding_model DROP NOT NULL,
    ALTER COLUMN embedding_model DROP DEFAULT;

-- The old schema assigned `embed-v4.0` even when a partition held no vectors.
-- Keep the model unknown until the first real vector write pins it.
UPDATE moa.storage_partition_state AS state
SET embedding_model = NULL
WHERE NOT EXISTS (
    SELECT 1
    FROM moa.embeddings AS embedding
    WHERE embedding.storage_partition_id = state.storage_partition_id
);
