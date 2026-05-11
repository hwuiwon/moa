-- Full agent lifecycle schema for templates and first-class agent principals.

CREATE TABLE IF NOT EXISTS agent_templates (
    id                 UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id          UUID        NOT NULL,
    name               TEXT        NOT NULL,
    description        TEXT,
    instructions       TEXT        NOT NULL,
    allowed_tools      TEXT[]      NOT NULL DEFAULT '{}',
    created_by_user_id UUID        NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deactivated_at     TIMESTAMPTZ,
    UNIQUE (tenant_id, name)
);

CREATE TABLE IF NOT EXISTS agents (
    id                  UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id           UUID        NOT NULL,
    template_id         UUID        REFERENCES agent_templates(id),
    operator_user_id    UUID        NOT NULL,
    display_name        TEXT        NOT NULL,
    status              TEXT        NOT NULL DEFAULT 'active',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deactivated_at      TIMESTAMPTZ,
    deactivated_reason  TEXT
);

ALTER TABLE agents
    ADD COLUMN IF NOT EXISTS template_id UUID REFERENCES agent_templates(id),
    ADD COLUMN IF NOT EXISTS operator_user_id UUID,
    ADD COLUMN IF NOT EXISTS display_name TEXT,
    ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'active',
    ADD COLUMN IF NOT EXISTS deactivated_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS deactivated_reason TEXT;

UPDATE agents
SET display_name = id::TEXT
WHERE display_name IS NULL;

ALTER TABLE agents
    ALTER COLUMN display_name SET NOT NULL,
    ALTER COLUMN operator_user_id SET NOT NULL;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'agents_status_check'
    ) THEN
        ALTER TABLE agents
            ADD CONSTRAINT agents_status_check
            CHECK (status IN ('active','suspended','deactivated')) NOT VALID;
    END IF;
END $$;

ALTER TABLE agents VALIDATE CONSTRAINT agents_status_check;

CREATE INDEX IF NOT EXISTS idx_agents_tenant ON agents(tenant_id);
CREATE INDEX IF NOT EXISTS idx_agents_operator ON agents(operator_user_id);
CREATE INDEX IF NOT EXISTS idx_agent_templates_tenant ON agent_templates(tenant_id);
CREATE INDEX IF NOT EXISTS idx_agent_templates_creator ON agent_templates(created_by_user_id);
