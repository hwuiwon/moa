-- Provider-native source/document ACL admission for tenant knowledge.
--
-- ACL evidence is born in its final tenant/partition-bound shape. Immutable
-- snapshots and entries do not bump cache epochs because visibility changes
-- only when an object's current ACL pointer changes.

CREATE TABLE IF NOT EXISTS moa.knowledge_source_acl_keys (
    tenant_id UUID NOT NULL,
    key_version INT NOT NULL,
    key_handle TEXT NOT NULL,
    wrapped_key BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, key_version),
    CONSTRAINT knowledge_source_acl_keys_version_range
        CHECK (key_version BETWEEN 1 AND 65535),
    CONSTRAINT knowledge_source_acl_keys_handle_present CHECK (key_handle <> ''),
    CONSTRAINT knowledge_source_acl_keys_material_present CHECK (octet_length(wrapped_key) > 0)
);

CREATE TABLE IF NOT EXISTS moa.knowledge_source_acl_epochs (
    tenant_id UUID PRIMARY KEY,
    epoch BIGINT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT knowledge_source_acl_epochs_non_negative CHECK (epoch >= 0)
);

CREATE OR REPLACE FUNCTION moa.bump_source_acl_epoch(target_tenant_id UUID)
RETURNS VOID
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    IF target_tenant_id IS NULL THEN
        RETURN;
    END IF;
    INSERT INTO moa.knowledge_source_acl_epochs AS epochs (tenant_id, epoch, updated_at)
    VALUES (target_tenant_id, 1, now())
    ON CONFLICT (tenant_id) DO UPDATE
        SET epoch = epochs.epoch + 1,
            updated_at = now();
END;
$$;

ALTER FUNCTION moa.bump_source_acl_epoch(UUID) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.bump_source_acl_epoch(UUID) FROM PUBLIC;
REVOKE ALL ON FUNCTION moa.bump_source_acl_epoch(UUID) FROM moa_app;

CREATE FUNCTION moa.source_acl_epoch_after_insert()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    affected_tenant_id UUID;
BEGIN
    FOR affected_tenant_id IN
        SELECT DISTINCT tenant_id
        FROM source_acl_new_rows
        WHERE tenant_id IS NOT NULL
    LOOP
        PERFORM moa.bump_source_acl_epoch(affected_tenant_id);
    END LOOP;
    RETURN NULL;
END;
$$;

ALTER FUNCTION moa.source_acl_epoch_after_insert() OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.source_acl_epoch_after_insert() FROM PUBLIC;
REVOKE ALL ON FUNCTION moa.source_acl_epoch_after_insert() FROM moa_app;

CREATE FUNCTION moa.source_acl_epoch_after_delete()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    affected_tenant_id UUID;
BEGIN
    FOR affected_tenant_id IN
        SELECT DISTINCT tenant_id
        FROM source_acl_old_rows
        WHERE tenant_id IS NOT NULL
    LOOP
        PERFORM moa.bump_source_acl_epoch(affected_tenant_id);
    END LOOP;
    RETURN NULL;
END;
$$;

ALTER FUNCTION moa.source_acl_epoch_after_delete() OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.source_acl_epoch_after_delete() FROM PUBLIC;
REVOKE ALL ON FUNCTION moa.source_acl_epoch_after_delete() FROM moa_app;

CREATE FUNCTION moa.source_acl_epoch_after_update()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    affected_tenant_id UUID;
BEGIN
    FOR affected_tenant_id IN
        WITH changed_rows AS (
            SELECT
                old_row.tenant_id AS old_tenant_id,
                new_row.tenant_id AS new_tenant_id
            FROM source_acl_old_rows AS old_row
            FULL JOIN source_acl_new_rows AS new_row USING (binding_uid)
            WHERE old_row IS DISTINCT FROM new_row
        ), affected_tenants AS (
            SELECT old_tenant_id AS tenant_id FROM changed_rows
            UNION
            SELECT new_tenant_id AS tenant_id FROM changed_rows
        )
        SELECT tenant_id
        FROM affected_tenants
        WHERE tenant_id IS NOT NULL
    LOOP
        PERFORM moa.bump_source_acl_epoch(affected_tenant_id);
    END LOOP;
    RETURN NULL;
END;
$$;

ALTER FUNCTION moa.source_acl_epoch_after_update() OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.source_acl_epoch_after_update() FROM PUBLIC;
REVOKE ALL ON FUNCTION moa.source_acl_epoch_after_update() FROM moa_app;

CREATE FUNCTION moa.source_acl_epoch_after_object_update()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    affected_tenant_id UUID;
BEGIN
    FOR affected_tenant_id IN
        WITH changed_rows AS (
            SELECT
                old_row.tenant_id AS old_tenant_id,
                new_row.tenant_id AS new_tenant_id
            FROM source_acl_old_rows AS old_row
            FULL JOIN source_acl_new_rows AS new_row USING (object_uid)
            WHERE old_row.object_uid IS NULL
               OR new_row.object_uid IS NULL
               OR ROW(
                    old_row.acl_state,
                    old_row.acl_revision,
                    old_row.current_acl_snapshot_id
               ) IS DISTINCT FROM ROW(
                    new_row.acl_state,
                    new_row.acl_revision,
                    new_row.current_acl_snapshot_id
               )
        ), affected_tenants AS (
            SELECT old_tenant_id AS tenant_id FROM changed_rows
            UNION
            SELECT new_tenant_id AS tenant_id FROM changed_rows
        )
        SELECT tenant_id
        FROM affected_tenants
        WHERE tenant_id IS NOT NULL
    LOOP
        PERFORM moa.bump_source_acl_epoch(affected_tenant_id);
    END LOOP;
    RETURN NULL;
END;
$$;

ALTER FUNCTION moa.source_acl_epoch_after_object_update() OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.source_acl_epoch_after_object_update() FROM PUBLIC;
REVOKE ALL ON FUNCTION moa.source_acl_epoch_after_object_update() FROM moa_app;

-- Composite parent identities make cross-tenant or cross-partition ACL links
-- impossible even when a globally unique UUID is supplied incorrectly.
CREATE UNIQUE INDEX IF NOT EXISTS knowledge_connections_tenant_partition_uid_uniq
    ON moa.knowledge_connections (tenant_id, storage_partition_id, connection_uid);
CREATE UNIQUE INDEX IF NOT EXISTS knowledge_objects_tenant_partition_uid_uniq
    ON moa.knowledge_objects (tenant_id, storage_partition_id, object_uid);

CREATE TABLE IF NOT EXISTS moa.knowledge_source_acl_snapshots (
    snapshot_uid UUID NOT NULL PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    connection_id UUID NOT NULL,
    object_id UUID NOT NULL,
    provider_revision TEXT NOT NULL,
    snapshot_hash TEXT NOT NULL,
    complete BOOLEAN NOT NULL,
    entry_count INT NOT NULL DEFAULT 0,
    captured_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT knowledge_source_acl_snapshots_connection_tenant_partition_fkey
        FOREIGN KEY (tenant_id, storage_partition_id, connection_id)
        REFERENCES moa.knowledge_connections
            (tenant_id, storage_partition_id, connection_uid),
    CONSTRAINT knowledge_source_acl_snapshots_object_tenant_partition_fkey
        FOREIGN KEY (tenant_id, storage_partition_id, object_id)
        REFERENCES moa.knowledge_objects
            (tenant_id, storage_partition_id, object_uid),
    CONSTRAINT knowledge_source_acl_snapshots_revision_present
        CHECK (provider_revision <> ''),
    CONSTRAINT knowledge_source_acl_snapshots_hash_present
        CHECK (snapshot_hash <> ''),
    CONSTRAINT knowledge_source_acl_snapshots_entry_count_non_negative
        CHECK (entry_count >= 0)
);

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_source_acl_snapshots_revision_uniq
    ON moa.knowledge_source_acl_snapshots
        (tenant_id, object_id, provider_revision, snapshot_hash);
CREATE UNIQUE INDEX IF NOT EXISTS knowledge_source_acl_snapshots_tenant_partition_uid_uniq
    ON moa.knowledge_source_acl_snapshots
        (tenant_id, storage_partition_id, snapshot_uid);
CREATE INDEX IF NOT EXISTS knowledge_source_acl_snapshots_object_captured_idx
    ON moa.knowledge_source_acl_snapshots (tenant_id, object_id, captured_at DESC);
CREATE INDEX IF NOT EXISTS knowledge_source_acl_snapshots_fk_connection_idx
    ON moa.knowledge_source_acl_snapshots (connection_id, captured_at DESC);

CREATE TABLE IF NOT EXISTS moa.knowledge_source_acl_entries (
    entry_uid UUID NOT NULL PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    snapshot_id UUID NOT NULL,
    entry_kind TEXT NOT NULL,
    principal_kind TEXT NOT NULL,
    principal_fingerprint BYTEA NOT NULL,
    fingerprint_key_version INT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT knowledge_source_acl_entries_snapshot_tenant_partition_fkey
        FOREIGN KEY (tenant_id, storage_partition_id, snapshot_id)
        REFERENCES moa.knowledge_source_acl_snapshots
            (tenant_id, storage_partition_id, snapshot_uid),
    CONSTRAINT knowledge_source_acl_entries_kind_valid
        CHECK (entry_kind IN ('allow', 'deny')),
    CONSTRAINT knowledge_source_acl_entries_principal_kind_valid
        CHECK (principal_kind IN ('user', 'group', 'domain', 'anyone')),
    CONSTRAINT knowledge_source_acl_entries_fingerprint_width
        CHECK (octet_length(principal_fingerprint) = 34),
    CONSTRAINT knowledge_source_acl_entries_key_version_range
        CHECK (fingerprint_key_version BETWEEN 1 AND 65535)
);

-- This unique index is also the admission lookup path; a duplicate non-unique
-- index on the same three columns only added write amplification.
CREATE UNIQUE INDEX IF NOT EXISTS knowledge_source_acl_entries_uniq
    ON moa.knowledge_source_acl_entries
        (snapshot_id, entry_kind, principal_fingerprint);

CREATE TABLE IF NOT EXISTS moa.knowledge_source_principal_bindings (
    binding_uid UUID NOT NULL PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    contact_id UUID NOT NULL,
    connection_id UUID,
    principal_kind TEXT NOT NULL,
    principal_fingerprint BYTEA NOT NULL,
    fingerprint_key_version INT NOT NULL,
    verified_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT knowledge_source_principal_bindings_connection_fkey
        FOREIGN KEY (tenant_id, storage_partition_id, connection_id)
        REFERENCES moa.knowledge_connections
            (tenant_id, storage_partition_id, connection_uid),
    CONSTRAINT knowledge_source_principal_bindings_kind_valid
        CHECK (principal_kind IN ('user', 'group', 'domain', 'anyone')),
    CONSTRAINT knowledge_source_principal_bindings_fingerprint_width
        CHECK (octet_length(principal_fingerprint) = 34),
    CONSTRAINT knowledge_source_principal_bindings_key_version_range
        CHECK (fingerprint_key_version BETWEEN 1 AND 65535)
);

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_source_principal_bindings_uniq
    ON moa.knowledge_source_principal_bindings (
        tenant_id,
        contact_id,
        principal_fingerprint,
        COALESCE(connection_id, '00000000-0000-0000-0000-000000000000'::UUID)
    );
CREATE INDEX IF NOT EXISTS knowledge_source_principal_bindings_fk_connection_idx
    ON moa.knowledge_source_principal_bindings (connection_id);

CREATE TABLE IF NOT EXISTS moa.knowledge_source_principal_group_bindings (
    binding_uid UUID NOT NULL PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    connection_id UUID,
    member_fingerprint BYTEA NOT NULL,
    group_kind TEXT NOT NULL,
    group_fingerprint BYTEA NOT NULL,
    fingerprint_key_version INT NOT NULL,
    verified_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT knowledge_source_group_bindings_connection_fkey
        FOREIGN KEY (tenant_id, storage_partition_id, connection_id)
        REFERENCES moa.knowledge_connections
            (tenant_id, storage_partition_id, connection_uid),
    CONSTRAINT knowledge_source_principal_group_bindings_kind_valid
        CHECK (group_kind IN ('group', 'domain')),
    CONSTRAINT knowledge_source_principal_group_bindings_member_width
        CHECK (octet_length(member_fingerprint) = 34),
    CONSTRAINT knowledge_source_principal_group_bindings_group_width
        CHECK (octet_length(group_fingerprint) = 34),
    CONSTRAINT knowledge_source_principal_group_bindings_distinct
        CHECK (member_fingerprint <> group_fingerprint),
    CONSTRAINT knowledge_source_principal_group_bindings_key_version_range
        CHECK (fingerprint_key_version BETWEEN 1 AND 65535)
);

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_source_principal_group_bindings_uniq
    ON moa.knowledge_source_principal_group_bindings (
        tenant_id,
        member_fingerprint,
        group_fingerprint,
        COALESCE(connection_id, '00000000-0000-0000-0000-000000000000'::UUID)
    );
CREATE INDEX IF NOT EXISTS knowledge_source_principal_group_bindings_fk_connection_idx
    ON moa.knowledge_source_principal_group_bindings (connection_id);

ALTER TABLE moa.knowledge_objects
    ADD CONSTRAINT knowledge_objects_current_acl_snapshot_tenant_partition_fkey
    FOREIGN KEY (tenant_id, storage_partition_id, current_acl_snapshot_id)
    REFERENCES moa.knowledge_source_acl_snapshots
        (tenant_id, storage_partition_id, snapshot_uid);

CREATE TRIGGER source_acl_epoch_insert
    AFTER INSERT ON moa.knowledge_source_principal_bindings
    REFERENCING NEW TABLE AS source_acl_new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION moa.source_acl_epoch_after_insert();
CREATE TRIGGER source_acl_epoch_update
    AFTER UPDATE ON moa.knowledge_source_principal_bindings
    REFERENCING OLD TABLE AS source_acl_old_rows NEW TABLE AS source_acl_new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION moa.source_acl_epoch_after_update();
CREATE TRIGGER source_acl_epoch_delete
    AFTER DELETE ON moa.knowledge_source_principal_bindings
    REFERENCING OLD TABLE AS source_acl_old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION moa.source_acl_epoch_after_delete();

CREATE TRIGGER source_acl_epoch_insert
    AFTER INSERT ON moa.knowledge_source_principal_group_bindings
    REFERENCING NEW TABLE AS source_acl_new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION moa.source_acl_epoch_after_insert();
CREATE TRIGGER source_acl_epoch_update
    AFTER UPDATE ON moa.knowledge_source_principal_group_bindings
    REFERENCING OLD TABLE AS source_acl_old_rows NEW TABLE AS source_acl_new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION moa.source_acl_epoch_after_update();
CREATE TRIGGER source_acl_epoch_delete
    AFTER DELETE ON moa.knowledge_source_principal_group_bindings
    REFERENCING OLD TABLE AS source_acl_old_rows
    FOR EACH STATEMENT EXECUTE FUNCTION moa.source_acl_epoch_after_delete();

CREATE TRIGGER source_acl_epoch_update
    AFTER UPDATE ON moa.knowledge_objects
    REFERENCING OLD TABLE AS source_acl_old_rows NEW TABLE AS source_acl_new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION moa.source_acl_epoch_after_object_update();

SELECT moa.apply_tenant_rls('moa.knowledge_source_acl_epochs'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.knowledge_source_principal_bindings'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.knowledge_source_principal_group_bindings'::REGCLASS);

ALTER TABLE moa.knowledge_source_acl_keys ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.knowledge_source_acl_keys FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON moa.knowledge_source_acl_keys FOR ALL TO moa_app
    USING (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID)
    WITH CHECK (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID);
GRANT SELECT, INSERT, DELETE ON moa.knowledge_source_acl_keys TO moa_app;

-- Snapshot/entry evidence is append-only. DELETE remains available for bounded
-- retention and tenant purge; UPDATE is denied by both policy and grant.
ALTER TABLE moa.knowledge_source_acl_snapshots ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.knowledge_source_acl_snapshots FORCE ROW LEVEL SECURITY;
CREATE POLICY rd_tenant ON moa.knowledge_source_acl_snapshots FOR SELECT TO moa_app
    USING (moa.current_control_plane() OR tenant_id = moa.current_tenant_id());
CREATE POLICY wr_tenant ON moa.knowledge_source_acl_snapshots FOR INSERT TO moa_app
    WITH CHECK (moa.current_control_plane() OR tenant_id = moa.current_tenant_id());
CREATE POLICY rm_tenant ON moa.knowledge_source_acl_snapshots FOR DELETE TO moa_app
    USING (moa.current_control_plane() OR tenant_id = moa.current_tenant_id());
GRANT SELECT, INSERT, DELETE ON moa.knowledge_source_acl_snapshots TO moa_app;

ALTER TABLE moa.knowledge_source_acl_entries ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.knowledge_source_acl_entries FORCE ROW LEVEL SECURITY;
CREATE POLICY rd_tenant ON moa.knowledge_source_acl_entries FOR SELECT TO moa_app
    USING (moa.current_control_plane() OR tenant_id = moa.current_tenant_id());
CREATE POLICY wr_tenant ON moa.knowledge_source_acl_entries FOR INSERT TO moa_app
    WITH CHECK (moa.current_control_plane() OR tenant_id = moa.current_tenant_id());
CREATE POLICY rm_tenant ON moa.knowledge_source_acl_entries FOR DELETE TO moa_app
    USING (moa.current_control_plane() OR tenant_id = moa.current_tenant_id());
GRANT SELECT, INSERT, DELETE ON moa.knowledge_source_acl_entries TO moa_app;
