-- Tenant-admin invitations for dashboard users.

CREATE TABLE IF NOT EXISTS tenant_user_invitations (
    id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id          UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    user_id            UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    email              TEXT NOT NULL,
    role               TEXT NOT NULL CHECK (role IN ('admin', 'operator')),
    token_hash         TEXT NOT NULL UNIQUE,
    invited_by_user_id UUID NOT NULL,
    expires_at         TIMESTAMPTZ NOT NULL,
    accepted_at        TIMESTAMPTZ,
    revoked_at         TIMESTAMPTZ,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_tenant_user_invitations_pending
    ON tenant_user_invitations (tenant_id, lower(email), expires_at)
    WHERE accepted_at IS NULL AND revoked_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_tenant_user_invitations_user
    ON tenant_user_invitations (tenant_id, user_id, created_at DESC);

SELECT moa.apply_tenant_rls('tenant_user_invitations'::regclass);
