-- Durable hand lease state for cross-pod sandbox reuse and cleanup.

CREATE TABLE IF NOT EXISTS moa.hand_leases (
    session_id UUID NOT NULL,
    tenant_id UUID NOT NULL,
    provider TEXT NOT NULL,
    tier TEXT NOT NULL CHECK (tier IN ('none', 'container', 'microvm', 'local')),
    handle JSONB NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('provisioning', 'active', 'stale', 'destroyed', 'failed')),
    generation BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (session_id, provider)
);

CREATE INDEX IF NOT EXISTS idx_hand_leases_status_expires
    ON moa.hand_leases (status, expires_at);

CREATE INDEX IF NOT EXISTS idx_hand_leases_tenant_session
    ON moa.hand_leases (tenant_id, session_id);
