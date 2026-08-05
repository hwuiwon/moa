-- Hard-cut the product session lifecycle label from `paused` to `idle`.
--
-- Restate uses "paused" for an invocation that cannot currently advance. MOA's
-- product status represented the ordinary between-turn state, so `idle` is the
-- exact meaning. This migration intentionally provides no compatibility label.
-- The deployment cutover stops edge admission and drains active Session, turn,
-- and worker invocations before applying this migration.

CREATE TABLE public.deployment_cutover_receipts (
    cutover_name TEXT PRIMARY KEY,
    completed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    sessions_verified BIGINT NOT NULL CHECK (sessions_verified >= 0),
    status_keys_rewritten BIGINT NOT NULL CHECK (status_keys_rewritten >= 0),
    meta_statuses_rewritten BIGINT NOT NULL CHECK (meta_statuses_rewritten >= 0)
);

COMMENT ON TABLE public.deployment_cutover_receipts IS
    'Forward-only deployment gates written after external durable-state rewrites complete.';

REVOKE ALL ON TABLE public.deployment_cutover_receipts FROM PUBLIC;
GRANT SELECT ON TABLE public.deployment_cutover_receipts TO moa_app;

UPDATE sessions
SET status = 'idle'
WHERE status = 'paused';

-- Events are append-only in normal operation. The transaction-local maintenance
-- flag admits only this exact forward rewrite while the migration transaction is
-- active; it cannot leak to a later transaction on the connection.
SELECT pg_catalog.set_config('moa.events_maintenance', 'on', true);

UPDATE events
SET payload =
    CASE
        WHEN payload #>> '{data,from}' = 'paused'
            THEN jsonb_set(payload, '{data,from}', '"idle"'::jsonb, false)
        ELSE payload
    END
WHERE event_type = 'SessionStatusChanged'
  AND payload #>> '{data,from}' = 'paused';

UPDATE events
SET payload =
    CASE
        WHEN payload #>> '{data,to}' = 'paused'
            THEN jsonb_set(payload, '{data,to}', '"idle"'::jsonb, false)
        ELSE payload
    END
WHERE event_type = 'SessionStatusChanged'
  AND payload #>> '{data,to}' = 'paused';

SELECT pg_catalog.set_config('moa.events_maintenance', 'off', true);

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM sessions WHERE status = 'paused') THEN
        RAISE EXCEPTION 'session status migration left paused session rows';
    END IF;
    IF EXISTS (
        SELECT 1
        FROM events
        WHERE event_type = 'SessionStatusChanged'
          AND (
              payload #>> '{data,from}' = 'paused'
              OR payload #>> '{data,to}' = 'paused'
          )
    ) THEN
        RAISE EXCEPTION 'session status migration left paused live event payloads';
    END IF;
END;
$$;
