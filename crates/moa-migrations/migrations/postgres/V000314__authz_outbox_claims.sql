-- Lease fencing for auth background work that may be picked up by any pod.

ALTER TABLE authz_outbox
    ADD COLUMN IF NOT EXISTS lease_token UUID,
    ADD COLUMN IF NOT EXISTS lease_expires_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_authz_outbox_claimable
    ON authz_outbox(status, next_attempt_at, lease_expires_at)
    WHERE status IN ('pending', 'in_flight');

ALTER TABLE builtin_pending_approvals
    ADD COLUMN IF NOT EXISTS resolve_claim_token UUID,
    ADD COLUMN IF NOT EXISTS resolve_claim_expires_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS idx_builtin_approvals_resolution_claim
    ON builtin_pending_approvals(resolve_claim_expires_at, decided_at, expires_at)
    WHERE status IN ('approved', 'denied', 'timeout') AND resolved_at IS NULL;

CREATE TABLE IF NOT EXISTS auth0_ciba_approvals (
    id                  UUID        PRIMARY KEY,
    session_id          UUID        NOT NULL,
    deciding_user_id    UUID        NOT NULL,
    awakeable_id        TEXT        NOT NULL UNIQUE,
    auth_req_id         TEXT        NOT NULL UNIQUE,
    status              TEXT        NOT NULL DEFAULT 'pending'
                                      CHECK (status IN ('pending', 'approved', 'denied', 'timeout')),
    deny_reason         TEXT,
    poll_interval_ms    INTEGER     NOT NULL,
    next_poll_at        TIMESTAMPTZ NOT NULL,
    expires_at          TIMESTAMPTZ NOT NULL,
    resolved_at         TIMESTAMPTZ,
    lease_token         UUID,
    lease_expires_at    TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_auth0_ciba_claimable
    ON auth0_ciba_approvals(status, next_poll_at, lease_expires_at)
    WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_auth0_ciba_unresolved_terminal
    ON auth0_ciba_approvals(lease_expires_at, updated_at)
    WHERE status IN ('approved', 'denied', 'timeout') AND resolved_at IS NULL;
