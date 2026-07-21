-- Self-hosted token vault: per-(tenant, user, connection) third-party OAuth tokens.
--
-- Unlike the Auth0 Token Vault (which holds token material externally and stores
-- only linkage metadata in `linked_connections`), the self-hosted vault persists
-- the sealed access and refresh token material here so MOA can broker delegated
-- third-party access without an external identity provider.
--
-- `PostgresTokenVaultProvider` seals token secrets through the explicitly
-- injected KMS before insertion, so this table never sees plaintext material.
--
-- The row-level-security policy is defined inline (rather than via the `moa`
-- schema helpers) so the table applies identically in the full central schema
-- and in the isolated auth-provider test schema, which does not install those
-- helpers.

CREATE TABLE token_vault_connections (
    id                    UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id             UUID        NOT NULL,
    user_id               UUID        NOT NULL,
    connection_name       TEXT        NOT NULL,
    provider              TEXT        NOT NULL,
    external_account_id   TEXT,
    access_token_sealed   BYTEA       NOT NULL,
    refresh_token_sealed  BYTEA,
    token_type            TEXT,
    scopes                TEXT[]      NOT NULL DEFAULT '{}',
    expires_at            TIMESTAMPTZ,
    generation            BIGINT      NOT NULL DEFAULT 1,
    refresh_state         TEXT        NOT NULL DEFAULT 'ready',
    refresh_lease_id      UUID,
    refresh_lease_expires_at TIMESTAMPTZ,
    created_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT token_vault_connections_generation_positive
        CHECK (generation > 0),
    CONSTRAINT token_vault_connections_refresh_state_valid
        CHECK (refresh_state IN ('ready', 'refreshing', 'relink_required')),
    CONSTRAINT token_vault_connections_refresh_lease_consistent
        CHECK (
            (
                refresh_state = 'refreshing'
                AND refresh_lease_id IS NOT NULL
                AND refresh_lease_expires_at IS NOT NULL
            )
            OR (
                refresh_state IN ('ready', 'relink_required')
                AND refresh_lease_id IS NULL
                AND refresh_lease_expires_at IS NULL
            )
        ),
    CONSTRAINT token_vault_connections_tenant_user_conn_key
        UNIQUE (tenant_id, user_id, connection_name)
);

-- get_token/list_connections resolve by the globally-unique user_id without a
-- tenant, so the (user_id, connection_name) pair is unique across all tenants.
CREATE UNIQUE INDEX idx_token_vault_connections_user
    ON token_vault_connections (user_id, connection_name);

CREATE INDEX idx_token_vault_connections_tenant
    ON token_vault_connections (tenant_id);

-- Supports a future refresh reaper sweeping tokens near expiry.
CREATE INDEX idx_token_vault_connections_expires
    ON token_vault_connections (expires_at)
    WHERE expires_at IS NOT NULL;

-- Supports bounded recovery of refresh attempts whose remote outcome is
-- uncertain after the winning pod disappears.
CREATE INDEX idx_token_vault_connections_refresh_lease
    ON token_vault_connections (refresh_lease_expires_at)
    WHERE refresh_state = 'refreshing';

ALTER TABLE token_vault_connections ENABLE ROW LEVEL SECURITY;
ALTER TABLE token_vault_connections FORCE ROW LEVEL SECURITY;

-- Tenant isolation with a control-plane escape hatch, mirroring the
-- moa.apply_tenant_rls helper used elsewhere. Control-plane transactions read
-- across tenants keyed by the globally-unique user_id; tenant-scoped writes are
-- pinned to the caller's tenant.
CREATE POLICY tenant_isolation ON token_vault_connections FOR ALL TO moa_app
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

GRANT SELECT, INSERT, UPDATE, DELETE ON token_vault_connections TO moa_app;

-- Grant schema usage to the app role for whichever schema owns the table
-- (public in the central schema; the isolated schema under auth-provider tests).
-- Granting usage on public to moa_app is a harmless no-op.
DO $$
BEGIN
    EXECUTE format('GRANT USAGE ON SCHEMA %I TO moa_app', current_schema());
END $$;
