-- Channel identity and session routing state.

CREATE TABLE IF NOT EXISTS contact_channel_accounts (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    contact_id UUID NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    contact_point_id UUID REFERENCES contact_points(id) ON DELETE SET NULL,
    channel TEXT NOT NULL CHECK (channel IN ('chat', 'slack', 'email', 'sms')),
    external_tenant_key TEXT,
    external_user_key TEXT NOT NULL,
    display_name TEXT,
    assurance TEXT NOT NULL DEFAULT 'provider_asserted'
        CHECK (assurance IN ('anonymous', 'provider_asserted', 'otp_verified', 'admin_linked')),
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    first_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    merged_into_id UUID REFERENCES contact_channel_accounts(id),
    CHECK (
        (channel IN ('email', 'sms') AND contact_point_id IS NOT NULL)
        OR channel NOT IN ('email', 'sms')
    )
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_contact_channel_accounts_lookup_active
    ON contact_channel_accounts(
        tenant_id,
        channel,
        COALESCE(external_tenant_key, ''),
        external_user_key
    )
    WHERE merged_into_id IS NULL;

CREATE INDEX IF NOT EXISTS idx_contact_channel_accounts_contact
    ON contact_channel_accounts(tenant_id, storage_partition_id, contact_id, channel, last_seen_at DESC);

CREATE INDEX IF NOT EXISTS idx_contact_channel_accounts_point
    ON contact_channel_accounts(contact_point_id)
    WHERE contact_point_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS session_channel_bindings (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    contact_id UUID NOT NULL REFERENCES contacts(id) ON DELETE CASCADE,
    channel_account_id UUID REFERENCES contact_channel_accounts(id) ON DELETE SET NULL,
    contact_point_id UUID REFERENCES contact_points(id) ON DELETE SET NULL,
    channel TEXT NOT NULL CHECK (channel IN ('chat', 'slack', 'email', 'sms')),
    external_tenant_key TEXT,
    external_conversation_key TEXT,
    external_thread_key TEXT,
    route JSONB NOT NULL DEFAULT '{}'::jsonb,
    reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    activated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ended_at TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_session_channel_bindings_one_active
    ON session_channel_bindings(session_id)
    WHERE ended_at IS NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_session_channel_bindings_external_active
    ON session_channel_bindings(
        channel,
        COALESCE(external_tenant_key, ''),
        COALESCE(external_conversation_key, ''),
        COALESCE(external_thread_key, ''),
        tenant_id
    )
    WHERE ended_at IS NULL AND external_conversation_key IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_session_channel_bindings_contact
    ON session_channel_bindings(tenant_id, storage_partition_id, contact_id, channel, last_used_at DESC);

CREATE INDEX IF NOT EXISTS idx_session_channel_bindings_account
    ON session_channel_bindings(channel_account_id, ended_at)
    WHERE channel_account_id IS NOT NULL;

ALTER TABLE sessions
    ADD CONSTRAINT sessions_active_channel_binding_fk
    FOREIGN KEY (active_channel_binding_id)
    REFERENCES session_channel_bindings(id)
    ON DELETE SET NULL
    DEFERRABLE INITIALLY DEFERRED;
