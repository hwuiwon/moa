-- Durable queue for syncing committed pgvector writes into external vector backends.

CREATE TABLE IF NOT EXISTS moa.vector_sync_outbox (
    sync_id BIGSERIAL PRIMARY KEY,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    uid UUID NOT NULL,
    op TEXT NOT NULL CHECK (op IN ('upsert', 'delete')),
    attempts INT NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    claim_token UUID,
    claim_expires_at TIMESTAMPTZ,
    processing_started_at TIMESTAMPTZ,
    available_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    processed_at TIMESTAMPTZ,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (user_id IS NULL),
    CHECK (scope = 'tenant')
);

CREATE INDEX IF NOT EXISTS vector_sync_outbox_pending_idx
    ON moa.vector_sync_outbox (available_at, sync_id)
    WHERE processed_at IS NULL;

CREATE INDEX IF NOT EXISTS vector_sync_outbox_partition_pending_idx
    ON moa.vector_sync_outbox (storage_partition_id, available_at, sync_id)
    WHERE processed_at IS NULL;

CREATE INDEX IF NOT EXISTS vector_sync_outbox_partition_uid_idx
    ON moa.vector_sync_outbox (storage_partition_id, uid, sync_id);

SELECT moa.apply_three_tier_rls('moa.vector_sync_outbox'::REGCLASS);

DROP POLICY IF EXISTS vector_sync_outbox_promoter ON moa.vector_sync_outbox;
CREATE POLICY vector_sync_outbox_promoter ON moa.vector_sync_outbox
    FOR ALL TO moa_promoter
    USING (true)
    WITH CHECK (scope = 'tenant' AND user_id IS NULL);

GRANT USAGE, SELECT ON SEQUENCE moa.vector_sync_outbox_sync_id_seq TO moa_app, moa_promoter;
