-- Behavior Lab resource supervision.
--
-- A Behavior Lab run fans out into parallel trials, each of which drives paid
-- provider calls, paid target turns, and side-effecting tool and sandbox work.
-- Before this migration nothing withheld capacity ahead of a dispatch: the plan
-- declared `budget.max_total_cents`, threshold evaluators scored the spend
-- afterwards, and a runaway plan was observable only once the money was gone.
--
-- The model here is reserve-then-reconcile against one durable ledger per run:
--
--   * `moa.experiment_run.resource_envelope` is the authored ceiling — total-run
--     limits, per-trial limits, and an absolute wall-clock deadline.
--   * `resource_committed` and `resource_outstanding` are the ledger. A caller
--     may dispatch only when `committed + outstanding + request` stays inside
--     every limit and the deadline has not passed.
--   * `moa.experiment_resource_reservation` holds one row per withheld
--     reservation. The row is keyed by a caller-supplied deterministic
--     `reservation_key`, so a Restate replay of the same dispatch finds its own
--     reservation instead of charging the envelope twice.
--
-- Parallel trials of one run reserve against the same `experiment_run` row, so
-- concurrency is serialized by that row's lock and cannot oversubscribe the
-- envelope.
--
-- Rows that predate this migration receive a zero-limit envelope with an
-- already-expired deadline. That is deliberate: an unsupervised run must refuse
-- every reservation rather than inherit an unbounded one.

ALTER TABLE moa.experiment_run
    ADD COLUMN resource_envelope JSONB NOT NULL DEFAULT '{
        "version": 1,
        "run_limits": {"cost_micro_usd": 0, "tokens": 0, "turns": 0, "model_calls": 0, "tool_calls": 0},
        "trial_limits": {"cost_micro_usd": 0, "tokens": 0, "turns": 0, "model_calls": 0, "tool_calls": 0},
        "deadline_at": "1970-01-01T00:00:00Z"
    }'::JSONB,
    ADD COLUMN resource_committed JSONB NOT NULL
        DEFAULT '{"cost_micro_usd": 0, "tokens": 0, "turns": 0, "model_calls": 0, "tool_calls": 0}'::JSONB,
    ADD COLUMN resource_outstanding JSONB NOT NULL
        DEFAULT '{"cost_micro_usd": 0, "tokens": 0, "turns": 0, "model_calls": 0, "tool_calls": 0}'::JSONB,
    ADD COLUMN plan_artifact_uid UUID REFERENCES moa.artifact(artifact_uid) ON DELETE SET NULL,
    ADD COLUMN expected_trials BIGINT NOT NULL DEFAULT 0 CHECK (expected_trials >= 0);

-- Preserve the number of already-minted trials for runs created before this
-- column existed. Those rows had no authored pre-expansion count, so the
-- durable child rows are the only count that can be recovered without guessing.
UPDATE moa.experiment_run run
SET expected_trials = trial_count.expected_trials
FROM (
    SELECT run_uid, count(*)::BIGINT AS expected_trials
    FROM moa.experiment_trial
    GROUP BY run_uid
) AS trial_count
WHERE trial_count.run_uid = run.run_uid;

-- The store states both values explicitly for every new run. The envelope
-- default is removed so a new unsupervised run cannot be inserted accidentally;
-- the projected-trial default remains zero for direct zero-trial rows, while
-- pre-existing rows receive the recoverable child-row count above.
ALTER TABLE moa.experiment_run
    ALTER COLUMN resource_envelope DROP DEFAULT;

ALTER TABLE moa.experiment_trial
    ADD COLUMN resource_envelope JSONB NOT NULL DEFAULT '{
        "version": 1,
        "limits": {"cost_micro_usd": 0, "tokens": 0, "turns": 0, "model_calls": 0, "tool_calls": 0},
        "deadline": "1970-01-01T00:00:00Z"
    }'::JSONB;

ALTER TABLE moa.experiment_trial
    ALTER COLUMN resource_envelope DROP DEFAULT;

-- A trial reservation must name a trial owned by the same run. A nullable
-- trial UID still permits run-scoped reservations.
CREATE UNIQUE INDEX IF NOT EXISTS experiment_trial_run_uid_pair_uniq
    ON moa.experiment_trial (run_uid, trial_uid);

-- One withheld reservation. `reservation_key` is the caller's deterministic
-- dispatch coordinate (trial, component, turn index), and the unique index on
-- `(run_uid, reservation_key)` is what makes a replayed dispatch idempotent
-- rather than a second charge.
CREATE TABLE IF NOT EXISTS moa.experiment_resource_reservation (
    reservation_uid UUID PRIMARY KEY,
    run_uid UUID NOT NULL REFERENCES moa.experiment_run(run_uid) ON DELETE CASCADE,
    trial_uid UUID,
    storage_partition_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    reservation_key TEXT NOT NULL,
    component TEXT NOT NULL CHECK (component IN ('target', 'simulator', 'judge', 'tool')),
    state TEXT NOT NULL CHECK (state IN ('open', 'reconciled', 'released')),
    reserved JSONB NOT NULL,
    actual JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (state <> 'reconciled' OR actual IS NOT NULL),
    CONSTRAINT experiment_resource_reservation_run_trial_fkey
        FOREIGN KEY (run_uid, trial_uid)
        REFERENCES moa.experiment_trial(run_uid, trial_uid)
        ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS experiment_resource_reservation_key_uniq
    ON moa.experiment_resource_reservation (run_uid, reservation_key);

CREATE INDEX IF NOT EXISTS experiment_resource_reservation_trial_idx
    ON moa.experiment_resource_reservation (run_uid, trial_uid);

CREATE INDEX IF NOT EXISTS experiment_resource_reservation_scope_idx
    ON moa.experiment_resource_reservation (storage_partition_id, scope, user_id, run_uid);

SELECT moa.apply_three_tier_rls('moa.experiment_resource_reservation'::REGCLASS);

CREATE INDEX IF NOT EXISTS experiment_run_active_admission_idx
    ON moa.experiment_run (plan_artifact_uid, storage_partition_id)
    WHERE status IN ('accepted', 'running');

-- Admission load for one prospective run, at all three scopes it must fit
-- inside. Per-artifact throttling alone is bypassable by creating more plan
-- artifacts, so the tenant and fleet totals are read from the same snapshot.
-- An admitted run reserves its entire projected matrix immediately; counting
-- child rows would leave the quota blind until asynchronous expansion finished.
--
-- SECURITY DEFINER because the fleet total spans tenants that the admitting
-- connection's row policies deliberately hide. Only six aggregate counts leave
-- the function; no cross-tenant row is ever returned. The search_path is pinned
-- so definer rights cannot be redirected to a caller-controlled schema.
CREATE OR REPLACE FUNCTION moa.experiment_admission_counts(
    target_storage_partition_id TEXT,
    target_plan_artifact_uid UUID
)
RETURNS TABLE (
    artifact_active_runs BIGINT,
    artifact_active_trials BIGINT,
    tenant_active_runs BIGINT,
    tenant_active_trials BIGINT,
    fleet_active_runs BIGINT,
    fleet_active_trials BIGINT
)
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = moa, pg_catalog
AS $$
    SELECT
        count(*) FILTER (
            WHERE target_plan_artifact_uid IS NOT NULL
              AND active.plan_artifact_uid = target_plan_artifact_uid
        ),
        coalesce(sum(active.active_trials) FILTER (
            WHERE target_plan_artifact_uid IS NOT NULL
              AND active.plan_artifact_uid = target_plan_artifact_uid
        ), 0),
        count(*) FILTER (
            WHERE active.storage_partition_id IS NOT DISTINCT FROM target_storage_partition_id
        ),
        coalesce(sum(active.active_trials) FILTER (
            WHERE active.storage_partition_id IS NOT DISTINCT FROM target_storage_partition_id
        ), 0),
        count(*),
        coalesce(sum(active.active_trials), 0)
    FROM (
        SELECT
            run.storage_partition_id,
            run.plan_artifact_uid,
            run.expected_trials AS active_trials
        FROM moa.experiment_run run
        WHERE run.status IN ('accepted', 'running')
    ) AS active;
$$;

REVOKE ALL ON FUNCTION moa.experiment_admission_counts(TEXT, UUID) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.experiment_admission_counts(TEXT, UUID) TO moa_app;
GRANT EXECUTE ON FUNCTION moa.experiment_admission_counts(TEXT, UUID) TO moa_promoter;
