-- Contact identities for agent-facing end users.

CREATE TABLE IF NOT EXISTS contacts (
    id UUID PRIMARY KEY,
    contact_id UUID NOT NULL,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('anonymous', 'unverified', 'verified', 'merged')),
    display_name TEXT,
    profile JSONB NOT NULL DEFAULT '{}'::jsonb,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    canonical_contact_id UUID REFERENCES contacts(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    merged_at TIMESTAMPTZ,
    CHECK (
        (state = 'merged' AND canonical_contact_id IS NOT NULL)
        OR (state <> 'merged')
    )
);

CREATE INDEX IF NOT EXISTS idx_contacts_storage_partition_state
    ON contacts(storage_partition_id, state, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_contacts_tenant_storage_partition
    ON contacts(tenant_id, storage_partition_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_contacts_canonical
    ON contacts(canonical_contact_id)
    WHERE canonical_contact_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS contact_points (
    id UUID PRIMARY KEY,
    contact_id UUID NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('email', 'phone', 'external_id', 'anonymous_handle')),
    normalized_hash TEXT NOT NULL,
    display_value TEXT,
    verified BOOLEAN NOT NULL DEFAULT FALSE,
    verified_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_contact_points_contact
    ON contact_points(contact_id, kind, verified);
CREATE UNIQUE INDEX IF NOT EXISTS idx_contact_points_contact_lookup
    ON contact_points(tenant_id, storage_partition_id, contact_id, kind, normalized_hash);
CREATE UNIQUE INDEX IF NOT EXISTS idx_contact_points_verified_unique
    ON contact_points(tenant_id, storage_partition_id, kind, normalized_hash)
    WHERE verified;

CREATE TABLE IF NOT EXISTS contact_token_grants (
    id UUID PRIMARY KEY,
    token_jti TEXT NOT NULL UNIQUE,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    contact_id UUID NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK (state IN ('anonymous', 'unverified', 'verified', 'merged')),
    scopes TEXT[] NOT NULL DEFAULT '{}',
    permissions JSONB NOT NULL DEFAULT '{}'::jsonb,
    agent_ids TEXT[] NOT NULL DEFAULT '{}',
    session_ids UUID[] NOT NULL DEFAULT '{}',
    issued_by_actor_type TEXT NOT NULL,
    issued_by_actor_id UUID,
    expires_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_contact_token_grants_contact
    ON contact_token_grants(tenant_id, storage_partition_id, contact_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_contact_token_grants_active_contact
    ON contact_token_grants(tenant_id, storage_partition_id, contact_id, expires_at)
    WHERE revoked_at IS NULL;

CREATE TABLE IF NOT EXISTS contact_verification_challenges (
    id UUID PRIMARY KEY,
    contact_id UUID NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    contact_point_id UUID NOT NULL REFERENCES contact_points(id) ON DELETE CASCADE,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    code_hash TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    consumed_at TIMESTAMPTZ,
    attempts INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_contact_verification_challenges_contact
    ON contact_verification_challenges(contact_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_contact_verification_challenges_open
    ON contact_verification_challenges(contact_point_id, expires_at)
    WHERE consumed_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_sessions_contact
    ON sessions(storage_partition_id, contact_tenant_id, contact_id, updated_at DESC)
    WHERE contact_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_sessions_contact_promoted_from
    ON sessions(contact_promoted_from_id)
    WHERE contact_promoted_from_id IS NOT NULL;
