
-- Source: V000201__orchestrator_agents.sql

-- Minimal first-class agent registry used by the auth pack.
--
-- P1.4 needs a durable row to pair with OpenFGA tuples when an agent is
-- registered. Later prompts extend the lifecycle; this table intentionally
-- stores only the fields required for first enforcement.

CREATE TABLE IF NOT EXISTS agents (
    id               UUID        PRIMARY KEY,
    tenant_id        UUID        NOT NULL,
    operator_user_id UUID        NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_agents_tenant_id
    ON agents(tenant_id);

-- Source: V000202__orchestrator_agents_lifecycle.sql

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
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'agents_status_check'
          AND conrelid = 'agents'::regclass
    ) THEN
        ALTER TABLE agents
            ADD CONSTRAINT agents_status_check
            CHECK (status IN ('active','suspended','deactivated')) NOT VALID;
    END IF;
END $$;

ALTER TABLE agents VALIDATE CONSTRAINT agents_status_check;

CREATE INDEX IF NOT EXISTS idx_agents_operator
    ON agents(operator_user_id)
    WHERE operator_user_id IS NOT NULL;

-- Source: V000203__orchestrator_users_and_scim.sql

-- Users and SCIM provisioning tables.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE IF NOT EXISTS users (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID NOT NULL,
    email           TEXT NOT NULL,
    external_id     TEXT,
    given_name      TEXT,
    family_name     TEXT,
    display_name    TEXT,
    active          BOOLEAN NOT NULL DEFAULT TRUE,
    deactivated_at  TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    version         BIGINT NOT NULL DEFAULT 1
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_users_tenant_email_unique
    ON users(tenant_id, email);
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_tenant_external_id_unique
    ON users(tenant_id, external_id)
    WHERE external_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_users_tenant_active
    ON users(tenant_id)
    WHERE active;

CREATE TABLE IF NOT EXISTS scim_groups (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID NOT NULL,
    display_name    TEXT NOT NULL,
    external_id     TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    version         BIGINT NOT NULL DEFAULT 1
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_scim_groups_tenant_display_name_unique
    ON scim_groups(tenant_id, display_name);
CREATE UNIQUE INDEX IF NOT EXISTS idx_scim_groups_tenant_external_id_unique
    ON scim_groups(tenant_id, external_id)
    WHERE external_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS scim_group_members (
    group_id   UUID NOT NULL REFERENCES scim_groups(id) ON DELETE CASCADE,
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    added_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (group_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_scim_group_members_user
    ON scim_group_members(user_id);
