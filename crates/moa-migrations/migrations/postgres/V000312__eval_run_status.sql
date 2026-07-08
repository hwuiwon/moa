-- Persist hosted eval run status outside Restate workflow state.

CREATE SCHEMA IF NOT EXISTS analytics;

CREATE TABLE IF NOT EXISTS analytics.eval_run_status (
    run_id     UUID        PRIMARY KEY,
    tenant_id  UUID        NOT NULL,
    status     TEXT        NOT NULL CHECK (status IN ('pending', 'running', 'completed', 'failed')),
    request    JSONB       NOT NULL,
    response   JSONB,
    error      TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS ix_eval_run_status_tenant_status_updated
    ON analytics.eval_run_status (tenant_id, status, updated_at DESC);

SELECT moa.apply_tenant_rls('analytics.eval_run_status'::REGCLASS);
