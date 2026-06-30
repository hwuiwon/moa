DROP TABLE IF EXISTS approval_rules;

CREATE TABLE IF NOT EXISTS action_policy_rules (
    id UUID PRIMARY KEY,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    tool TEXT NOT NULL,
    pattern TEXT NOT NULL,
    effect TEXT NOT NULL CHECK (effect IN ('allow', 'deny', 'admin_review')),
    scope TEXT NOT NULL CHECK (scope IN ('tenant')),
    reason TEXT,
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT action_policy_rules_global_partition_check
        CHECK (
            scope = 'tenant' AND storage_partition_id <> 'global'
        )
);

CREATE INDEX IF NOT EXISTS idx_action_policy_rules_scope
    ON action_policy_rules(storage_partition_id, scope, user_id);
CREATE INDEX IF NOT EXISTS idx_action_policy_rules_lookup
    ON action_policy_rules(storage_partition_id, tool, user_id, created_at);
CREATE UNIQUE INDEX IF NOT EXISTS idx_action_policy_rules_unique_scope
    ON action_policy_rules(storage_partition_id, tool, pattern, COALESCE(user_id, ''));

SELECT moa.apply_three_tier_rls('action_policy_rules'::REGCLASS);

CREATE TABLE IF NOT EXISTS tenant_action_reviews (
    id UUID PRIMARY KEY,
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
        CHECK (status IN ('pending', 'cleared', 'denied')),
    requested_by TEXT NOT NULL,
    requested_event_recorded_at TIMESTAMPTZ,
    decided_by TEXT,
    deny_reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    decided_at TIMESTAMPTZ,
    decision_event_recorded_at TIMESTAMPTZ,
    execution_tool_call_id UUID,
    execution_requested_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_tenant_action_reviews_pending
    ON tenant_action_reviews(storage_partition_id, created_at DESC)
    WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_tenant_action_reviews_session
    ON tenant_action_reviews(session_id, created_at DESC)
    WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_tenant_action_reviews_scope
    ON tenant_action_reviews(storage_partition_id, scope, user_id);

SELECT moa.apply_three_tier_rls('tenant_action_reviews'::REGCLASS);

ALTER TABLE moa.artifact_run
    DROP CONSTRAINT IF EXISTS artifact_run_status_check;
ALTER TABLE moa.artifact_run
    ADD CONSTRAINT artifact_run_status_check
    CHECK (status IN ('queued', 'running', 'pending_review', 'completed', 'failed', 'cancelled'));

ALTER TABLE moa.artifact_node_run
    DROP CONSTRAINT IF EXISTS artifact_node_run_status_check;
ALTER TABLE moa.artifact_node_run
    ADD CONSTRAINT artifact_node_run_status_check
    CHECK (status IN ('queued', 'running', 'pending_review', 'completed', 'failed', 'cancelled', 'skipped'));

ALTER TABLE moa.experiment_run
    DROP CONSTRAINT IF EXISTS experiment_run_status_check;
ALTER TABLE moa.experiment_run
    ADD CONSTRAINT experiment_run_status_check
    CHECK (status IN ('accepted', 'running', 'completed', 'failed', 'cancelled'));

ALTER TABLE moa.experiment_trial
    DROP CONSTRAINT IF EXISTS experiment_trial_status_check;
ALTER TABLE moa.experiment_trial
    ADD CONSTRAINT experiment_trial_status_check
    CHECK (status IN ('accepted', 'dispatched', 'running', 'completed', 'failed', 'cancelled'));

ALTER TABLE moa.experiment_trial
    DROP CONSTRAINT IF EXISTS experiment_trial_stop_reason_check;
ALTER TABLE moa.experiment_trial
    ADD CONSTRAINT experiment_trial_stop_reason_check
    CHECK (
        stop_reason IS NULL OR stop_reason IN (
            'success',
            'failure',
            'max_turns',
            'budget_cap',
            'simulator_done',
            'target_terminal',
            'error',
            'cancelled'
        )
    );
