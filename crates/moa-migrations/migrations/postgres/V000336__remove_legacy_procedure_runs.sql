-- Destructive removal of the superseded procedure runtime.
--
-- This cutover intentionally does not translate procedure runs, node history,
-- experiment targets, or terminal state into execution records. Deployments
-- adopting the execution runtime must start from a reset database or accept
-- that procedure-era runtime data is discarded.

LOCK TABLE moa.artifact_run IN ACCESS EXCLUSIVE MODE;
LOCK TABLE moa.artifact_node_run IN ACCESS EXCLUSIVE MODE;
LOCK TABLE moa.execution_run IN ACCESS EXCLUSIVE MODE;
LOCK TABLE moa.experiment_run IN ACCESS EXCLUSIVE MODE;
LOCK TABLE moa.experiment_trial IN ACCESS EXCLUSIVE MODE;

ALTER TABLE moa.execution_run
    ADD CONSTRAINT execution_run_contact_not_nil
    CHECK (
        contact_id IS NULL
        OR contact_id <> '00000000-0000-0000-0000-000000000000'::UUID
    ),
    ADD CONSTRAINT execution_run_uid_tenant_key UNIQUE (run_uid, tenant_id);

ALTER TABLE moa.execution_task
    ADD CONSTRAINT execution_task_contact_not_nil
    CHECK (
        contact_id IS NULL
        OR contact_id <> '00000000-0000-0000-0000-000000000000'::UUID
    );

ALTER TABLE moa.execution_action_review_outbox
    ADD CONSTRAINT execution_action_review_outbox_contact_not_nil
    CHECK (
        contact_id IS NULL
        OR contact_id <> '00000000-0000-0000-0000-000000000000'::UUID
    );

CREATE TABLE moa.execution_template_admission (
    operation_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    contact_id UUID,
    session_id UUID NOT NULL,
    idempotency_key TEXT,
    request_fingerprint TEXT NOT NULL
        CHECK (request_fingerprint ~ '^[0-9a-f]{64}$'),
    originating_user_sequence_num BIGINT
        CHECK (originating_user_sequence_num >= 0),
    execution_run_uid UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT execution_template_admission_key_bytes CHECK (
        idempotency_key IS NULL
        OR octet_length(idempotency_key) BETWEEN 1 AND 256
    ),
    CONSTRAINT execution_template_admission_contact_not_nil CHECK (
        contact_id IS NULL
        OR contact_id <> '00000000-0000-0000-0000-000000000000'::UUID
    ),
    CONSTRAINT execution_template_admission_run_scope_fkey
        FOREIGN KEY (execution_run_uid, tenant_id, contact_id)
        REFERENCES moa.execution_run (run_uid, tenant_id, contact_id)
);

CREATE UNIQUE INDEX execution_template_admission_tenant_key_uidx
    ON moa.execution_template_admission (tenant_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE INDEX execution_template_admission_session_idx
    ON moa.execution_template_admission (tenant_id, session_id, created_at);

SELECT moa.apply_contact_rls('moa.execution_template_admission'::REGCLASS);

-- Procedure-backed experiments are unsupported and intentionally discarded.
DELETE FROM moa.experiment_trial
WHERE target_kind = 'procedure';

DELETE FROM moa.experiment_run
WHERE target_kind = 'procedure';

ALTER TABLE moa.experiment_run
    DROP CONSTRAINT IF EXISTS experiment_run_target_kind_check,
    ADD COLUMN execution_run_uid UUID,
    ADD CONSTRAINT experiment_run_target_kind_check
        CHECK (target_kind IN ('agent_loop', 'execution_template')),
    ADD CONSTRAINT experiment_run_execution_scope_fkey
        FOREIGN KEY (execution_run_uid, tenant_id)
        REFERENCES moa.execution_run (run_uid, tenant_id)
        ON DELETE SET NULL (execution_run_uid);

ALTER TABLE moa.experiment_trial
    DROP CONSTRAINT IF EXISTS experiment_trial_target_kind_check,
    ADD COLUMN execution_run_uid UUID,
    ADD CONSTRAINT experiment_trial_target_kind_check
        CHECK (target_kind IN ('agent_loop', 'execution_template')),
    ADD CONSTRAINT experiment_trial_execution_scope_fkey
        FOREIGN KEY (execution_run_uid, tenant_id)
        REFERENCES moa.execution_run (run_uid, tenant_id)
        ON DELETE SET NULL (execution_run_uid);

CREATE INDEX experiment_run_execution_run_idx
    ON moa.experiment_run (execution_run_uid)
    WHERE execution_run_uid IS NOT NULL;

CREATE INDEX experiment_trial_execution_run_idx
    ON moa.experiment_trial (execution_run_uid)
    WHERE execution_run_uid IS NOT NULL;

DROP INDEX IF EXISTS moa.experiment_run_procedure_run_idx;
DROP INDEX IF EXISTS moa.experiment_trial_procedure_run_idx;

ALTER TABLE moa.experiment_run
    DROP COLUMN procedure_run_uid;

ALTER TABLE moa.experiment_trial
    DROP COLUMN procedure_run_uid;

DROP MATERIALIZED VIEW IF EXISTS analytics.procedure_node_run_fact;
DROP MATERIALIZED VIEW IF EXISTS analytics.procedure_run_fact;

CREATE MATERIALIZED VIEW analytics.execution_run_fact AS
SELECT
    r.run_uid,
    r.tenant_id,
    r.session_id,
    sac.agent_id,
    r.source_provenance ->> 'kind' AS source_kind,
    r.source_provenance ->> 'skill_template_ref' AS source_ref,
    r.active_plan_hash AS plan_hash,
    r.plan_revision,
    r.source_provenance ->> 'route_reason' AS route_reason,
    r.status,
    r.terminal_cause ->> 'kind' AS terminal_reason,
    (jsonb_array_length(r.terminal_gaps) > 0) AS error_present,
    r.created_at,
    r.updated_at,
    r.started_at,
    r.completed_at,
    r.terminal_requirement_count AS required_count,
    r.terminal_satisfied_requirement_count AS satisfied_count,
    r.reserved_cost_microusd,
    r.consumed_cost_microusd AS actual_cost_microusd,
    r.reserved_tokens,
    r.consumed_tokens AS actual_tokens,
    r.progress_total_tasks AS logical_task_count,
    CASE
        WHEN r.started_at IS NULL OR r.completed_at IS NULL THEN NULL::DOUBLE PRECISION
        ELSE EXTRACT(EPOCH FROM (r.completed_at - r.started_at)) * 1000.0
    END AS duration_ms
FROM moa.execution_run r
LEFT JOIN session_agent_context sac ON sac.session_id = r.session_id;

CREATE UNIQUE INDEX analytics_execution_run_fact_run_uidx
    ON analytics.execution_run_fact (run_uid);

CREATE INDEX analytics_execution_run_fact_tenant_started_idx
    ON analytics.execution_run_fact (tenant_id, started_at DESC);

CREATE MATERIALIZED VIEW analytics.execution_task_fact AS
SELECT
    t.task_id AS task_uid,
    t.run_uid,
    t.tenant_id,
    t.node_id,
    t.item_key,
    CASE
        WHEN t.task_kind ->> 'kind' = 'capability'
        THEN (t.task_kind #>> '{reference,name}') || ':'
             || (t.task_kind #>> '{reference,version}')
        ELSE NULL
    END AS capability_ref,
    t.plan_revision,
    t.status,
    t.attempt,
    t.generation,
    (t.error IS NOT NULL) AS error_present,
    t.created_at,
    t.updated_at,
    t.started_at,
    t.completed_at,
    t.reserved_cost_microusd,
    t.actual_cost_microusd,
    t.reserved_tokens,
    t.actual_tokens,
    jsonb_array_length(t.citations) AS citation_count,
    CASE
        WHEN t.started_at IS NULL OR t.completed_at IS NULL THEN NULL::DOUBLE PRECISION
        ELSE EXTRACT(EPOCH FROM (t.completed_at - t.started_at)) * 1000.0
    END AS duration_ms
FROM moa.execution_task t;

CREATE UNIQUE INDEX analytics_execution_task_fact_task_uidx
    ON analytics.execution_task_fact (task_uid);

CREATE INDEX analytics_execution_task_fact_tenant_started_idx
    ON analytics.execution_task_fact (tenant_id, started_at DESC);

DROP TABLE moa.artifact_node_run;
DROP TABLE moa.artifact_run;
