CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE IF NOT EXISTS tenant_signing_keys (
    id             UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id      UUID        NOT NULL,
    key_b64        TEXT        NOT NULL,
    active         BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deactivated_at TIMESTAMPTZ
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_tenant_signing_keys_active
    ON tenant_signing_keys(tenant_id)
    WHERE active = TRUE;

CREATE INDEX IF NOT EXISTS idx_tenant_signing_keys_tenant
    ON tenant_signing_keys(tenant_id, created_at DESC);
