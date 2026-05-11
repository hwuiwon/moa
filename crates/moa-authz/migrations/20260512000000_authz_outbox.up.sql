-- Transactional outbox for OpenFGA tuple writes.
--
-- A handler that mutates Postgres state queues tuple operations into this
-- table inside the same transaction. The outbox poller claims pending rows
-- and applies them to OpenFGA. Idempotency keys prevent duplicate rows for
-- the same desired tuple state.

CREATE TABLE authz_outbox (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    idempotency_key TEXT        NOT NULL UNIQUE,
    op              TEXT        NOT NULL CHECK (op IN ('write', 'delete')),
    tuple_user      TEXT        NOT NULL,
    tuple_relation  TEXT        NOT NULL,
    tuple_object    TEXT        NOT NULL,
    model_version   INTEGER     NOT NULL,
    status          TEXT        NOT NULL DEFAULT 'pending'
                                CHECK (status IN ('pending', 'in_flight', 'succeeded', 'dead_letter')),
    attempts        INTEGER     NOT NULL DEFAULT 0,
    last_error      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    tenant_id       UUID
);

CREATE INDEX idx_authz_outbox_pending
    ON authz_outbox(next_attempt_at)
    WHERE status = 'pending';

CREATE INDEX idx_authz_outbox_dead_letter
    ON authz_outbox(updated_at DESC)
    WHERE status = 'dead_letter';

CREATE INDEX idx_authz_outbox_tenant
    ON authz_outbox(tenant_id, status)
    WHERE tenant_id IS NOT NULL;
