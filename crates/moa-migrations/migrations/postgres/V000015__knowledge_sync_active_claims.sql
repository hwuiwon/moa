-- Atomic claims for replicated knowledge sync and object ingestion workers.

CREATE UNIQUE INDEX IF NOT EXISTS knowledge_sync_runs_active_claim_uniq
    ON moa.knowledge_sync_runs (tenant_id, connection_id)
    WHERE status IN (
        'queued',
        'provider_syncing',
        'provider_synced',
        'parse_pending',
        'ingesting'
    );

CREATE TABLE IF NOT EXISTS moa.knowledge_object_ingestion_claims (
    tenant_id UUID NOT NULL,
    storage_partition_id TEXT NOT NULL,
    object_id UUID NOT NULL REFERENCES moa.knowledge_objects(object_uid) ON DELETE CASCADE,
    content_hash TEXT NOT NULL,
    document_version_id UUID NOT NULL REFERENCES moa.knowledge_document_versions(document_version_uid) ON DELETE CASCADE,
    claimed_by_sync_run_id UUID NOT NULL REFERENCES moa.knowledge_sync_runs(sync_run_uid) ON DELETE CASCADE,
    completed_by_sync_run_id UUID REFERENCES moa.knowledge_sync_runs(sync_run_uid) ON DELETE SET NULL,
    status TEXT NOT NULL DEFAULT 'started',
    claim_token UUID NOT NULL DEFAULT gen_random_uuid(),
    claimed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    lease_expires_at TIMESTAMPTZ NOT NULL DEFAULT (now() + INTERVAL '15 minutes'),
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, object_id, content_hash),
    CHECK (content_hash <> ''),
    CHECK (status IN ('started', 'completed', 'failed'))
);

CREATE INDEX IF NOT EXISTS knowledge_object_ingestion_claims_version_idx
    ON moa.knowledge_object_ingestion_claims (document_version_id);

CREATE INDEX IF NOT EXISTS knowledge_object_ingestion_claims_status_idx
    ON moa.knowledge_object_ingestion_claims (tenant_id, status, updated_at DESC);

CREATE INDEX IF NOT EXISTS knowledge_object_ingestion_claims_started_lease_idx
    ON moa.knowledge_object_ingestion_claims (tenant_id, lease_expires_at)
    WHERE status = 'started';

DROP TRIGGER IF EXISTS knowledge_object_ingestion_claims_set_tenant_columns
    ON moa.knowledge_object_ingestion_claims;
CREATE TRIGGER knowledge_object_ingestion_claims_set_tenant_columns
    BEFORE INSERT OR UPDATE ON moa.knowledge_object_ingestion_claims
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_runtime_tenant_columns();

SELECT moa.apply_tenant_rls('moa.knowledge_object_ingestion_claims'::REGCLASS);
