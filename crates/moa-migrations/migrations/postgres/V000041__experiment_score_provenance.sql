-- Typed provenance for Behavior Lab experiment scores.
--
-- Before this migration a Behavior Lab score was a bare `analytics.scores` row:
-- a name, a value, and a free-text `model_or_evaluator`. Nothing recorded which
-- evaluator version produced it, which pinned plan revision and trial it came
-- from, which exact target session or execution run it observed, or what
-- evidence it was derived from. A score row could therefore be seeded by any
-- writer and still satisfy a completeness check, which is exactly the failure
-- this table exists to make impossible.
--
-- One provenance row per score row, keyed by `score_id`:
--
--   * Identity is the score. `score_id` is derived by the trial finalizer from
--     the score run, the evaluator id and exact version, the score name, and the
--     exact target, so two evaluator versions produce two rows rather than one
--     row that overwrites itself.
--
--   * Linkage is enforced by the database, not by the writer. The composite
--     foreign keys below make it impossible to attach a score to a trial from a
--     different tenant, a different experiment run, or a different pinned plan
--     revision, and impossible to attach it to a score run belonging to another
--     tenant.
--
--   * Provenance is immutable. `experiment_score_provenance_no_update` refuses
--     every UPDATE, so replay can only be accepted (identical row, no-op) or
--     refused (different row, primary-key conflict the writer surfaces). There
--     is no path that rewrites a score's provenance after the fact.
--
--   * Evidence is referenced, never reproduced. `evidence_ref` is a bounded
--     pointer at the durable target log and `evidence_hash` is the BLAKE3 digest
--     that binds the score to the exact observations it was derived from. No
--     target output text is stored here.
--
-- Deletion is deliberately NOT cascaded from `moa.experiment_trial`. The tenant
-- purge repository carries an explicit delete for this table, and a cascade
-- would make that step unfalsifiable: removing it would change nothing
-- observable because the trial delete would remove the same rows moments later.

-- Composite uniqueness the provenance foreign keys reference. These are the
-- exact identity tuples a score may attach to; without them the linkage below
-- could only be checked one column at a time, which is not a check at all.
CREATE UNIQUE INDEX IF NOT EXISTS experiment_trial_score_identity_idx
    ON moa.experiment_trial (trial_uid, run_uid, storage_partition_id, plan_revision_uid);

ALTER TABLE moa.experiment_trial
    ADD COLUMN IF NOT EXISTS final_evidence_hash BYTEA;

ALTER TABLE moa.experiment_run
    ADD COLUMN IF NOT EXISTS cancel_signal JSONB;

ALTER TABLE moa.experiment_trial
    DROP CONSTRAINT IF EXISTS experiment_trial_final_evidence_hash_len;
ALTER TABLE moa.experiment_trial
    ADD CONSTRAINT experiment_trial_final_evidence_hash_len CHECK (
        final_evidence_hash IS NULL OR octet_length(final_evidence_hash) = 32
    );

CREATE TABLE IF NOT EXISTS moa.experiment_score_provenance (
    score_id UUID PRIMARY KEY,
    score_ts TIMESTAMPTZ NOT NULL,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    score_run_id UUID NOT NULL,
    experiment_run_uid UUID NOT NULL,
    plan_revision_uid UUID NOT NULL,
    trial_uid UUID NOT NULL,
    target_session_id UUID,
    target_execution_run_uid UUID,
    evaluator_id TEXT NOT NULL,
    evaluator_version TEXT NOT NULL,
    score_name TEXT NOT NULL,
    value_type TEXT NOT NULL CHECK (value_type IN ('numeric', 'boolean', 'categorical')),
    evidence_ref TEXT NOT NULL,
    evidence_hash BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    -- A score observes exactly one target. "Both" and "neither" are equally
    -- unattributable, and an unattributable score must not exist at all.
    CONSTRAINT experiment_score_provenance_one_target CHECK (
        (target_session_id IS NOT NULL)::INT + (target_execution_run_uid IS NOT NULL)::INT = 1
    ),
    -- Bounded by construction: the reference names where the evidence lives.
    CONSTRAINT experiment_score_provenance_ref_bounded CHECK (
        length(evidence_ref) BETWEEN 1 AND 512
    ),
    CONSTRAINT experiment_score_provenance_hash_len CHECK (
        octet_length(evidence_hash) = 32
    ),
    CONSTRAINT experiment_score_provenance_identity_len CHECK (
        length(evaluator_id) BETWEEN 1 AND 128
        AND length(evaluator_version) BETWEEN 1 AND 64
        AND length(score_name) BETWEEN 1 AND 128
    ),
    -- Tenant, run, and pinned plan revision are checked as one tuple: a score
    -- cannot name trial A while claiming run B, plan revision C, or tenant D.
    CONSTRAINT experiment_score_provenance_trial_fkey
        FOREIGN KEY (trial_uid, experiment_run_uid, storage_partition_id, plan_revision_uid)
        REFERENCES moa.experiment_trial (trial_uid, run_uid, storage_partition_id, plan_revision_uid)
        ON DELETE RESTRICT,
    -- The score run must belong to the same tenant as the score.
    CONSTRAINT experiment_score_provenance_score_run_fkey
        FOREIGN KEY (score_run_id, storage_partition_id)
        REFERENCES analytics.score_run (run_id, storage_partition_id)
        ON DELETE RESTRICT,
    -- Bind provenance to the exact append-only score row. `score_id` alone is
    -- not enough because analytics scores are keyed by `(score_id, ts)`.
    CONSTRAINT experiment_score_provenance_score_fkey
        FOREIGN KEY (score_id, score_ts)
        REFERENCES analytics.scores (score_id, ts)
        ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS experiment_score_provenance_score_run_idx
    ON moa.experiment_score_provenance (score_run_id, score_name);

CREATE INDEX IF NOT EXISTS experiment_score_provenance_trial_idx
    ON moa.experiment_score_provenance (trial_uid);

CREATE INDEX IF NOT EXISTS experiment_score_provenance_scope_idx
    ON moa.experiment_score_provenance (storage_partition_id, scope, user_id, experiment_run_uid);

CREATE OR REPLACE FUNCTION moa.experiment_score_provenance_immutable_guard() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION
        'experiment score provenance is immutable (score_id=%, trial=%)',
        OLD.score_id, OLD.trial_uid
        USING ERRCODE = 'P0001';
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION moa.experiment_score_provenance_immutable_guard() IS
    'Refuses UPDATE on moa.experiment_score_provenance so a replayed score can never rewrite its provenance.';

DROP TRIGGER IF EXISTS experiment_score_provenance_no_update
    ON moa.experiment_score_provenance;
CREATE TRIGGER experiment_score_provenance_no_update
    BEFORE UPDATE ON moa.experiment_score_provenance
    FOR EACH ROW EXECUTE FUNCTION moa.experiment_score_provenance_immutable_guard();

SELECT moa.apply_three_tier_rls('moa.experiment_score_provenance'::REGCLASS);
