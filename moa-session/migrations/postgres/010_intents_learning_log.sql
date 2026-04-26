CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS tenant_intents (
    id UUID PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    label TEXT NOT NULL,
    description TEXT,
    status TEXT NOT NULL DEFAULT 'proposed',
    source TEXT NOT NULL DEFAULT 'discovered',
    catalog_ref UUID,
    example_queries TEXT[] NOT NULL DEFAULT '{}',
    embedding vector(1536),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deprecated_at TIMESTAMPTZ,
    segment_count INT NOT NULL DEFAULT 0,
    resolution_rate NUMERIC(4,3),
    UNIQUE(tenant_id, label)
);

CREATE INDEX IF NOT EXISTS idx_tenant_intents_tenant
    ON tenant_intents (tenant_id, status);
CREATE INDEX IF NOT EXISTS idx_tenant_intents_scope
    ON tenant_intents (workspace_id, scope, user_id);

CREATE INDEX IF NOT EXISTS idx_tenant_intents_embedding_hnsw
    ON tenant_intents USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);

CREATE TABLE IF NOT EXISTS global_intent_catalog (
    id UUID PRIMARY KEY,
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    label TEXT NOT NULL UNIQUE,
    description TEXT NOT NULL,
    category TEXT,
    example_queries TEXT[] NOT NULL,
    embedding vector(1536),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_global_intent_catalog_category
    ON global_intent_catalog (category, label);
CREATE INDEX IF NOT EXISTS idx_global_intent_catalog_scope
    ON global_intent_catalog (workspace_id, scope, user_id);

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
