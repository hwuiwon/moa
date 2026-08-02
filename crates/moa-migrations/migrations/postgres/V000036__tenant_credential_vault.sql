-- Durable encrypted tenant credential owner.
--
-- Replaces process-local environment credential vaults with one versioned,
-- envelope-encrypted store plus an append-only, secret-free operation audit.
--
-- Two tables:
--
--   tenant_credential_versions  -- ciphertext + KMS metadata, one active version
--                                  per (tenant, connection, kind)
--   tenant_credential_operations -- append-only audit of create/resolve/rotate/
--                                  revoke/delete, keyed for replay safety
--
-- `PostgresCredentialVault` seals material through the explicitly injected KMS
-- before insertion, so neither table ever sees plaintext.
--
-- There is deliberately NO foreign key from `connection_uid` into the knowledge
-- schema: the auth-provider DB harness installs these tables standalone, and the
-- connection lifecycle is enforced by the typed owner plus the orchestrator
-- transaction/workflow boundary instead.
--
-- Row-level-security policies are defined inline (rather than through the `moa`
-- schema helpers) so the tables apply identically in the full central schema and
-- in the isolated auth-provider test schema, which does not install those
-- helpers.

CREATE TABLE IF NOT EXISTS tenant_credential_versions (
    credential_uid    UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id         UUID        NOT NULL,
    connection_uid    UUID        NOT NULL,
    kind              TEXT        NOT NULL,
    version           BIGINT      NOT NULL,
    material_sealed   BYTEA       NOT NULL,
    kms_key_id        TEXT        NOT NULL,
    active            BOOLEAN     NOT NULL DEFAULT TRUE,
    revoked           BOOLEAN     NOT NULL DEFAULT FALSE,
    revoked_at        TIMESTAMPTZ,
    owner_identity_id UUID,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT tenant_credential_versions_version_positive
        CHECK (version > 0),
    CONSTRAINT tenant_credential_versions_kind_valid
        CHECK (kind IN ('provider_api_key', 'oauth')),
    CONSTRAINT tenant_credential_versions_material_present
        CHECK (octet_length(material_sealed) > 0),
    CONSTRAINT tenant_credential_versions_kms_key_present
        CHECK (kms_key_id <> ''),
    -- A revoked version is never the active one, and carries its revocation time.
    CONSTRAINT tenant_credential_versions_revocation_consistent
        CHECK (
            (revoked = FALSE AND revoked_at IS NULL)
            OR (revoked = TRUE AND revoked_at IS NOT NULL AND active = FALSE)
        ),
    CONSTRAINT tenant_credential_versions_identity_key
        UNIQUE (tenant_id, connection_uid, kind, version)
);

-- At most one active version per credential series. This is the database-owned
-- half of the compare-and-swap rotation contract: two concurrent rotations
-- cannot both leave an active version behind.
CREATE UNIQUE INDEX IF NOT EXISTS tenant_credential_versions_one_active
    ON tenant_credential_versions (tenant_id, connection_uid, kind)
    WHERE active;

CREATE INDEX IF NOT EXISTS tenant_credential_versions_tenant_idx
    ON tenant_credential_versions (tenant_id);

-- Supports tenant-purge and connection-scoped lifecycle sweeps.
CREATE INDEX IF NOT EXISTS tenant_credential_versions_connection_idx
    ON tenant_credential_versions (tenant_id, connection_uid);

CREATE TABLE IF NOT EXISTS tenant_credential_operations (
    operation_uid   UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID        NOT NULL,
    operation_id    TEXT        NOT NULL,
    request_hash    TEXT        NOT NULL,
    operation       TEXT        NOT NULL,
    credential_uid  UUID,
    connection_uid  UUID,
    kind            TEXT,
    version         BIGINT,
    principal_kind  TEXT        NOT NULL,
    principal_id    UUID,
    delegated_by    UUID,
    service_actor   TEXT,
    outcome         TEXT        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT tenant_credential_operations_operation_valid
        CHECK (operation IN ('create', 'resolve', 'rotate', 'revoke', 'delete')),
    CONSTRAINT tenant_credential_operations_principal_valid
        CHECK (
            (principal_kind = 'caller' AND principal_id IS NOT NULL AND service_actor IS NULL)
            OR (principal_kind = 'service' AND service_actor IS NOT NULL AND principal_id IS NULL)
        ),
    CONSTRAINT tenant_credential_operations_outcome_valid
        CHECK (outcome IN ('succeeded', 'denied', 'failed')),
    CONSTRAINT tenant_credential_operations_operation_id_present
        CHECK (operation_id <> ''),
    CONSTRAINT tenant_credential_operations_request_hash_present
        CHECK (request_hash <> '')
);

-- The replay key. A repeated (tenant, operation_id) with the same request hash
-- replays exactly one row; a changed hash is a typed idempotency conflict.
CREATE UNIQUE INDEX IF NOT EXISTS tenant_credential_operations_replay_key
    ON tenant_credential_operations (tenant_id, operation_id);

CREATE INDEX IF NOT EXISTS tenant_credential_operations_credential_idx
    ON tenant_credential_operations (tenant_id, credential_uid)
    WHERE credential_uid IS NOT NULL;

ALTER TABLE tenant_credential_versions ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_credential_versions FORCE ROW LEVEL SECURITY;
ALTER TABLE tenant_credential_operations ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_credential_operations FORCE ROW LEVEL SECURITY;

-- Strict tenant isolation. Unlike the token vault there is NO control-plane
-- escape hatch: every credential access is tenant-bound, so a missing or wrong
-- `moa.tenant_id` denies rather than widening to all tenants.
CREATE POLICY tenant_isolation ON tenant_credential_versions FOR ALL TO moa_app
    USING (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID)
    WITH CHECK (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID);

-- The audit is append-only for ordinary application access: SELECT and INSERT
-- policies exist, and no UPDATE policy exists, so an ordinary role cannot
-- rewrite history.
CREATE POLICY audit_tenant_read ON tenant_credential_operations FOR SELECT TO moa_app
    USING (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID);

CREATE POLICY audit_tenant_append ON tenant_credential_operations FOR INSERT TO moa_app
    WITH CHECK (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID);

-- Deletion is reachable only through the narrowly scoped tenant-purge lifecycle
-- path, which must explicitly set `moa.credential_purge` for the transaction in
-- addition to being correctly tenant-scoped. Ordinary resolve/rotate traffic
-- never sets it, so it cannot delete audit history.
CREATE POLICY audit_purge_delete ON tenant_credential_operations FOR DELETE TO moa_app
    USING (
        tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID
        AND lower(COALESCE(NULLIF(current_setting('moa.credential_purge', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
    );

GRANT SELECT, INSERT, UPDATE, DELETE ON tenant_credential_versions TO moa_app;
-- No UPDATE grant: the audit is append-only even before policy evaluation.
GRANT SELECT, INSERT, DELETE ON tenant_credential_operations TO moa_app;

-- Grant schema usage to the app role for whichever schema owns the tables
-- (public in the central schema; the isolated schema under auth-provider tests).
DO $$
BEGIN
    EXECUTE format('GRANT USAGE ON SCHEMA %I TO moa_app', current_schema());
END $$;
