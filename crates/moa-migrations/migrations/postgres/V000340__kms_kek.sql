-- Persistent root-key rotation state and per-subject key-encryption keys.
--
-- Root-key material is never stored here. Kubernetes mounts an immutable
-- directory of base64 keys into every KMS-capable pod; Postgres stores only the
-- generation identifiers needed for non-sticky replicas to agree on the active
-- key and resume bounded rewrap jobs.

CREATE TABLE moa.kms_root_key_generations (
    generation   TEXT        PRIMARY KEY,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    activated_at TIMESTAMPTZ,
    retired_at   TIMESTAMPTZ,
    CONSTRAINT kms_root_key_generation_nonempty CHECK (generation <> '')
);

COMMENT ON TABLE moa.kms_root_key_generations IS
    'Non-secret lifecycle metadata for mounted KMS root-key generations.';

CREATE TABLE moa.kms_root_key_state (
    singleton         BOOLEAN     PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    active_generation TEXT        NOT NULL REFERENCES moa.kms_root_key_generations(generation),
    state_version     BIGINT      NOT NULL DEFAULT 0 CHECK (state_version >= 0),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

COMMENT ON TABLE moa.kms_root_key_state IS
    'Singleton database-selected root-key generation for new KEKs. state_version is the activation CAS fence.';

CREATE TABLE moa.kek (
    kek_id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id           UUID        NOT NULL,
    subject_id          UUID        NOT NULL,
    wrapped_kek         BYTEA,
    root_key_generation TEXT        NOT NULL REFERENCES moa.kms_root_key_generations(generation),
    rewrap_version      BIGINT      NOT NULL DEFAULT 0 CHECK (rewrap_version >= 0),
    rewrapped_at        TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    destroyed_at        TIMESTAMPTZ,
    CONSTRAINT kek_tenant_subject_key UNIQUE (tenant_id, subject_id),
    CONSTRAINT kek_destroyed_material CHECK (
        (destroyed_at IS NULL AND wrapped_kek IS NOT NULL)
        OR (destroyed_at IS NOT NULL AND wrapped_kek IS NULL)
    )
);

COMMENT ON TABLE moa.kek IS
    'Per-(tenant, subject) KEKs wrapped under a recorded deployment root-key generation. Per-row generation/version make bounded rewrap resumable and CAS-safe.';
COMMENT ON COLUMN moa.kek.wrapped_kek IS
    'KEK wrapped by moa-crypto as nonce || ciphertext+tag with tenant|subject|kek_id AAD; NULL after crypto-shred.';
COMMENT ON COLUMN moa.kek.root_key_generation IS
    'Mounted root-key filename/generation that currently wraps wrapped_kek.';
COMMENT ON COLUMN moa.kek.rewrap_version IS
    'Monotonic CAS fence incremented after each successful root-key rewrap.';
COMMENT ON COLUMN moa.kek.destroyed_at IS
    'When set, the subject KEK is irrecoverably crypto-shredded and wrapped_kek is NULL.';

CREATE INDEX idx_kek_tenant ON moa.kek (tenant_id);
CREATE INDEX idx_kek_live_root_generation
    ON moa.kek (root_key_generation, kek_id)
    WHERE destroyed_at IS NULL;

ALTER TABLE moa.kek ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.kek FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON moa.kek FOR ALL TO moa_app
    USING (
        lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
        OR tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), '')
    )
    WITH CHECK (
        lower(COALESCE(NULLIF(current_setting('moa.control_plane', TRUE), ''), 'false'))
            IN ('1', 'true', 't', 'yes', 'on')
        OR tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), '')
    );

GRANT SELECT, INSERT, UPDATE, DELETE ON moa.kek TO moa_app;
GRANT SELECT, INSERT, UPDATE ON moa.kms_root_key_generations TO moa_app;
GRANT SELECT, INSERT, UPDATE ON moa.kms_root_key_state TO moa_app;
GRANT USAGE ON SCHEMA moa TO moa_app;
