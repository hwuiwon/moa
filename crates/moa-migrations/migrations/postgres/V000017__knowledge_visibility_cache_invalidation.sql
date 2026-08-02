-- Bump graph-retrieval cache versions when tenant knowledge visibility changes
-- outside graph node writes. Transition tables coalesce a bulk update into one
-- generation bump for each affected storage partition.

CREATE OR REPLACE FUNCTION moa.bump_storage_partition_state_from_knowledge_objects()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO moa.storage_partition_state (storage_partition_id, changelog_version)
    SELECT DISTINCT new_rows.storage_partition_id, 1
    FROM old_rows
    JOIN new_rows USING (object_uid)
    WHERE new_rows.storage_partition_id IS NOT NULL
      AND (old_rows.status = 'active') IS DISTINCT FROM (new_rows.status = 'active')
    ON CONFLICT (storage_partition_id) DO UPDATE
        SET changelog_version = moa.storage_partition_state.changelog_version + 1,
            updated_at = now();

    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION moa.bump_storage_partition_state_from_knowledge_chunks()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO moa.storage_partition_state (storage_partition_id, changelog_version)
    SELECT DISTINCT new_rows.storage_partition_id, 1
    FROM old_rows
    JOIN new_rows USING (chunk_uid)
    WHERE new_rows.storage_partition_id IS NOT NULL
      AND (old_rows.metadata->>'active' IS DISTINCT FROM 'false')
          IS DISTINCT FROM
          (new_rows.metadata->>'active' IS DISTINCT FROM 'false')
    ON CONFLICT (storage_partition_id) DO UPDATE
        SET changelog_version = moa.storage_partition_state.changelog_version + 1,
            updated_at = now();

    RETURN NULL;
END;
$$;

DROP TRIGGER IF EXISTS knowledge_objects_bump_cache_version ON moa.knowledge_objects;
CREATE TRIGGER knowledge_objects_bump_cache_version
    AFTER UPDATE ON moa.knowledge_objects
    REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
    FOR EACH STATEMENT
    EXECUTE FUNCTION moa.bump_storage_partition_state_from_knowledge_objects();

DROP TRIGGER IF EXISTS knowledge_chunks_bump_cache_version ON moa.knowledge_chunks;
CREATE TRIGGER knowledge_chunks_bump_cache_version
    AFTER UPDATE ON moa.knowledge_chunks
    REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
    FOR EACH STATEMENT
    EXECUTE FUNCTION moa.bump_storage_partition_state_from_knowledge_chunks();
