-- Durable hand lease state for cross-pod sandbox reuse and cleanup.

CREATE TABLE IF NOT EXISTS moa.hand_leases (
    session_id UUID NOT NULL,
    worker_id TEXT NOT NULL DEFAULT '',
    tenant_id UUID NOT NULL,
    provider TEXT NOT NULL,
    tier TEXT NOT NULL CHECK (tier IN ('none', 'container', 'microvm', 'local')),
    handle JSONB,
    status TEXT NOT NULL CHECK (
        status IN ('provisioning', 'active', 'stale', 'destroyed', 'failed', 'reaping')
    ),
    generation BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    idle_expires_at TIMESTAMPTZ,
    hard_expires_at TIMESTAMPTZ,
    profile JSONB,
    profile_hash TEXT,
    source_deployment_revision TEXT,
    source_tenant_revision TEXT,
    source_agent_revision TEXT,
    source_route_revision TEXT,
    source_origin_revision TEXT,
    capability_revision TEXT,
    reap_attempts INTEGER NOT NULL DEFAULT 0,
    reap_not_before TIMESTAMPTZ,
    reap_claim_token UUID,
    reap_claim_expires_at TIMESTAMPTZ,
    PRIMARY KEY (session_id, worker_id, provider),
    CONSTRAINT hand_leases_policy_identity_check CHECK (
        status NOT IN ('active', 'provisioning')
        OR (
            profile IS NOT NULL
            AND profile_hash IS NOT NULL
            AND source_deployment_revision IS NOT NULL
            AND source_tenant_revision IS NOT NULL
            AND source_agent_revision IS NOT NULL
            AND source_route_revision IS NOT NULL
            AND source_origin_revision IS NOT NULL
            AND capability_revision IS NOT NULL
        )
    ),
    CONSTRAINT hand_leases_idle_within_hard_check CHECK (
        idle_expires_at IS NULL
        OR hard_expires_at IS NULL
        OR idle_expires_at <= hard_expires_at
    ),
    CONSTRAINT hand_leases_reap_claim_pair_check CHECK (
        (reap_claim_token IS NULL) = (reap_claim_expires_at IS NULL)
    ),
    CONSTRAINT hand_leases_reaping_claim_check CHECK (
        (status = 'reaping')
        = (reap_claim_token IS NOT NULL AND reap_claim_expires_at IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_hand_leases_tenant_session
    ON moa.hand_leases (tenant_id, session_id);

CREATE INDEX IF NOT EXISTS idx_hand_leases_reaper
    ON moa.hand_leases (status, reap_not_before, hard_expires_at, idle_expires_at);
