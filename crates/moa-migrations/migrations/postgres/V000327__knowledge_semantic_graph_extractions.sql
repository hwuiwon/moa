-- Persistent semantic graph extraction cache for tenant knowledge chunks.

CREATE TABLE IF NOT EXISTS moa.knowledge_semantic_graph_extractions (
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    chunk_hash TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    schema_version TEXT NOT NULL,
    model TEXT NOT NULL,
    prompt_version TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'completed',
    extraction JSONB NOT NULL DEFAULT '{}'::JSONB,
    error_code TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, chunk_hash, schema_version, model, prompt_version),
    CHECK (chunk_hash <> ''),
    CHECK (content_hash <> ''),
    CHECK (schema_version <> ''),
    CHECK (model <> ''),
    CHECK (prompt_version <> ''),
    CHECK (status IN ('completed', 'failed'))
);

CREATE INDEX IF NOT EXISTS knowledge_semantic_graph_extractions_content_idx
    ON moa.knowledge_semantic_graph_extractions (
        tenant_id,
        content_hash,
        schema_version,
        model,
        prompt_version
    );

DROP TRIGGER IF EXISTS knowledge_semantic_graph_extractions_set_tenant_columns
    ON moa.knowledge_semantic_graph_extractions;
CREATE TRIGGER knowledge_semantic_graph_extractions_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.knowledge_semantic_graph_extractions
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

SELECT moa.apply_tenant_rls('moa.knowledge_semantic_graph_extractions'::REGCLASS);
