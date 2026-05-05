CREATE TABLE IF NOT EXISTS context_snapshots (
    session_id UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    format_version INTEGER NOT NULL,
    last_sequence_num BIGINT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_context_snapshots_last_seq
    ON context_snapshots(session_id, last_sequence_num);
CREATE INDEX IF NOT EXISTS idx_context_snapshots_scope
    ON context_snapshots(workspace_id, scope, user_id);
