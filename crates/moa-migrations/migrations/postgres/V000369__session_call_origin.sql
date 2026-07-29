-- Durable provenance class of the runtime a session was created for.
--
-- Every tool dispatch reloads the session row to decide what the caller may
-- hold, so an experiment trial's ceiling has to live on the session rather than
-- in the request that reaches the tool executor. Ordinary tenant traffic is
-- 'production'; an eval-owned session records the experiment run (and trial,
-- when the run expanded a plan) that owns it.
ALTER TABLE sessions
    ADD COLUMN call_origin JSONB NOT NULL DEFAULT '{"origin": "production"}'::JSONB;

-- The column is a security ceiling, so a row can never carry an unreadable
-- origin: the tag is closed, and the experiment variant must name its run.
ALTER TABLE sessions
    ADD CONSTRAINT sessions_call_origin_is_closed CHECK (
        (call_origin->>'origin') IN ('production', 'experiment', 'generated_code')
        AND (
            (call_origin->>'origin') <> 'experiment'
            OR (call_origin ? 'run_uid' AND call_origin ? 'trial_uid')
        )
    );
