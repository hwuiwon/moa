-- Minimal first-class agent registry used by the auth pack.
--
-- P1.4 needs a durable row to pair with OpenFGA tuples when an agent is
-- registered. Later prompts extend the lifecycle; this table intentionally
-- stores only the fields required for first enforcement.

CREATE TABLE IF NOT EXISTS agents (
    id               UUID        PRIMARY KEY,
    tenant_id        UUID        NOT NULL,
    template_id      UUID,
    operator_user_id UUID        NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_agents_tenant_id
    ON agents(tenant_id);
