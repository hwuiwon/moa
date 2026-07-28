-- Tenant-owned MCP connection bindings.
--
-- Before this table one deployment credential served every tenant that invoked a
-- configured MCP server: least privilege was impossible and one tenant's key
-- could not be rotated or revoked without cutting off every other tenant.
--
-- A binding is the secret-free owner of that relationship. One row states, for
-- exactly one tenant, that MCP server `server_name` is served by the tenant's
-- own connection `connection_uid` using the exact stored credential version
-- `credential_ref`, and that the credential may be used only for the operations
-- named in `allowed_operations`.
--
-- The row deliberately holds no material and no material selector. It names an
-- opaque `CredentialRef` handle (the `tenant_credential_versions.credential_uid`
-- minted by the durable credential vault), which is resolvable only inside the
-- trusted MCP proxy under the tenant's own forced-RLS context. A leaked binding
-- row therefore discloses nothing beyond the fact that a connection exists.
--
-- Two states only: `active` (usable) and `disabled` (retained for operator
-- history, never dispatched). A binding is not deleted on disable, so an
-- operator can still see that a server was once connected.
--
-- There is deliberately no foreign key into `tenant_credential_versions`: that
-- table is installed standalone by the auth-provider test harness, and the
-- reference is re-validated on every resolve against the version's own identity
-- (tenant, connection, kind), which a foreign key cannot express.
--
-- The table is unqualified and its row-level-security policies are defined
-- inline (rather than through the `moa` schema helpers) so it applies
-- identically in the full central schema and in the isolated tool-routing test
-- schema, which does not install those helpers.

CREATE TABLE IF NOT EXISTS tenant_mcp_connection_bindings (
    tenant_id          UUID        NOT NULL,
    connection_uid     UUID        NOT NULL,
    server_name        TEXT        NOT NULL,
    credential_ref     UUID        NOT NULL,
    status             TEXT        NOT NULL,
    allowed_operations TEXT[]      NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, connection_uid, server_name),
    CONSTRAINT tenant_mcp_connection_bindings_status_valid
        CHECK (status IN ('active', 'disabled')),
    CONSTRAINT tenant_mcp_connection_bindings_server_name_present
        CHECK (server_name <> ''),
    -- A closed allowlist: a binding that names no operation authorizes nothing,
    -- and an empty or NULL entry can never match a canonical operation name, so
    -- both are rejected at write time rather than silently never matching.
    CONSTRAINT tenant_mcp_connection_bindings_operations_closed
        CHECK (
            array_ndims(allowed_operations) = 1
            AND cardinality(allowed_operations) > 0
            AND array_position(allowed_operations, NULL) IS NULL
            AND array_position(allowed_operations, '') IS NULL
        )
);

-- At most one active binding per (tenant, server). Two connections may both hold
-- a disabled binding for the same server — that is operator history — but the
-- server a tenant dispatches to resolves to exactly one connection, so a
-- dispatch can never have to choose between two candidate credentials.
CREATE UNIQUE INDEX IF NOT EXISTS tenant_mcp_connection_bindings_one_active_server
    ON tenant_mcp_connection_bindings (tenant_id, server_name)
    WHERE status = 'active';

-- Supports connection-scoped lifecycle sweeps (disable/delete on unlink).
CREATE INDEX IF NOT EXISTS tenant_mcp_connection_bindings_connection_idx
    ON tenant_mcp_connection_bindings (tenant_id, connection_uid);

ALTER TABLE tenant_mcp_connection_bindings ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_mcp_connection_bindings FORCE ROW LEVEL SECURITY;

-- Strict tenant isolation with no control-plane branch, matching the credential
-- tables this row references: a binding is always tenant-bound, so a missing or
-- wrong `moa.tenant_id` denies rather than widening to every tenant. This is
-- what makes one tenant's MCP credential unreachable from another tenant's
-- session even when a server name is shared.
CREATE POLICY tenant_isolation ON tenant_mcp_connection_bindings FOR ALL TO moa_app
    USING (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''))
    WITH CHECK (tenant_id::TEXT = NULLIF(current_setting('moa.tenant_id', TRUE), ''));

GRANT SELECT, INSERT, UPDATE, DELETE ON tenant_mcp_connection_bindings TO moa_app;

-- Grant schema usage to the app role for whichever schema owns the table
-- (public in the central schema; the isolated schema under tool-routing tests).
DO $$
BEGIN
    EXECUTE format('GRANT USAGE ON SCHEMA %I TO moa_app', current_schema());
END $$;
