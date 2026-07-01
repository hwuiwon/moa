-- Track whether a terminal builtin approval decision has been delivered to Restate.
--
-- The decision row is product-visible state, while the awakeable resolution is
-- an orchestration side effect. Keeping a marker lets retries and the timeout
-- reaper safely resume delivery after a crash between those two operations.

ALTER TABLE builtin_pending_approvals
    ADD COLUMN IF NOT EXISTS resolved_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_builtin_approvals_unresolved_terminal
    ON builtin_pending_approvals(decided_at, expires_at)
    WHERE status IN ('approved', 'denied', 'timeout') AND resolved_at IS NULL;
