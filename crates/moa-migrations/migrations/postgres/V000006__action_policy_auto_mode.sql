CREATE TABLE IF NOT EXISTS action_policy_rules (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    tool TEXT NOT NULL,
    pattern TEXT NOT NULL,
    effect TEXT NOT NULL CHECK (effect IN ('allow', 'deny', 'admin_review')),
    scope TEXT NOT NULL CHECK (scope IN ('tenant', 'contact')),
    reason TEXT,
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT action_policy_rules_storage_partition_check
        CHECK (storage_partition_id <> 'global'),
    CONSTRAINT action_policy_rules_scope_identity_check CHECK (
        (scope = 'tenant' AND user_id IS NULL)
        OR (scope = 'contact' AND user_id IS NOT NULL)
    )
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_action_policy_rules_unique_scope
    ON action_policy_rules(storage_partition_id, tool, pattern, COALESCE(user_id, ''));
CREATE INDEX IF NOT EXISTS action_policy_rules_tenant_rls_idx
    ON action_policy_rules(tenant_id, tool, created_at);

CREATE TABLE IF NOT EXISTS tenant_action_reviews (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    session_id UUID REFERENCES sessions(id) ON DELETE SET NULL,
    worker_id TEXT,
    tool_call_id UUID NOT NULL,
    tool_name TEXT NOT NULL,
    action_class TEXT NOT NULL,
    risk_level TEXT NOT NULL,
    input_summary TEXT NOT NULL,
    normalized_input TEXT NOT NULL,
    envelope JSONB NOT NULL,
    preview JSONB NOT NULL,
    tool_request JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'cleared', 'denied', 'timeout')),
    requested_by TEXT NOT NULL,
    decided_by TEXT,
    deny_reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- When a pending review fails closed if still undecided. Set by the service
    -- at insert time from `async_authz.action_review_timeout_secs`; the
    -- action-review reaper transitions expired pending rows to `timeout`.
    expires_at TIMESTAMPTZ NOT NULL DEFAULT NOW() + INTERVAL '1 day',
    decided_at TIMESTAMPTZ,
    execution_tool_call_id UUID,
    execution_requested_at TIMESTAMPTZ,
    owner_registered_at TIMESTAMPTZ,
    owner_release_delivered_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_tenant_action_reviews_pending
    ON tenant_action_reviews(storage_partition_id, created_at DESC)
    WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_tenant_action_reviews_session
    ON tenant_action_reviews(session_id, created_at DESC)
    WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS tenant_action_reviews_tenant_rls_idx
    ON tenant_action_reviews(tenant_id, created_at DESC);

-- Drives the action-review reaper timeout sweep: the oldest expired pending
-- rows are read expires_at-first.
CREATE INDEX IF NOT EXISTS idx_tenant_action_reviews_expiry
    ON tenant_action_reviews(expires_at)
    WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_tenant_action_reviews_owner_release
    ON tenant_action_reviews(created_at, id)
    WHERE status = 'timeout'
      AND owner_registered_at IS NOT NULL
      AND owner_release_delivered_at IS NULL;
