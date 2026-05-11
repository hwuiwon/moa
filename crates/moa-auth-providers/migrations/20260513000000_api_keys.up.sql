-- API keys for the LocalAuthProvider.
--
-- Wire format: moa_<env>_<32-char-base62-random>_<8-char-lowercase-hex-crc32>
-- Example:      moa_dev_a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6_3f9a1b2c
--
-- The full key value is never stored; only argon2id(full_key) is stored.
-- Prefix lookup narrows validation to at most one candidate hash.

CREATE TABLE api_keys (
    id               UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    prefix           TEXT        NOT NULL UNIQUE,
    hash             TEXT        NOT NULL,
    owner_user_id    UUID,
    owner_agent_id   UUID,
    tenant_id        UUID        NOT NULL,
    name             TEXT        NOT NULL,
    description      TEXT,
    env              TEXT        NOT NULL CHECK (env IN ('live','prod','stg','dev')),
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_used_at     TIMESTAMPTZ,
    revoked_at       TIMESTAMPTZ,
    revoked_reason   TEXT,
    scopes_synced_at TIMESTAMPTZ,
    CHECK (
        (owner_user_id IS NOT NULL AND owner_agent_id IS NULL)
        OR
        (owner_user_id IS NULL     AND owner_agent_id IS NOT NULL)
    )
);

CREATE INDEX idx_api_keys_owner_user
    ON api_keys(owner_user_id)
    WHERE owner_user_id IS NOT NULL;

CREATE INDEX idx_api_keys_owner_agent
    ON api_keys(owner_agent_id)
    WHERE owner_agent_id IS NOT NULL;

CREATE INDEX idx_api_keys_tenant
    ON api_keys(tenant_id);

CREATE INDEX idx_api_keys_active
    ON api_keys(prefix)
    WHERE revoked_at IS NULL;

CREATE TABLE api_key_revocations (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    api_key_id    UUID        NOT NULL REFERENCES api_keys(id),
    revoked_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    reason        TEXT        NOT NULL,
    actor_user_id UUID
);

CREATE INDEX idx_api_key_revocations_key
    ON api_key_revocations(api_key_id);
