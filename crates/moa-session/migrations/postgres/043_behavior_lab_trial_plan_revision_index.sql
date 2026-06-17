-- `plan_revision_uid` deliberately has no FK because legacy/non-plan trials can
-- use the nil sentinel during forward migration, but plan-scoped reads still
-- need an index.
CREATE INDEX IF NOT EXISTS experiment_trial_plan_revision_idx
    ON moa.experiment_trial (plan_revision_uid);
