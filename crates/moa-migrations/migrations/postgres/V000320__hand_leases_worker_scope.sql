-- Add a worker scope dimension to durable hand leases.
--
-- Hands are re-keyed from (session_id, provider) to
-- (session_id, worker_id, provider) so each worker owns its own sandbox
-- lease instead of collapsing onto a single session-shared hand. Existing rows
-- default to worker_id = '' (the session-level / coordinator scope), which is
-- the only scope written today, so (session_id, '', provider) stays unique and
-- the primary-key swap introduces no conflict. Idempotent and append-only-safe.

ALTER TABLE moa.hand_leases
    ADD COLUMN IF NOT EXISTS worker_id TEXT NOT NULL DEFAULT '';

-- V000312 created the primary key unnamed, so Postgres named it hand_leases_pkey.
-- Drop-if-exists then re-add keeps the swap re-runnable.
ALTER TABLE moa.hand_leases DROP CONSTRAINT IF EXISTS hand_leases_pkey;
ALTER TABLE moa.hand_leases
    ADD CONSTRAINT hand_leases_pkey PRIMARY KEY (session_id, worker_id, provider);

CREATE INDEX IF NOT EXISTS idx_hand_leases_session_worker
    ON moa.hand_leases (session_id, worker_id);
