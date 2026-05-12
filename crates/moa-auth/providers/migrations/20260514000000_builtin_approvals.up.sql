-- Pending approval requests for the BuiltinAsyncAuthzProvider.
--
-- A handler writes a row here and waits on the Restate awakeable id. The
-- approval decision handler updates this row and resolves the awakeable.

CREATE TABLE builtin_pending_approvals (
    id                  UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id          UUID        NOT NULL,
    deciding_user_id    UUID        NOT NULL,
    tenant_id           UUID        NOT NULL,
    awakeable_id        TEXT        NOT NULL UNIQUE,
    action_summary      TEXT        NOT NULL,
    action_details      JSONB       NOT NULL,
    status              TEXT        NOT NULL DEFAULT 'pending'
                                        CHECK (status IN ('pending', 'approved', 'denied', 'timeout')),
    deny_reason         TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at          TIMESTAMPTZ NOT NULL,
    decided_at          TIMESTAMPTZ,
    decided_by_user_id  UUID
);

CREATE INDEX idx_builtin_approvals_pending
    ON builtin_pending_approvals(deciding_user_id, created_at DESC)
    WHERE status = 'pending';

CREATE INDEX idx_builtin_approvals_session
    ON builtin_pending_approvals(session_id);

CREATE INDEX idx_builtin_approvals_expires
    ON builtin_pending_approvals(expires_at)
    WHERE status = 'pending';
