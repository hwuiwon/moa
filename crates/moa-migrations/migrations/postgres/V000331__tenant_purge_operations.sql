-- Durable idempotency fence for destructive tenant offboarding.
--
-- The row is inserted and advanced to `relationally_committed` in the same
-- transaction as product-row deletion and inverse OpenFGA outbox writes. A
-- Restate retry after PostgreSQL committed but before the run result was
-- journaled therefore observes the committed fence and skips relational work.
-- This control-plane record intentionally contains no tenant profile or
-- credential data.

CREATE TABLE IF NOT EXISTS moa.tenant_purge_operations (
    tenant_id UUID PRIMARY KEY,
    operation_id TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'relationally_committed')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    relationally_committed_at TIMESTAMPTZ
);

GRANT SELECT, INSERT, UPDATE ON moa.tenant_purge_operations TO moa_app, moa_promoter;
GRANT SELECT ON moa.tenant_purge_operations TO moa_auditor;
