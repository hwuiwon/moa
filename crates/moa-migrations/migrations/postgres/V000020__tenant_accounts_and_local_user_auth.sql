-- First-party tenant accounts and password-backed dashboard auth.

CREATE TABLE IF NOT EXISTS tenants (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id          UUID GENERATED ALWAYS AS (id) STORED,
    slug               TEXT NOT NULL,
    name               TEXT NOT NULL,
    status             TEXT NOT NULL DEFAULT 'active'
                           CHECK (status IN ('active', 'suspended', 'deleted')),
    settings           JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_by_user_id UUID,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at         TIMESTAMPTZ
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_tenants_slug_lower_unique
    ON tenants (lower(slug));
CREATE INDEX IF NOT EXISTS idx_tenants_status
    ON tenants (status);

CREATE TABLE IF NOT EXISTS local_user_credentials (
    user_id                 UUID PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    tenant_id               UUID NOT NULL,
    password_hash           TEXT NOT NULL,
    password_set_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    password_reset_required BOOLEAN NOT NULL DEFAULT FALSE,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_local_user_credentials_tenant
    ON local_user_credentials (tenant_id);

CREATE TABLE IF NOT EXISTS password_reset_tokens (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID NOT NULL,
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash  TEXT NOT NULL UNIQUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at  TIMESTAMPTZ NOT NULL,
    used_at     TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_password_reset_tokens_user_active
    ON password_reset_tokens (tenant_id, user_id, expires_at)
    WHERE used_at IS NULL;

CREATE TABLE IF NOT EXISTS user_session_tokens (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    token_prefix   TEXT NOT NULL UNIQUE,
    token_hash     TEXT NOT NULL,
    tenant_id      UUID NOT NULL,
    user_id        UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at     TIMESTAMPTZ NOT NULL,
    last_used_at   TIMESTAMPTZ,
    revoked_at     TIMESTAMPTZ,
    revoked_reason TEXT
);

CREATE INDEX IF NOT EXISTS idx_user_session_tokens_user_active
    ON user_session_tokens (tenant_id, user_id, expires_at)
    WHERE revoked_at IS NULL;

SELECT moa.apply_tenant_rls('tenants'::regclass);
SELECT moa.apply_tenant_rls('local_user_credentials'::regclass);
SELECT moa.apply_tenant_rls('password_reset_tokens'::regclass);
SELECT moa.apply_tenant_rls('user_session_tokens'::regclass);
