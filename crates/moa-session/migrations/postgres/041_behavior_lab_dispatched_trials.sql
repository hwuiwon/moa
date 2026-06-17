ALTER TABLE moa.experiment_trial
    DROP CONSTRAINT IF EXISTS experiment_trial_status_check;

ALTER TABLE moa.experiment_trial
    ADD CONSTRAINT experiment_trial_status_check
    CHECK (status IN (
        'accepted',
        'dispatched',
        'running',
        'waiting_approval',
        'completed',
        'failed',
        'cancelled'
    ));
