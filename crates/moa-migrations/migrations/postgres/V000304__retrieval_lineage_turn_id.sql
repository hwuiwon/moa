-- Link retrieval-quality sidecar rows to turn-scoped lineage records.

ALTER TABLE moa.retrieval_lineage
    ADD COLUMN IF NOT EXISTS turn_id UUID;

CREATE INDEX IF NOT EXISTS retrieval_lineage_turn_id
    ON moa.retrieval_lineage (turn_id)
    WHERE turn_id IS NOT NULL;
