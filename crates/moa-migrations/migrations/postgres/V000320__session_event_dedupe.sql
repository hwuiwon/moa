-- Idempotent session-event append support.
--
-- Retried Restate handlers (control-plane signals, the heartbeat watchdog, and
-- progress narration) re-issue the *same* logical append after a partial failure.
-- To make those appends idempotent without weakening the append-only `events`
-- table, dedupe state lives in this SEPARATE table keyed by
-- `(session_id, dedupe_key)`. The primary key doubles as the uniqueness guard.
--
-- Why not a column + partial unique index on `events`: adding a unique index to
-- the large, trigger-heavy, append-only `events` table requires a non-concurrent
-- `CREATE UNIQUE INDEX` (refinery runs each migration inside a transaction, so
-- `CREATE INDEX CONCURRENTLY` is unavailable), which takes a write-blocking lock
-- and stalls writes during deploy. A fresh empty table with a PK has none of that
-- risk and never touches the hot path.
--
-- emit_event_record consults/inserts this table under the same
-- `sessions ... FOR UPDATE` lock and transaction that guards the matching event
-- insert: on `Some(key)` it `SELECT sequence_num`; if a row exists it returns
-- that sequence without inserting a second event; otherwise it inserts the event
-- and the dedupe row together.
--
-- RLS / tenant-scoping note: this is an internal idempotency guard, never read by
-- product surfaces. It is written only inside emit_event_record under the
-- already-RLS-scoped `sessions` row lock, and every row is reachable only through
-- its owning `session_id` (itself tenant-scoped via `sessions`). It therefore
-- carries no independent tenant columns. If stricter per-tenant isolation is later
-- required, mirror `V000316__session_blobs.sql`: add a `tenant_id` column, a
-- BEFORE INSERT trigger that derives it from the session, and
-- `moa.apply_tenant_rls(...)`.

CREATE TABLE IF NOT EXISTS session_event_dedupe (
    session_id   UUID NOT NULL,
    dedupe_key   TEXT NOT NULL,
    sequence_num BIGINT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (session_id, dedupe_key)
);

-- The runtime application role appends events through this table; grant it the
-- minimal privileges it needs (matching the grants `events` receives).
GRANT SELECT, INSERT ON TABLE session_event_dedupe TO moa_app;
GRANT SELECT, INSERT ON TABLE session_event_dedupe TO moa_promoter;
