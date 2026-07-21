-- One durable signed evidence row per logical retrieval. The caller supplies a
-- replay-stable operation id; uniqueness is database-owned across replicas.
ALTER TABLE security_events
    ADD COLUMN IF NOT EXISTS retrieval_operation_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS security_events_retrieval_operation_uniq
    ON security_events (tenant_id, retrieval_operation_id)
    WHERE retrieval_operation_id IS NOT NULL;
