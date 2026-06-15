CREATE TABLE IF NOT EXISTS experience_records (
    id UUID PRIMARY KEY,
    segment_id UUID NOT NULL REFERENCES task_segments(id) ON DELETE CASCADE,
    session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    tenant_id TEXT NOT NULL,
    task_summary TEXT,
    task_fingerprint TEXT NOT NULL,
    task_fingerprint_payload JSONB NOT NULL,
    task_facets JSONB NOT NULL,
    actions TEXT[] NOT NULL DEFAULT '{}',
    resources JSONB NOT NULL DEFAULT '[]'::JSONB,
    outcome TEXT NOT NULL,
    confidence NUMERIC(4,3) NOT NULL,
    evidence JSONB NOT NULL DEFAULT '[]'::JSONB,
    tools_used TEXT[] NOT NULL DEFAULT '{}',
    skills_activated TEXT[] NOT NULL DEFAULT '{}',
    turn_count INT NOT NULL DEFAULT 0,
    token_cost BIGINT NOT NULL DEFAULT 0,
    duration_ms BIGINT,
    assessment_policy_version TEXT NOT NULL,
    extraction_policy_version TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(segment_id, extraction_policy_version)
);

CREATE INDEX IF NOT EXISTS idx_experience_records_session
    ON experience_records (session_id, created_at);
CREATE INDEX IF NOT EXISTS idx_experience_records_tenant_task
    ON experience_records (tenant_id, task_fingerprint, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_experience_records_scope
    ON experience_records (workspace_id, scope, user_id);

CREATE TABLE IF NOT EXISTS experience_attributions (
    id UUID PRIMARY KEY,
    experience_id UUID NOT NULL REFERENCES experience_records(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    subject_type TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    effect TEXT NOT NULL,
    confidence NUMERIC(4,3) NOT NULL,
    evidence JSONB NOT NULL DEFAULT '[]'::JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(experience_id, subject_type, subject_id)
);

CREATE INDEX IF NOT EXISTS idx_experience_attributions_experience
    ON experience_attributions (experience_id, subject_type);
CREATE INDEX IF NOT EXISTS idx_experience_attributions_subject
    ON experience_attributions (tenant_id, subject_type, subject_id);
CREATE INDEX IF NOT EXISTS idx_experience_attributions_scope
    ON experience_attributions (workspace_id, scope, user_id);

CREATE TABLE IF NOT EXISTS learning_candidates (
    id UUID PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    candidate_type TEXT NOT NULL,
    status TEXT NOT NULL,
    target_id TEXT,
    target_label TEXT,
    task_fingerprint TEXT,
    task_fingerprint_payload JSONB,
    task_facets JSONB,
    payload JSONB NOT NULL,
    evaluation_payload JSONB,
    source_experience_ids UUID[] NOT NULL DEFAULT '{}',
    confidence NUMERIC(4,3),
    risk_class TEXT NOT NULL,
    promotion_requirements TEXT[] NOT NULL DEFAULT '{}',
    status_reason TEXT,
    batch_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_learning_candidates_tenant_status
    ON learning_candidates (tenant_id, status, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_learning_candidates_task
    ON learning_candidates (tenant_id, task_fingerprint);
CREATE INDEX IF NOT EXISTS idx_learning_candidates_batch
    ON learning_candidates (batch_id) WHERE batch_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_learning_candidates_scope
    ON learning_candidates (workspace_id, scope, user_id);

DROP MATERIALIZED VIEW IF EXISTS task_strategy_success_rates;

CREATE MATERIALIZED VIEW task_strategy_success_rates AS
SELECT
    e.tenant_id,
    e.task_fingerprint,
    a.subject_type,
    a.subject_id,
    COUNT(*)::BIGINT AS uses,
    AVG(CASE WHEN e.outcome = 'resolved' THEN 1.0
             WHEN e.outcome = 'partial' THEN 0.5
             ELSE 0.0 END)::DOUBLE PRECISION AS success_rate,
    AVG(e.confidence)::DOUBLE PRECISION AS avg_confidence,
    AVG(e.token_cost)::DOUBLE PRECISION AS avg_token_cost,
    AVG(e.turn_count)::DOUBLE PRECISION AS avg_turn_count
FROM experience_records e
JOIN experience_attributions a ON a.experience_id = e.id
WHERE a.subject_type IN ('skill', 'tool', 'memory', 'verification', 'policy')
GROUP BY e.tenant_id, e.task_fingerprint, a.subject_type, a.subject_id;

CREATE UNIQUE INDEX IF NOT EXISTS idx_task_strategy_success_rates_unique
    ON task_strategy_success_rates(tenant_id, task_fingerprint, subject_type, subject_id);

SELECT moa.apply_three_tier_rls('experience_records'::REGCLASS);
SELECT moa.apply_three_tier_rls('experience_attributions'::REGCLASS);
SELECT moa.apply_three_tier_rls('learning_candidates'::REGCLASS);
