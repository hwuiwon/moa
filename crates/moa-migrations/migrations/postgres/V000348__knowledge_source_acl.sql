-- Provider-native source/document ACL admission for tenant knowledge.
--
-- Tenant isolation was the only boundary around synced knowledge: every member
-- of a tenant could retrieve every chunk the tenant had ever ingested. That is
-- correct for a connector whose content is uniformly readable, and a disclosure
-- for every permission-bearing connector — a Google Drive folder shared with
-- three people, a Merge knowledge base with per-space permissions — because the
-- source system's own decision was discarded at ingestion.
--
-- This migration reproduces that decision durably:
--
--   * A connection is `tenant_public` or `provider_managed`. The mode follows
--     the adapter's declared capability; an operator cannot pick the weaker one.
--   * A `provider_managed` object is admitted only through an immutable
--     `moa.knowledge_source_acl_snapshots` row that is COMPLETE and whose
--     provider revision equals the object's recorded revision, with at least one
--     matching allow entry and no matching deny entry. Missing snapshot,
--     incomplete capture, stale state, or revision drift all deny.
--   * Principals are stored only as keyed opaque fingerprints. No email, phone
--     number, or provider label enters any row here.
--   * Every snapshot, binding, and object-state change bumps the tenant's
--     source-ACL epoch, which is part of retrieval cache identity. An ACL-only
--     change therefore flips visibility without re-parsing or re-embedding.
--
-- Backfill is deliberately closed: MOA's shipped adapters (Nango, Merge) are
-- both permission-bearing, so there is no known uniformly-public provider to
-- promote. Every existing connection becomes `provider_managed` and every
-- existing object becomes `incomplete`, which hides all previously ingested
-- content until a resync captures real provider ACLs. Nothing is guessed; no
-- reader that predates this migration can see provider-managed content.

-- ---------------------------------------------------------------------------
-- Versioned fingerprint keys
-- ---------------------------------------------------------------------------

-- One KMS-wrapped MAC key per tenant per version. The plaintext never lives
-- here; `moa-knowledge` unwraps it through the deployment KMS. Rotation inserts
-- a new version, and because the version is encoded into every fingerprint,
-- entries minted under a retired key stop matching instead of matching wrongly.
CREATE TABLE IF NOT EXISTS moa.knowledge_source_acl_keys (
    tenant_id    UUID        NOT NULL,
    key_version  INT         NOT NULL,
    key_handle   TEXT        NOT NULL,
    wrapped_key  BYTEA       NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, key_version),
    CONSTRAINT knowledge_source_acl_keys_version_range
        CHECK (key_version BETWEEN 1 AND 65535),
    CONSTRAINT knowledge_source_acl_keys_handle_present
        CHECK (key_handle <> ''),
    CONSTRAINT knowledge_source_acl_keys_material_present
        CHECK (octet_length(wrapped_key) > 0)
);

-- ---------------------------------------------------------------------------
-- Tenant source-ACL epoch
-- ---------------------------------------------------------------------------

-- Monotonic per-tenant counter bumped by every ACL-affecting write. Retrieval
-- pins it into result- and runtime-cache identity, so a permission revocation
-- invalidates warm caches without any explicit cache plumbing.
CREATE TABLE IF NOT EXISTS moa.knowledge_source_acl_epochs (
    tenant_id  UUID        NOT NULL PRIMARY KEY,
    epoch      BIGINT      NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT knowledge_source_acl_epochs_non_negative CHECK (epoch >= 0)
);

-- SECURITY DEFINER so an epoch bump cannot be skipped by a writer whose
-- transaction-local GUCs do not satisfy the epoch table's own policy (a
-- control-plane purge, for example). The search_path is pinned so the definer's
-- rights cannot be redirected to a caller-controlled schema.
CREATE OR REPLACE FUNCTION moa.bump_source_acl_epoch(target_tenant_id UUID)
RETURNS VOID
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = moa, pg_catalog
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

-- Row trigger used by every ACL-bearing table. The epoch row is touched after
-- the table's own row lock is already held, so concurrent writers form a queue
-- on one row rather than a lock cycle.
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
    PERFORM moa.bump_source_acl_epoch(NEW.tenant_id);
    IF TG_OP = 'UPDATE' AND OLD.tenant_id IS DISTINCT FROM NEW.tenant_id THEN
        PERFORM moa.bump_source_acl_epoch(OLD.tenant_id);
    END IF;
    RETURN NEW;
END;
$$;

-- ---------------------------------------------------------------------------
-- Immutable provider ACL snapshots
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS moa.knowledge_source_acl_snapshots (
    snapshot_uid         UUID        NOT NULL PRIMARY KEY,
    tenant_id            UUID        NOT NULL,
    storage_partition_id TEXT        NOT NULL,
    -- No `ON DELETE CASCADE` on either parent. A cascade would remove a
    -- tenant's captured ACL rows as a side effect of removing its connections
    -- or objects, which makes the purge's explicit ACL deletes unfalsifiable:
    -- neutering one changes nothing observable, because the cascade removes the
    -- same rows moments later in the same transaction. Nothing in production
    -- deletes a connection or an object -- disconnect disables, and ingestion
    -- tombstones -- so the cascade only ever fired for tenant purge, which
    -- already deletes these rows explicitly and in dependency order.
    connection_id        UUID        NOT NULL
        REFERENCES moa.knowledge_connections(connection_uid),
    object_id            UUID        NOT NULL
        REFERENCES moa.knowledge_objects(object_uid),
    provider_revision    TEXT        NOT NULL,
    snapshot_hash        TEXT        NOT NULL,
    provenance           TEXT        NOT NULL,
    complete             BOOLEAN     NOT NULL,
    entry_count          INT         NOT NULL DEFAULT 0,
    captured_at          TIMESTAMPTZ NOT NULL,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT knowledge_source_acl_snapshots_revision_present
        CHECK (provider_revision <> ''),
    CONSTRAINT knowledge_source_acl_snapshots_hash_present
        CHECK (snapshot_hash <> ''),
    CONSTRAINT knowledge_source_acl_snapshots_provenance_valid
        CHECK (provenance IN ('provider_listing', 'provider_change_notification')),
    CONSTRAINT knowledge_source_acl_snapshots_entry_count_non_negative
        CHECK (entry_count >= 0)
);

-- One snapshot per (object, revision, hash): re-capturing identical permissions
-- is idempotent, while a genuinely different entry set under the same revision
-- is a distinct row rather than a silent overwrite.
CREATE UNIQUE INDEX IF NOT EXISTS knowledge_source_acl_snapshots_revision_uniq
    ON moa.knowledge_source_acl_snapshots (tenant_id, object_id, provider_revision, snapshot_hash);

CREATE INDEX IF NOT EXISTS knowledge_source_acl_snapshots_object_captured_idx
    ON moa.knowledge_source_acl_snapshots (tenant_id, object_id, captured_at DESC);

CREATE INDEX IF NOT EXISTS knowledge_source_acl_snapshots_fk_connection_idx
    ON moa.knowledge_source_acl_snapshots (connection_id, captured_at DESC);

CREATE TABLE IF NOT EXISTS moa.knowledge_source_acl_entries (
    entry_uid               UUID   NOT NULL PRIMARY KEY,
    tenant_id               UUID   NOT NULL,
    storage_partition_id    TEXT   NOT NULL,
    -- No cascade, same reasoning as the snapshot table's own parents. Entries
    -- are deleted explicitly before snapshots; without the cascade that
    -- ordering is enforced by the foreign key instead of assumed.
    snapshot_id             UUID   NOT NULL
        REFERENCES moa.knowledge_source_acl_snapshots(snapshot_uid),
    entry_kind              TEXT   NOT NULL,
    principal_kind          TEXT   NOT NULL,
    principal_fingerprint   BYTEA  NOT NULL,
    fingerprint_key_version INT    NOT NULL,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT knowledge_source_acl_entries_kind_valid
        CHECK (entry_kind IN ('allow', 'deny')),
    CONSTRAINT knowledge_source_acl_entries_principal_kind_valid
        CHECK (principal_kind IN ('user', 'group', 'domain', 'anyone')),
    -- Two big-endian key-version bytes plus a 32-byte HMAC-SHA256 digest. A row
    -- of any other width is not a fingerprint this deployment can have minted.
    CONSTRAINT knowledge_source_acl_entries_fingerprint_width
        CHECK (octet_length(principal_fingerprint) = 34),
    CONSTRAINT knowledge_source_acl_entries_key_version_range
        CHECK (fingerprint_key_version BETWEEN 1 AND 65535)
);

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_source_acl_entries_uniq
    ON moa.knowledge_source_acl_entries (snapshot_id, entry_kind, principal_fingerprint);

-- The admission predicate probes (snapshot, kind, fingerprint) for both the
-- allow existence check and the deny anti-join, so both are index-only lookups.
CREATE INDEX IF NOT EXISTS knowledge_source_acl_entries_lookup_idx
    ON moa.knowledge_source_acl_entries (snapshot_id, entry_kind, principal_fingerprint);

-- ---------------------------------------------------------------------------
-- Verified caller principal bindings
-- ---------------------------------------------------------------------------

-- Which provider principal a MOA contact has been VERIFIED to control. Written
-- only by identity-verification paths; retrieval reads it and never writes it,
-- and a caller can never supply one in a request.
CREATE TABLE IF NOT EXISTS moa.knowledge_source_principal_bindings (
    binding_uid             UUID   NOT NULL PRIMARY KEY,
    tenant_id               UUID   NOT NULL,
    storage_partition_id    TEXT   NOT NULL,
    contact_id              UUID   NOT NULL,
    -- No cascade: keyed principal material must not disappear as a side effect
    -- of a connection delete, because that would make the purge's explicit
    -- binding delete unprovable.
    connection_id           UUID
        REFERENCES moa.knowledge_connections(connection_uid),
    principal_kind          TEXT   NOT NULL,
    principal_fingerprint   BYTEA  NOT NULL,
    fingerprint_key_version INT    NOT NULL,
    verified_at             TIMESTAMPTZ NOT NULL,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
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

CREATE INDEX IF NOT EXISTS knowledge_source_principal_bindings_contact_idx
    ON moa.knowledge_source_principal_bindings (tenant_id, contact_id);

CREATE INDEX IF NOT EXISTS knowledge_source_principal_bindings_fk_connection_idx
    ON moa.knowledge_source_principal_bindings (connection_id);

-- Group and domain membership in fingerprint space: `member_fingerprint` is a
-- principal the caller already holds, `group_fingerprint` is one it therefore
-- also holds. Kept separate from the direct bindings so a group expansion can
-- be revoked without touching the caller's own identity binding.
CREATE TABLE IF NOT EXISTS moa.knowledge_source_principal_group_bindings (
    binding_uid             UUID   NOT NULL PRIMARY KEY,
    tenant_id               UUID   NOT NULL,
    storage_partition_id    TEXT   NOT NULL,
    -- No cascade: keyed principal material must not disappear as a side effect
    -- of a connection delete, because that would make the purge's explicit
    -- binding delete unprovable.
    connection_id           UUID
        REFERENCES moa.knowledge_connections(connection_uid),
    member_fingerprint      BYTEA  NOT NULL,
    group_kind              TEXT   NOT NULL,
    group_fingerprint       BYTEA  NOT NULL,
    fingerprint_key_version INT    NOT NULL,
    verified_at             TIMESTAMPTZ NOT NULL,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
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

CREATE INDEX IF NOT EXISTS knowledge_source_principal_group_bindings_member_idx
    ON moa.knowledge_source_principal_group_bindings (tenant_id, member_fingerprint);

CREATE INDEX IF NOT EXISTS knowledge_source_principal_group_bindings_fk_connection_idx
    ON moa.knowledge_source_principal_group_bindings (connection_id);

-- ---------------------------------------------------------------------------
-- Connection mode, object ACL state, and document node identity
-- ---------------------------------------------------------------------------

ALTER TABLE moa.knowledge_connections
    ADD COLUMN IF NOT EXISTS acl_mode TEXT;

ALTER TABLE moa.knowledge_objects
    ADD COLUMN IF NOT EXISTS acl_state TEXT;

ALTER TABLE moa.knowledge_objects
    ADD COLUMN IF NOT EXISTS acl_revision TEXT;

ALTER TABLE moa.knowledge_objects
    ADD COLUMN IF NOT EXISTS current_acl_snapshot_id UUID
        REFERENCES moa.knowledge_source_acl_snapshots(snapshot_uid) ON DELETE SET NULL;

-- The graph node a document version materializes, mirroring the chunk-side
-- occurrence identity landed in V000347. Without it, a `Document` node's title
-- would remain retrievable for an object whose chunks are denied.
ALTER TABLE moa.knowledge_document_versions
    ADD COLUMN IF NOT EXISTS graph_node_uid UUID;

-- Deterministic backfill.
--
-- Every shipped adapter is permission-bearing, so there is no uniformly-public
-- provider to promote and every connection lands on the closed mode. Should a
-- deployment carry a connection from a provider MOA no longer ships, it also
-- lands here: an unrecognized provider is ambiguous, and ambiguity denies.
UPDATE moa.knowledge_connections
SET acl_mode = 'provider_managed'
WHERE acl_mode IS NULL;

-- Objects inherit `incomplete`: no snapshot has ever been captured, so their
-- content stays hidden until a resync produces one.
UPDATE moa.knowledge_objects
SET acl_state = 'incomplete'
WHERE acl_state IS NULL;

-- Recover the graph node identity of already-written document versions from the
-- node the ingestion writer created. `version_uid` is written into every
-- knowledge `Document` node's properties, so this covers exactly the nodes that
-- exist; a version that was never graph-written correctly stays NULL because
-- there is no node for it to govern.
UPDATE moa.knowledge_document_versions AS versions
SET graph_node_uid = node.uid
FROM moa.node_index AS node
WHERE versions.graph_node_uid IS NULL
  AND node.label = 'Document'
  AND node.properties_summary ? 'version_uid'
  AND (node.properties_summary ->> 'version_uid') = versions.document_version_uid::TEXT;

ALTER TABLE moa.knowledge_connections
    ALTER COLUMN acl_mode SET NOT NULL;

ALTER TABLE moa.knowledge_objects
    ALTER COLUMN acl_state SET NOT NULL;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_catalog.pg_constraint
        WHERE conname = 'knowledge_connections_acl_mode_valid'
    ) THEN
        ALTER TABLE moa.knowledge_connections
            ADD CONSTRAINT knowledge_connections_acl_mode_valid
            CHECK (acl_mode IN ('tenant_public', 'provider_managed'));
    END IF;

    IF NOT EXISTS (
        SELECT 1 FROM pg_catalog.pg_constraint
        WHERE conname = 'knowledge_objects_acl_state_valid'
    ) THEN
        ALTER TABLE moa.knowledge_objects
            ADD CONSTRAINT knowledge_objects_acl_state_valid
            CHECK (acl_state IN ('current', 'stale', 'incomplete'));
    END IF;

    -- A `current` object must name the snapshot and revision it is current for.
    -- Without this an object could claim freshness while pointing at nothing,
    -- which the admission predicate would read as "no snapshot" and deny — but
    -- only by accident. Making it a constraint keeps the denial intentional.
    IF NOT EXISTS (
        SELECT 1 FROM pg_catalog.pg_constraint
        WHERE conname = 'knowledge_objects_current_acl_complete'
    ) THEN
        ALTER TABLE moa.knowledge_objects
            ADD CONSTRAINT knowledge_objects_current_acl_complete
            CHECK (
                acl_state <> 'current'
                OR (current_acl_snapshot_id IS NOT NULL AND acl_revision IS NOT NULL)
            );
    END IF;
END;
$$;

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_document_versions_graph_node_uniq
    ON moa.knowledge_document_versions (graph_node_uid)
    WHERE graph_node_uid IS NOT NULL;

CREATE INDEX IF NOT EXISTS knowledge_objects_acl_state_idx
    ON moa.knowledge_objects (tenant_id, acl_state);

-- ---------------------------------------------------------------------------
-- Epoch triggers
-- ---------------------------------------------------------------------------

DROP TRIGGER IF EXISTS source_acl_epoch ON moa.knowledge_source_acl_snapshots;
CREATE TRIGGER source_acl_epoch
    AFTER INSERT OR UPDATE OR DELETE ON moa.knowledge_source_acl_snapshots
    FOR EACH ROW EXECUTE FUNCTION moa.source_acl_epoch_trigger();

DROP TRIGGER IF EXISTS source_acl_epoch ON moa.knowledge_source_acl_entries;
CREATE TRIGGER source_acl_epoch
    AFTER INSERT OR UPDATE OR DELETE ON moa.knowledge_source_acl_entries
    FOR EACH ROW EXECUTE FUNCTION moa.source_acl_epoch_trigger();

DROP TRIGGER IF EXISTS source_acl_epoch ON moa.knowledge_source_principal_bindings;
CREATE TRIGGER source_acl_epoch
    AFTER INSERT OR UPDATE OR DELETE ON moa.knowledge_source_principal_bindings
    FOR EACH ROW EXECUTE FUNCTION moa.source_acl_epoch_trigger();

DROP TRIGGER IF EXISTS source_acl_epoch ON moa.knowledge_source_principal_group_bindings;
CREATE TRIGGER source_acl_epoch
    AFTER INSERT OR UPDATE OR DELETE ON moa.knowledge_source_principal_group_bindings
    FOR EACH ROW EXECUTE FUNCTION moa.source_acl_epoch_trigger();

-- Object-side state changes matter as much as snapshot writes: flipping an
-- object to `stale` is how a revoked permission takes effect before the resync
-- lands, and a cached result must not survive it.
DROP TRIGGER IF EXISTS source_acl_epoch ON moa.knowledge_objects;
CREATE TRIGGER source_acl_epoch
    AFTER UPDATE OF acl_state, acl_revision, current_acl_snapshot_id
    ON moa.knowledge_objects
    FOR EACH ROW EXECUTE FUNCTION moa.source_acl_epoch_trigger();

DROP TRIGGER IF EXISTS source_acl_epoch ON moa.knowledge_connections;
CREATE TRIGGER source_acl_epoch
    AFTER UPDATE OF acl_mode ON moa.knowledge_connections
    FOR EACH ROW EXECUTE FUNCTION moa.source_acl_epoch_trigger();

-- ---------------------------------------------------------------------------
-- Row-level security
-- ---------------------------------------------------------------------------

SELECT moa.apply_tenant_rls('moa.knowledge_source_acl_epochs'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.knowledge_source_principal_bindings'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.knowledge_source_principal_group_bindings'::REGCLASS);

-- Strict tenant isolation with no control-plane branch: an ACL key is always
-- tenant-bound, and a missing `moa.tenant_id` must deny rather than widen to
-- every tenant's fingerprint key.
ALTER TABLE moa.knowledge_source_acl_keys ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.knowledge_source_acl_keys FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON moa.knowledge_source_acl_keys;
CREATE POLICY tenant_isolation ON moa.knowledge_source_acl_keys FOR ALL TO moa_app
    USING (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''))
    WITH CHECK (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''));
GRANT SELECT, INSERT, DELETE ON moa.knowledge_source_acl_keys TO moa_app;

-- Snapshots and their entries are append-only evidence. Granting UPDATE would
-- allow a permission set to be edited in place under an unchanged revision,
-- which is precisely the mutation this table exists to make impossible; the
-- absence of an UPDATE policy AND of the UPDATE grant enforces it twice.
-- DELETE remains so tenant purge and retention can remove them wholesale.
ALTER TABLE moa.knowledge_source_acl_snapshots ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.knowledge_source_acl_snapshots FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON moa.knowledge_source_acl_snapshots;
DROP POLICY IF EXISTS rd_tenant ON moa.knowledge_source_acl_snapshots;
DROP POLICY IF EXISTS wr_tenant ON moa.knowledge_source_acl_snapshots;
CREATE POLICY rd_tenant ON moa.knowledge_source_acl_snapshots FOR SELECT TO moa_app
    USING (
        moa.current_control_plane()
        OR tenant_id::TEXT = moa.current_tenant_id()::TEXT
    );
CREATE POLICY wr_tenant ON moa.knowledge_source_acl_snapshots FOR INSERT TO moa_app
    WITH CHECK (
        moa.current_control_plane()
        OR tenant_id::TEXT = moa.current_tenant_id()::TEXT
    );
CREATE POLICY rm_tenant ON moa.knowledge_source_acl_snapshots FOR DELETE TO moa_app
    USING (
        moa.current_control_plane()
        OR tenant_id::TEXT = moa.current_tenant_id()::TEXT
    );
GRANT SELECT, INSERT, DELETE ON moa.knowledge_source_acl_snapshots TO moa_app;

ALTER TABLE moa.knowledge_source_acl_entries ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.knowledge_source_acl_entries FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON moa.knowledge_source_acl_entries;
DROP POLICY IF EXISTS rd_tenant ON moa.knowledge_source_acl_entries;
DROP POLICY IF EXISTS wr_tenant ON moa.knowledge_source_acl_entries;
CREATE POLICY rd_tenant ON moa.knowledge_source_acl_entries FOR SELECT TO moa_app
    USING (
        moa.current_control_plane()
        OR tenant_id::TEXT = moa.current_tenant_id()::TEXT
    );
CREATE POLICY wr_tenant ON moa.knowledge_source_acl_entries FOR INSERT TO moa_app
    WITH CHECK (
        moa.current_control_plane()
        OR tenant_id::TEXT = moa.current_tenant_id()::TEXT
    );
CREATE POLICY rm_tenant ON moa.knowledge_source_acl_entries FOR DELETE TO moa_app
    USING (
        moa.current_control_plane()
        OR tenant_id::TEXT = moa.current_tenant_id()::TEXT
    );
GRANT SELECT, INSERT, DELETE ON moa.knowledge_source_acl_entries TO moa_app;

GRANT USAGE ON SCHEMA moa TO moa_app;
