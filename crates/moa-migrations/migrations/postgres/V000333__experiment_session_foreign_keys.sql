-- Shared experiment ledgers always belong to the canonical public session
-- store. The V000001 REFERENCES stays schema-relative because the session set
-- also replays into bare isolated databases that have no public.sessions;
-- this main-flow migration retargets the shared constraints afterwards.
ALTER TABLE moa.experiment_run
    DROP CONSTRAINT IF EXISTS experiment_run_session_id_fkey,
    ADD CONSTRAINT experiment_run_session_id_fkey
        FOREIGN KEY (session_id)
        REFERENCES public.sessions(id)
        ON DELETE SET NULL;

ALTER TABLE moa.experiment_trial
    DROP CONSTRAINT IF EXISTS experiment_trial_session_id_fkey,
    ADD CONSTRAINT experiment_trial_session_id_fkey
        FOREIGN KEY (session_id)
        REFERENCES public.sessions(id)
        ON DELETE SET NULL;
