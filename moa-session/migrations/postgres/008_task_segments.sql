CREATE TABLE IF NOT EXISTS task_segments (
    id UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    tenant_id TEXT NOT NULL,
    segment_index INT NOT NULL,
    intent_label TEXT,
    intent_confidence NUMERIC(4,3),
    task_summary TEXT,
    started_at TIMESTAMPTZ NOT NULL,
    ended_at TIMESTAMPTZ,
    resolution TEXT,
    resolution_signal TEXT,
    resolution_confidence NUMERIC(4,3),
    tools_used TEXT[] NOT NULL DEFAULT '{}',
    skills_activated TEXT[] NOT NULL DEFAULT '{}',
    turn_count INT NOT NULL DEFAULT 0,
    token_cost BIGINT NOT NULL DEFAULT 0,
    previous_segment_id UUID,
    UNIQUE(session_id, segment_index)
);

CREATE INDEX IF NOT EXISTS idx_task_segments_tenant_intent
    ON task_segments (tenant_id, intent_label, resolution);
CREATE INDEX IF NOT EXISTS idx_task_segments_session
    ON task_segments (session_id, segment_index);
CREATE INDEX IF NOT EXISTS idx_task_segments_tenant_time
    ON task_segments (tenant_id, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_task_segments_scope
    ON task_segments (workspace_id, scope, user_id);
