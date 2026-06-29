-- Bump graph-retrieval cache versions when tenant knowledge visibility changes
-- outside graph node writes.

CREATE OR REPLACE FUNCTION moa.bump_storage_partition_state_from_knowledge_visibility() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    old_visible BOOLEAN;
    new_visible BOOLEAN;
BEGIN
    IF NEW.storage_partition_id IS NULL THEN
        RETURN NEW;
    END IF;

    IF TG_TABLE_NAME = 'knowledge_objects' THEN
        old_visible := OLD.status = 'active';
        new_visible := NEW.status = 'active';
    ELSIF TG_TABLE_NAME = 'knowledge_chunks' THEN
        old_visible := OLD.metadata->>'active' IS DISTINCT FROM 'false';
        new_visible := NEW.metadata->>'active' IS DISTINCT FROM 'false';
    ELSE
        RETURN NEW;
    END IF;

    IF old_visible IS DISTINCT FROM new_visible THEN
        INSERT INTO moa.storage_partition_state (storage_partition_id, changelog_version)
        VALUES (NEW.storage_partition_id, 1)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET changelog_version = moa.storage_partition_state.changelog_version + 1,
                updated_at = now();
    END IF;

    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS knowledge_objects_bump_cache_version ON moa.knowledge_objects;
CREATE TRIGGER knowledge_objects_bump_cache_version
    AFTER UPDATE OF status ON moa.knowledge_objects
    FOR EACH ROW
    EXECUTE FUNCTION moa.bump_storage_partition_state_from_knowledge_visibility();

DROP TRIGGER IF EXISTS knowledge_chunks_bump_cache_version ON moa.knowledge_chunks;
CREATE TRIGGER knowledge_chunks_bump_cache_version
    AFTER UPDATE OF metadata ON moa.knowledge_chunks
    FOR EACH ROW
    EXECUTE FUNCTION moa.bump_storage_partition_state_from_knowledge_visibility();
