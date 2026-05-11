-- Users and SCIM provisioning tables.
--
-- Earlier auth prompts may already have created a minimal users table.
-- This migration keeps that table and adds the columns SCIM needs.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE IF NOT EXISTS users (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID NOT NULL,
    email           TEXT,
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

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'public'
          AND table_name = 'users'
          AND column_name = 'id'
          AND data_type IN ('text', 'character varying')
    ) THEN
        ALTER TABLE users
            ALTER COLUMN id TYPE UUID
            USING CASE
                WHEN id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                    THEN id::UUID
                ELSE gen_random_uuid()
            END;
    END IF;
END $$;

ALTER TABLE users
    ADD COLUMN IF NOT EXISTS tenant_id UUID,
    ADD COLUMN IF NOT EXISTS email TEXT,
    ADD COLUMN IF NOT EXISTS external_id TEXT,
    ADD COLUMN IF NOT EXISTS given_name TEXT,
    ADD COLUMN IF NOT EXISTS family_name TEXT,
    ADD COLUMN IF NOT EXISTS display_name TEXT,
    ADD COLUMN IF NOT EXISTS active BOOLEAN NOT NULL DEFAULT TRUE,
    ADD COLUMN IF NOT EXISTS deactivated_at TIMESTAMPTZ,
    ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ADD COLUMN IF NOT EXISTS version BIGINT NOT NULL DEFAULT 1;

UPDATE users
SET email = COALESCE(email, external_id, id::TEXT || '@local.invalid')
WHERE email IS NULL;

UPDATE users
SET tenant_id = '00000000-0000-0000-0000-000000000000'::UUID
WHERE tenant_id IS NULL;

ALTER TABLE users
    ALTER COLUMN tenant_id SET NOT NULL,
    ALTER COLUMN email SET NOT NULL,
    ALTER COLUMN active SET NOT NULL,
    ALTER COLUMN created_at SET NOT NULL,
    ALTER COLUMN updated_at SET NOT NULL,
    ALTER COLUMN version SET NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_users_tenant_email_unique
    ON users(tenant_id, email);
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_tenant_external_id_unique
    ON users(tenant_id, external_id)
    WHERE external_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_users_tenant_active
    ON users(tenant_id, active);
CREATE INDEX IF NOT EXISTS idx_users_external_id
    ON users(tenant_id, external_id)
    WHERE external_id IS NOT NULL;

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
