-- Close the remaining source-ACL privilege, identity, and cache-epoch gaps.
--
-- Every relationship added for ACL evidence includes the owning tenant and
-- storage partition, so a globally unique UUID cannot attach one tenant's ACL
-- row to another tenant's parent. The SECURITY DEFINER epoch helpers are
-- trigger internals, not application APIs, and therefore have no caller execute
-- privilege.

-- Connection-level mode and capture provenance have only one possible value,
-- so neither is data. Remove both columns.
DROP TRIGGER IF EXISTS source_acl_epoch ON moa.knowledge_connections;
ALTER TABLE moa.knowledge_connections
    DROP CONSTRAINT IF EXISTS knowledge_connections_acl_mode_valid,
    DROP COLUMN IF EXISTS acl_mode;
ALTER TABLE moa.knowledge_source_acl_snapshots
    DROP CONSTRAINT IF EXISTS knowledge_source_acl_snapshots_provenance_valid,
    DROP COLUMN IF EXISTS provenance;

-- Immutable snapshot and entry rows are staged in the same transaction before
-- the object pointer becomes current. Only that pointer/state transition changes
-- visibility, so per-row bumps add lock contention without improving freshness.
DROP TRIGGER IF EXISTS source_acl_epoch ON moa.knowledge_source_acl_snapshots;
DROP TRIGGER IF EXISTS source_acl_epoch ON moa.knowledge_source_acl_entries;

-- PostgreSQL grants EXECUTE on new functions to PUBLIC by default. These
-- SECURITY DEFINER functions are invoked only through table triggers.
REVOKE ALL ON FUNCTION moa.bump_source_acl_epoch(UUID) FROM PUBLIC;
REVOKE ALL ON FUNCTION moa.bump_source_acl_epoch(UUID) FROM moa_app;

-- A syntactic UPDATE that leaves the row unchanged must not invalidate every
-- warm retrieval cache for the tenant.
CREATE OR REPLACE FUNCTION moa.source_acl_epoch_trigger()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = moa, pg_catalog
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM moa.bump_source_acl_epoch(OLD.tenant_id);
        RETURN OLD;
    END IF;
    IF TG_OP = 'UPDATE' AND OLD IS NOT DISTINCT FROM NEW THEN
        RETURN NEW;
    END IF;
    PERFORM moa.bump_source_acl_epoch(NEW.tenant_id);
    IF TG_OP = 'UPDATE' AND OLD.tenant_id IS DISTINCT FROM NEW.tenant_id THEN
        PERFORM moa.bump_source_acl_epoch(OLD.tenant_id);
    END IF;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION moa.source_acl_epoch_trigger() FROM PUBLIC;
REVOKE ALL ON FUNCTION moa.source_acl_epoch_trigger() FROM moa_app;

-- Composite parent identities required by the tenant-bearing foreign keys.
CREATE UNIQUE INDEX IF NOT EXISTS knowledge_connections_tenant_partition_uid_uniq
    ON moa.knowledge_connections (tenant_id, storage_partition_id, connection_uid);
CREATE UNIQUE INDEX IF NOT EXISTS knowledge_objects_tenant_partition_uid_uniq
    ON moa.knowledge_objects (tenant_id, storage_partition_id, object_uid);
CREATE UNIQUE INDEX IF NOT EXISTS knowledge_source_acl_snapshots_tenant_partition_uid_uniq
    ON moa.knowledge_source_acl_snapshots (tenant_id, storage_partition_id, snapshot_uid);

-- Snapshot parents.
ALTER TABLE moa.knowledge_source_acl_snapshots
    DROP CONSTRAINT IF EXISTS knowledge_source_acl_snapshots_connection_id_fkey;
ALTER TABLE moa.knowledge_source_acl_snapshots
    DROP CONSTRAINT IF EXISTS knowledge_source_acl_snapshots_object_id_fkey;
ALTER TABLE moa.knowledge_source_acl_snapshots
    DROP CONSTRAINT IF EXISTS knowledge_source_acl_snapshots_connection_tenant_partition_fkey;
ALTER TABLE moa.knowledge_source_acl_snapshots
    ADD CONSTRAINT knowledge_source_acl_snapshots_connection_tenant_partition_fkey
    FOREIGN KEY (tenant_id, storage_partition_id, connection_id)
    REFERENCES moa.knowledge_connections (tenant_id, storage_partition_id, connection_uid);
ALTER TABLE moa.knowledge_source_acl_snapshots
    DROP CONSTRAINT IF EXISTS knowledge_source_acl_snapshots_object_tenant_partition_fkey;
ALTER TABLE moa.knowledge_source_acl_snapshots
    ADD CONSTRAINT knowledge_source_acl_snapshots_object_tenant_partition_fkey
    FOREIGN KEY (tenant_id, storage_partition_id, object_id)
    REFERENCES moa.knowledge_objects (tenant_id, storage_partition_id, object_uid);

-- Entry snapshot parent.
ALTER TABLE moa.knowledge_source_acl_entries
    DROP CONSTRAINT IF EXISTS knowledge_source_acl_entries_snapshot_id_fkey;
ALTER TABLE moa.knowledge_source_acl_entries
    DROP CONSTRAINT IF EXISTS knowledge_source_acl_entries_snapshot_tenant_partition_fkey;
ALTER TABLE moa.knowledge_source_acl_entries
    ADD CONSTRAINT knowledge_source_acl_entries_snapshot_tenant_partition_fkey
    FOREIGN KEY (tenant_id, storage_partition_id, snapshot_id)
    REFERENCES moa.knowledge_source_acl_snapshots
        (tenant_id, storage_partition_id, snapshot_uid);

-- Optional connection provenance on direct and group bindings.
ALTER TABLE moa.knowledge_source_principal_bindings
    DROP CONSTRAINT IF EXISTS knowledge_source_principal_bindings_connection_id_fkey;
ALTER TABLE moa.knowledge_source_principal_bindings
    DROP CONSTRAINT IF EXISTS knowledge_source_principal_bindings_connection_tenant_partition_fkey;
ALTER TABLE moa.knowledge_source_principal_bindings
    ADD CONSTRAINT knowledge_source_principal_bindings_connection_tenant_partition_fkey
    FOREIGN KEY (tenant_id, storage_partition_id, connection_id)
    REFERENCES moa.knowledge_connections (tenant_id, storage_partition_id, connection_uid);

ALTER TABLE moa.knowledge_source_principal_group_bindings
    DROP CONSTRAINT IF EXISTS knowledge_source_principal_group_bindings_connection_id_fkey;
ALTER TABLE moa.knowledge_source_principal_group_bindings
    DROP CONSTRAINT IF EXISTS knowledge_source_principal_group_bindings_connection_tenant_partition_fkey;
ALTER TABLE moa.knowledge_source_principal_group_bindings
    ADD CONSTRAINT knowledge_source_principal_group_bindings_connection_tenant_partition_fkey
    FOREIGN KEY (tenant_id, storage_partition_id, connection_id)
    REFERENCES moa.knowledge_connections (tenant_id, storage_partition_id, connection_uid);

-- A current snapshot cannot disappear behind the object's back. Purge and any
-- future deletion path must first move the object to an explicit incomplete
-- position and clear its revision and pointer.
ALTER TABLE moa.knowledge_objects
    DROP CONSTRAINT IF EXISTS knowledge_objects_current_acl_snapshot_id_fkey;
ALTER TABLE moa.knowledge_objects
    DROP CONSTRAINT IF EXISTS knowledge_objects_current_acl_snapshot_tenant_partition_fkey;
ALTER TABLE moa.knowledge_objects
    ADD CONSTRAINT knowledge_objects_current_acl_snapshot_tenant_partition_fkey
    FOREIGN KEY (tenant_id, storage_partition_id, current_acl_snapshot_id)
    REFERENCES moa.knowledge_source_acl_snapshots
        (tenant_id, storage_partition_id, snapshot_uid);
