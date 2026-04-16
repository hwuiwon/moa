CREATE TABLE IF NOT EXISTS context_snapshots (
    session_id UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    format_version INTEGER NOT NULL,
    last_sequence_num BIGINT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_context_snapshots_last_seq
    ON context_snapshots(session_id, last_sequence_num);
