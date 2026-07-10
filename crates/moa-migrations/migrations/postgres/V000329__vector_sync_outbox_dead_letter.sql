-- Quarantine poison vector-sync jobs so permanent failures stop retrying forever.
--
-- A row is dead-lettered when it hits a permanent (config/schema/dimension/auth)
-- failure or exhausts its transient-retry budget. Dead-lettered rows keep
-- `processed_at` NULL but are excluded from the claim predicate by
-- `dead_lettered_at IS NOT NULL`, and an operator redrive resets them to pending.

ALTER TABLE moa.vector_sync_outbox
    ADD COLUMN IF NOT EXISTS dead_lettered_at TIMESTAMPTZ;

-- The pending indexes must exclude dead-lettered rows so the drainer's claim
-- scan never revisits quarantined work.
DROP INDEX IF EXISTS moa.vector_sync_outbox_pending_idx;
CREATE INDEX IF NOT EXISTS vector_sync_outbox_pending_idx
    ON moa.vector_sync_outbox (available_at, sync_id)
    WHERE processed_at IS NULL AND dead_lettered_at IS NULL;

DROP INDEX IF EXISTS moa.vector_sync_outbox_partition_pending_idx;
CREATE INDEX IF NOT EXISTS vector_sync_outbox_partition_pending_idx
    ON moa.vector_sync_outbox (storage_partition_id, available_at, sync_id)
    WHERE processed_at IS NULL AND dead_lettered_at IS NULL;

-- Dedicated index for surfacing and redriving quarantined rows per partition.
CREATE INDEX IF NOT EXISTS vector_sync_outbox_dead_letter_idx
    ON moa.vector_sync_outbox (storage_partition_id, sync_id)
    WHERE dead_lettered_at IS NOT NULL AND processed_at IS NULL;
