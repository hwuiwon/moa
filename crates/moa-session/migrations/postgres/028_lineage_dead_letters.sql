-- Dead-letter storage for lineage writer batches that cannot be written after bounded retries.

CREATE TABLE IF NOT EXISTS analytics.lineage_dead_letters (
    dead_letter_id      UUID        PRIMARY KEY,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    error               TEXT        NOT NULL,
    attempts            INTEGER     NOT NULL,
    row_count           INTEGER     NOT NULL,
    first_workspace_id  TEXT,
    first_session_id    UUID,
    first_turn_id       UUID,
    rows                JSONB       NOT NULL
);

CREATE INDEX IF NOT EXISTS lineage_dead_letters_created_idx
    ON analytics.lineage_dead_letters (created_at DESC);
