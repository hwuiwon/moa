-- Full agent lifecycle schema for first-class agent principals.

CREATE TABLE IF NOT EXISTS agents (
    -- Registration omits `id` and relies on the database to generate it.
    id                  UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id           UUID        NOT NULL,
    operator_user_id    UUID,
    display_name        TEXT        NOT NULL,
    status              TEXT        NOT NULL DEFAULT 'active',
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deactivated_at      TIMESTAMPTZ,
    deactivated_reason  TEXT,
    CONSTRAINT agents_status_check
        CHECK (status IN ('active','suspended','deactivated'))
);

CREATE INDEX IF NOT EXISTS idx_agents_tenant_id
    ON agents(tenant_id);

CREATE INDEX IF NOT EXISTS idx_agents_operator
    ON agents(operator_user_id)
    WHERE operator_user_id IS NOT NULL;

-- Users and SCIM provisioning tables.

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
    settings        JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    version         BIGINT NOT NULL DEFAULT 1
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_users_tenant_email_lower_unique
    ON users(tenant_id, lower(email));
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
