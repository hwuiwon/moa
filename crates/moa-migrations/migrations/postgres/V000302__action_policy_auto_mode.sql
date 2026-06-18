DROP TABLE IF EXISTS approval_rules;

CREATE TABLE IF NOT EXISTS action_policy_rules (
    id UUID PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    tool TEXT NOT NULL,
    pattern TEXT NOT NULL,
    effect TEXT NOT NULL CHECK (effect IN ('allow', 'deny', 'admin_review')),
    scope TEXT NOT NULL CHECK (scope IN ('global', 'workspace')),
    reason TEXT,
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(workspace_id, tool, pattern)
);

CREATE INDEX IF NOT EXISTS idx_action_policy_rules_scope
    ON action_policy_rules(workspace_id, scope, user_id);

CREATE TABLE IF NOT EXISTS workspace_action_reviews (
    id UUID PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    session_id UUID,
    sub_agent_id TEXT,
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
        CHECK (status IN ('pending', 'cleared', 'denied', 'expired')),
    requested_by TEXT NOT NULL,
    decided_by TEXT,
    deny_reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ,
    decided_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_workspace_action_reviews_pending
    ON workspace_action_reviews(workspace_id, created_at DESC)
    WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_workspace_action_reviews_session
    ON workspace_action_reviews(session_id, created_at DESC)
    WHERE session_id IS NOT NULL;

UPDATE moa.artifact_run SET status = 'running' WHERE status = 'waiting_approval';
UPDATE moa.artifact_node_run SET status = 'running' WHERE status = 'waiting_approval';
UPDATE moa.experiment_run SET status = 'running' WHERE status = 'waiting_approval';
UPDATE moa.experiment_trial
SET status = 'running', stop_reason = NULL
WHERE status = 'waiting_approval' OR stop_reason = 'approval_wait';

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
