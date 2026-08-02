-- Tenant sandbox-policy layer. The hand-lease migration creates the lease table
-- in its final worker/profile/origin/deadline shape.

-- The tenant policy layer. A tenant with no row contributes the named
-- `tenant-sandbox-unset` identity layer, which restricts nothing; a row here is
-- how a tenant tightens beyond what the deployment already bounds.
CREATE TABLE IF NOT EXISTS moa.tenant_sandbox_policy (
    tenant_id UUID PRIMARY KEY,
    revision TEXT NOT NULL CHECK (length(btrim(revision)) > 0),
    profile JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

SELECT moa.apply_tenant_rls('moa.tenant_sandbox_policy'::REGCLASS);
