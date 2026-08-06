-- Replay-stable slow-path ingestion reports.

ALTER TABLE moa.ingest_dedup
    ADD COLUMN apply_outcome TEXT;

-- A pre-V56 row only proved that the effect had already committed. Preserve
-- its established retry behavior as skipped; V56 writers persist the exact
-- first outcome atomically with every new effect.
UPDATE moa.ingest_dedup
SET apply_outcome = 'skipped'
WHERE apply_outcome IS NULL;

ALTER TABLE moa.ingest_dedup
    ALTER COLUMN apply_outcome SET NOT NULL,
    ADD CONSTRAINT ingest_dedup_apply_outcome_check CHECK (
        apply_outcome IN ('inserted', 'superseded', 'reinforced', 'skipped')
    );
