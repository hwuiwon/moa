CREATE TABLE IF NOT EXISTS learning_log (
    id UUID PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    learning_type TEXT NOT NULL,
    target_id TEXT NOT NULL,
    target_label TEXT,
    payload JSONB NOT NULL,
    confidence NUMERIC(4,3),
    source_refs UUID[] NOT NULL DEFAULT '{}',
    actor TEXT NOT NULL DEFAULT 'system',
    valid_from TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    valid_to TIMESTAMPTZ,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    batch_id UUID,
    version INT NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_learning_log_tenant_type
    ON learning_log (tenant_id, learning_type, valid_to);
CREATE INDEX IF NOT EXISTS idx_learning_log_target
    ON learning_log (tenant_id, target_id, valid_from DESC);
CREATE INDEX IF NOT EXISTS idx_learning_log_batch
    ON learning_log (batch_id) WHERE batch_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_learning_log_scope
    ON learning_log (workspace_id, scope, user_id);
