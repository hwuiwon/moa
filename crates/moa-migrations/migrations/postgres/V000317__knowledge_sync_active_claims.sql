-- Atomic claims for replicated knowledge sync and object ingestion workers.

WITH ranked_active_runs AS (
    SELECT
        sync_run_uid,
        row_number() OVER (
            PARTITION BY tenant_id, connection_id
            ORDER BY started_at DESC, sync_run_uid DESC
        ) AS active_rank
    FROM moa.knowledge_sync_runs
    WHERE status IN (
        'queued',
        'provider_syncing',
        'provider_synced',
        'parse_pending',
        'ingesting'
    )
)
UPDATE moa.knowledge_sync_runs run
SET status = 'canceled',
    finished_at = COALESCE(run.finished_at, now()),
    error = COALESCE(
        run.error,
        jsonb_build_object('code', 'active_claim_superseded')
    ),
    updated_at = now()
FROM ranked_active_runs ranked
WHERE run.sync_run_uid = ranked.sync_run_uid
  AND ranked.active_rank > 1;

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

ALTER TABLE moa.knowledge_object_ingestion_claims
    ADD COLUMN IF NOT EXISTS claim_token UUID DEFAULT gen_random_uuid();
ALTER TABLE moa.knowledge_object_ingestion_claims
    ADD COLUMN IF NOT EXISTS lease_expires_at TIMESTAMPTZ DEFAULT (now() + INTERVAL '15 minutes');

UPDATE moa.knowledge_object_ingestion_claims
SET claim_token = gen_random_uuid()
WHERE claim_token IS NULL;

UPDATE moa.knowledge_object_ingestion_claims
SET lease_expires_at = COALESCE(updated_at, claimed_at, now()) + INTERVAL '15 minutes'
WHERE lease_expires_at IS NULL;

ALTER TABLE moa.knowledge_object_ingestion_claims
    ALTER COLUMN claim_token SET NOT NULL;
ALTER TABLE moa.knowledge_object_ingestion_claims
    ALTER COLUMN lease_expires_at SET NOT NULL;

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
