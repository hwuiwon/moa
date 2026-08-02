-- Idempotency claims are kept separate from the append-only events table so
-- retries do not add a uniqueness check to the hottest write target.
CREATE TABLE IF NOT EXISTS session_event_dedupe (
    session_id UUID NOT NULL,
    dedupe_key TEXT NOT NULL,
    sequence_num BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (session_id, dedupe_key)
);

GRANT SELECT, INSERT ON TABLE session_event_dedupe TO moa_app;
GRANT SELECT, INSERT ON TABLE session_event_dedupe TO moa_promoter;
