CREATE TABLE IF NOT EXISTS moa.experiment_trial (
    trial_uid UUID PRIMARY KEY,
    run_uid UUID NOT NULL REFERENCES moa.experiment_run(run_uid) ON DELETE CASCADE,
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    trial_key TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('accepted', 'running', 'waiting_approval', 'completed', 'failed', 'cancelled')),
    target_kind TEXT NOT NULL CHECK (target_kind IN ('agent_loop', 'workflow')),
    variant_key TEXT NOT NULL,
    plan_revision_uid UUID NOT NULL,
    persona_id TEXT,
    profile_id TEXT,
    scenario_id TEXT,
    data_bundle_ids TEXT[] NOT NULL DEFAULT '{}',
    artifact_revision_uids UUID[] NOT NULL DEFAULT '{}',
    simulator JSONB NOT NULL,
    simulator_model TEXT NOT NULL,
    target_model TEXT,
    seed TEXT,
    session_id UUID REFERENCES sessions(id) ON DELETE SET NULL,
    workflow_run_uid UUID REFERENCES moa.artifact_run(run_uid) ON DELETE SET NULL,
    score_run_id UUID NOT NULL REFERENCES analytics.score_run(run_id) ON DELETE RESTRICT,
    turn_count INT NOT NULL DEFAULT 0,
    stop_reason TEXT CHECK (
        stop_reason IS NULL OR stop_reason IN (
            'success',
            'failure',
            'max_turns',
            'budget_cap',
            'simulator_done',
            'target_terminal',
            'approval_wait',
            'error',
            'cancelled'
        )
    ),
    error TEXT,
    trace_id TEXT,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS experiment_trial_scope_run_status_idx
    ON moa.experiment_trial (workspace_id, scope, user_id, run_uid, status, created_at DESC);

CREATE UNIQUE INDEX IF NOT EXISTS experiment_trial_run_key_uniq
    ON moa.experiment_trial (run_uid, trial_key);

CREATE INDEX IF NOT EXISTS experiment_trial_score_run_idx
    ON moa.experiment_trial (score_run_id);

CREATE INDEX IF NOT EXISTS experiment_trial_session_idx
    ON moa.experiment_trial (session_id)
    WHERE session_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS experiment_trial_workflow_run_idx
    ON moa.experiment_trial (workflow_run_uid)
    WHERE workflow_run_uid IS NOT NULL;

SELECT moa.apply_three_tier_rls('moa.experiment_trial'::REGCLASS);
