ALTER TABLE moa.experiment_trial
    ADD COLUMN IF NOT EXISTS plan_revision_uid UUID;

UPDATE moa.experiment_trial trial
SET plan_revision_uid = run.artifact_revision_uids[1]
FROM moa.experiment_run run
WHERE trial.run_uid = run.run_uid
  AND trial.plan_revision_uid IS NULL
  AND cardinality(run.artifact_revision_uids) > 0;

UPDATE moa.experiment_trial
SET plan_revision_uid = artifact_revision_uids[1]
WHERE plan_revision_uid IS NULL
  AND cardinality(artifact_revision_uids) > 0;

-- Prototype trial rows created before experiment_plan existed cannot be
-- reconstructed into a real pinned plan revision. Keep them loadable as old
-- records; runtime plan execution still rejects the nil revision.
UPDATE moa.experiment_trial
SET plan_revision_uid = '00000000-0000-0000-0000-000000000000'::UUID
WHERE plan_revision_uid IS NULL;

ALTER TABLE moa.experiment_trial
    ALTER COLUMN plan_revision_uid SET NOT NULL;
