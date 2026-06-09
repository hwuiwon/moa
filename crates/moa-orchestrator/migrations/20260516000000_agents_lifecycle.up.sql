-- Full agent lifecycle schema for first-class agent principals.

CREATE TABLE IF NOT EXISTS agents (
    id                  UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id           UUID        NOT NULL,
    operator_user_id    UUID,
    display_name        TEXT        NOT NULL,
    status              TEXT        NOT NULL DEFAULT 'active',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deactivated_at      TIMESTAMPTZ,
    deactivated_reason  TEXT
);

ALTER TABLE agents
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
    ALTER COLUMN operator_user_id DROP NOT NULL;

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
