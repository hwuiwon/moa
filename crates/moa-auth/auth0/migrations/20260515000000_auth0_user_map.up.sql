-- Maps an external identity provider's subject (Auth0 `sub`, OIDC `sub`)
-- to MOA's internal principal UUID. The column keeps the historical
-- `user_id` name because P1.9 SCIM will own the final users/principals shape.

CREATE TABLE IF NOT EXISTS users (
    id             UUID PRIMARY KEY,
    tenant_id      UUID NOT NULL,
    email          TEXT,
    display_name   TEXT,
    source         TEXT NOT NULL DEFAULT 'local',
    external_id    TEXT,
    deactivated_at TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

ALTER TABLE users
    ADD COLUMN IF NOT EXISTS source TEXT NOT NULL DEFAULT 'local',
    ADD COLUMN IF NOT EXISTS external_id TEXT;

CREATE INDEX IF NOT EXISTS idx_users_tenant ON users(tenant_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_external_per_tenant
    ON users(tenant_id, source, external_id)
    WHERE external_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS auth0_user_map (
    sub        TEXT NOT NULL,
    tenant_id  UUID NOT NULL,
    user_id    UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (sub, tenant_id)
);

CREATE INDEX IF NOT EXISTS idx_auth0_user_map_user ON auth0_user_map(user_id);
