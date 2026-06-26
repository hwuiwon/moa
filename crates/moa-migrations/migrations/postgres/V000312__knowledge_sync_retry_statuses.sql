-- Allow retry-safe tenant knowledge sync status labels.

ALTER TABLE moa.knowledge_sync_runs
    DROP CONSTRAINT IF EXISTS knowledge_sync_runs_status_check;

ALTER TABLE moa.knowledge_sync_runs
    ADD CONSTRAINT knowledge_sync_runs_status_check
    CHECK (status IN (
        'queued',
        'provider_syncing',
        'provider_synced',
        'parse_pending',
        'ingesting',
        'completed',
        'failed_retryable',
        'failed_terminal',
        'canceled',
        'pending',
        'running',
        'partial_failure',
        'failed'
    ));
