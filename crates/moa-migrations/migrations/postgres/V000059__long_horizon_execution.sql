-- Bounded activation persistence for execution runs that can remain durable for
-- days or weeks without retaining a live handler, task lease, or sandbox.
--
-- This is a deliberate hard break. Legacy nonterminal runs are tied to the
-- lifetime-spanning ExecutionRun/ExecutionTask workflow protocol and cannot be
-- reinterpreted safely as bounded activations.

DO $long_horizon_cutover$
DECLARE
    nonterminal_count BIGINT;
BEGIN
    SELECT count(*)
    INTO nonterminal_count
    FROM moa.execution_run
    WHERE status NOT IN (
        'completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
    );

    IF nonterminal_count <> 0 THEN
        RAISE EXCEPTION
            'cannot install long-horizon execution while % legacy execution run(s) are nonterminal; terminalize or cancel them and deliberately reset the old Restate execution journals before retrying',
            nonterminal_count
            USING ERRCODE = 'check_violation';
    END IF;
END
$long_horizon_cutover$;

CREATE TABLE moa.execution_maintenance_checkpoint (
    job_kind TEXT PRIMARY KEY CHECK (btrim(job_kind) <> ''),
    generation BIGINT NOT NULL DEFAULT 0 CHECK (generation >= 0),
    last_started_at TIMESTAMPTZ,
    last_succeeded_at TIMESTAMPTZ,
    last_failure_at TIMESTAMPTZ,
    next_run_at TIMESTAMPTZ,
    scheduled_generation BIGINT CHECK (scheduled_generation >= 1),
    claim_owner TEXT,
    claimed_generation BIGINT CHECK (claimed_generation >= 1),
    claim_expires_at TIMESTAMPTZ,
    last_error TEXT CHECK (
        last_error IS NULL OR octet_length(last_error) BETWEEN 1 AND 4096
    ),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT execution_maintenance_checkpoint_failure_pair_check CHECK (
        (last_failure_at IS NULL) = (last_error IS NULL)
    ),
    CONSTRAINT execution_maintenance_checkpoint_time_order_check CHECK (
        (last_succeeded_at IS NULL
            OR last_started_at IS NOT NULL)
        AND
        (last_failure_at IS NULL
            OR last_started_at IS NOT NULL)
    ),
    CONSTRAINT execution_maintenance_checkpoint_schedule_pair_check CHECK (
        (next_run_at IS NULL) = (scheduled_generation IS NULL)
        AND (scheduled_generation IS NULL OR scheduled_generation <= generation + 1)
    ),
    CONSTRAINT execution_maintenance_checkpoint_claim_shape_check CHECK (
        (claim_owner IS NULL
            AND claimed_generation IS NULL
            AND claim_expires_at IS NULL)
        OR
        (claim_owner IS NOT NULL
            AND btrim(claim_owner) <> ''
            AND claimed_generation IS NOT NULL
            AND claim_expires_at IS NOT NULL
            AND claimed_generation = scheduled_generation)
    )
);

CREATE INDEX execution_maintenance_checkpoint_due_idx
    ON moa.execution_maintenance_checkpoint (next_run_at, job_kind)
    WHERE next_run_at IS NOT NULL;

REVOKE ALL ON TABLE moa.execution_maintenance_checkpoint FROM PUBLIC;
GRANT SELECT, INSERT, UPDATE ON TABLE moa.execution_maintenance_checkpoint TO moa_app;

CREATE OR REPLACE FUNCTION moa.execution_admitted_identity_is_valid(
    candidate JSONB,
    expected_tenant_id UUID
) RETURNS BOOLEAN
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT moa.execution_json_object_has_exact_keys(
               candidate,
               ARRAY[
                   'identity_type', 'id', 'tenant_id', 'api_key_id',
                   'acting_on_behalf_of'
               ]
           )
       AND candidate ->> 'identity_type' IN ('operator', 'contact', 'agent', 'service')
       AND candidate ->> 'id' ~
           '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
       AND candidate ->> 'tenant_id' = expected_tenant_id::TEXT
       AND (
           candidate -> 'api_key_id' = 'null'::JSONB
           OR candidate ->> 'api_key_id' ~
               '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
       )
       AND (
           candidate -> 'acting_on_behalf_of' = 'null'::JSONB
           OR candidate ->> 'acting_on_behalf_of' ~
               '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
       )
$$;

CREATE OR REPLACE FUNCTION moa.execution_schedule_origin_is_valid(
    candidate JSONB,
    expected_tenant_id UUID
) RETURNS BOOLEAN
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT moa.execution_json_object_has_exact_keys(
               candidate, ARRAY['request_uid', 'created_by', 'source']
           )
       AND candidate ->> 'request_uid' ~
           '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
       AND moa.execution_admitted_identity_is_valid(
               candidate -> 'created_by', expected_tenant_id
           )
       AND CASE candidate #>> '{source,kind}'
           WHEN 'tenant_api' THEN candidate -> 'source' = '{"kind":"tenant_api"}'::JSONB
           WHEN 'session' THEN
               moa.execution_json_object_has_exact_keys(
                   candidate -> 'source',
                   ARRAY['kind', 'session_id', 'originating_user_sequence_num']
               )
               AND candidate #>> '{source,session_id}' ~
                   '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
               AND candidate #>> '{source,originating_user_sequence_num}' ~ '^[0-9]+$'
           ELSE FALSE
       END
$$;

CREATE OR REPLACE FUNCTION moa.execution_temporal_target_is_valid(candidate JSONB)
RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
AS $$
BEGIN
    RETURN CASE candidate ->> 'kind'
        WHEN 'at' THEN
            moa.execution_json_object_has_exact_keys(candidate, ARRAY['kind', 'at'])
            AND jsonb_typeof(candidate -> 'at') = 'string'
            AND (candidate ->> 'at')::TIMESTAMPTZ IS NOT NULL
        WHEN 'after' THEN
            moa.execution_json_object_has_exact_keys(
                candidate, ARRAY['kind', 'delay_seconds']
            )
            AND jsonb_typeof(candidate -> 'delay_seconds') = 'number'
            AND (candidate ->> 'delay_seconds') ~ '^[1-9][0-9]*$'
        ELSE FALSE
    END;
EXCEPTION
    WHEN OTHERS THEN RETURN FALSE;
END;
$$;

CREATE OR REPLACE FUNCTION moa.execution_wait_expiry_action_is_valid(candidate JSONB)
RETURNS BOOLEAN
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT CASE candidate ->> 'kind'
        WHEN 'fail_task' THEN
            candidate = '{"kind":"fail_task"}'::JSONB
        WHEN 'continue_with' THEN
            moa.execution_json_object_has_exact_keys(candidate, ARRAY['kind', 'output'])
        ELSE FALSE
    END
$$;

CREATE OR REPLACE FUNCTION moa.execution_wait_policy_is_valid(candidate JSONB)
RETURNS BOOLEAN
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT moa.execution_json_object_has_exact_keys(
               candidate, ARRAY['expiry', 'on_expiry']
           )
       AND moa.execution_temporal_target_is_valid(candidate -> 'expiry')
       AND moa.execution_wait_expiry_action_is_valid(candidate -> 'on_expiry')
$$;

-- The old four-key definition remains valid only so retained terminal audit
-- rows can continue to satisfy their original check constraint. Every new run
-- and serving skill template is required to use the current five-key shape.
CREATE OR REPLACE FUNCTION moa.execution_plan_definition_is_current(candidate JSONB)
RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
    node JSONB;
    operation JSONB;
BEGIN
    IF NOT moa.execution_json_object_has_exact_keys(
           candidate,
           ARRAY[
               'cancel_policy', 'input_schema', 'output_schema', 'input_wait_policy',
               'nodes'
           ]
       )
       OR candidate ->> 'cancel_policy' NOT IN (
           'retain_effects', 'compensate_committed'
       )
       OR NOT moa.execution_wait_policy_is_valid(candidate -> 'input_wait_policy')
       OR jsonb_typeof(candidate -> 'nodes') <> 'array' THEN
        RETURN FALSE;
    END IF;
    FOR node IN SELECT value FROM jsonb_array_elements(candidate -> 'nodes') LOOP
        IF NOT moa.execution_json_object_has_exact_keys(
            node,
            ARRAY[
                'id', 'requirement_ids', 'depends_on', 'when', 'input',
                'output_schema', 'operation', 'compensation', 'retry', 'budget'
            ]
        ) THEN
            RETURN FALSE;
        END IF;
        operation := node -> 'operation';
        IF operation ->> 'kind' IN ('review', 'wait_signal')
           AND NOT moa.execution_wait_policy_is_valid(operation -> 'wait_policy') THEN
            RETURN FALSE;
        END IF;
        IF operation ->> 'kind' = 'wait_until'
           AND NOT (
               moa.execution_json_object_has_exact_keys(
                   operation, ARRAY['kind', 'wake', 'result']
               )
               AND moa.execution_temporal_target_is_valid(operation -> 'wake')
           ) THEN
            RETURN FALSE;
        END IF;
    END LOOP;
    RETURN TRUE;
EXCEPTION
    WHEN OTHERS THEN RETURN FALSE;
END;
$$;

CREATE OR REPLACE FUNCTION moa.execution_plan_snapshot_is_current(candidate JSONB)
RETURNS BOOLEAN
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT moa.execution_json_object_has_exact_keys(
               candidate,
               ARRAY['definition', 'plan_hash', 'catalog_hash', 'estimate', 'report']
           )
       AND moa.execution_plan_definition_is_current(candidate -> 'definition')
       AND jsonb_typeof(candidate -> 'plan_hash') = 'string'
       AND candidate ->> 'plan_hash' ~ '^[0-9a-f]{64}$'
$$;

-- V55 bound both run snapshots to the old four-key plan validator. Replace
-- those constraints at the cutover boundary so every nonterminal/current run
-- uses the five-key long-horizon contract. Pre-cutover terminal rows remain
-- immutable audit evidence and are the only permitted legacy shape.
ALTER TABLE moa.execution_run
    DROP CONSTRAINT execution_run_initial_plan_check,
    DROP CONSTRAINT execution_run_active_plan_check,
    ADD CONSTRAINT execution_run_initial_plan_check CHECK (
        status IN (
            'completed', 'partial', 'blocked', 'unsupported',
            'failed', 'cancelled'
        )
        OR moa.execution_plan_snapshot_is_current(initial_plan)
    ),
    ADD CONSTRAINT execution_run_active_plan_check CHECK (
        status IN (
            'completed', 'partial', 'blocked', 'unsupported',
            'failed', 'cancelled'
        )
        OR moa.execution_plan_snapshot_is_current(active_plan)
    );

CREATE OR REPLACE FUNCTION moa.skill_execution_template_is_valid(candidate JSONB)
RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
    plan JSONB;
BEGIN
    IF candidate #>> '{definition,type}' IS DISTINCT FROM 'skill'
       OR candidate #> '{definition,spec,execution_plan}' IS NULL THEN
        RETURN TRUE;
    END IF;
    plan := candidate #> '{definition,spec,execution_plan,plan}';
    RETURN moa.execution_plan_definition_is_current(plan);
EXCEPTION
    WHEN OTHERS THEN RETURN FALSE;
END;
$$;

-- Existing rows are terminal by the cutover precondition. Initialize them as
-- inactive while retaining all immutable plans, outcomes, and audit evidence.
ALTER TABLE moa.execution_run
    ADD COLUMN admitted_identity JSONB,
    ADD COLUMN controller_generation BIGINT NOT NULL DEFAULT 1
        CHECK (controller_generation >= 1),
    ADD COLUMN activation_state TEXT NOT NULL DEFAULT 'queued'
        CHECK (activation_state IN ('idle', 'queued', 'advancing', 'paused', 'terminal')),
    ADD COLUMN next_wake_at TIMESTAMPTZ,
    ADD COLUMN waiting_since TIMESTAMPTZ,
    ADD COLUMN last_progress_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN pause_requested_at TIMESTAMPTZ,
    ADD COLUMN paused_at TIMESTAMPTZ,
    -- Consecutive crashed activations since the last acknowledged wake. This is
    -- deliberately not generation-scoped: the reset rides the existing healthy
    -- wake acknowledgement, so failures can carry across a generation bump, but
    -- one healthy activation clears it and a run that cannot complete even one
    -- is genuinely not progressing.
    ADD COLUMN activation_failure_count BIGINT NOT NULL DEFAULT 0
        CHECK (activation_failure_count >= 0),
    ADD COLUMN ready_task_count BIGINT NOT NULL DEFAULT 0
        CHECK (ready_task_count >= 0),
    ADD COLUMN active_task_count BIGINT NOT NULL DEFAULT 0
        CHECK (active_task_count >= 0),
    ADD COLUMN waiting_task_count BIGINT NOT NULL DEFAULT 0
        CHECK (waiting_task_count >= 0),
    ADD COLUMN waiting_input_task_count BIGINT NOT NULL DEFAULT 0
        CHECK (waiting_input_task_count >= 0),
    ADD COLUMN waiting_input_user_task_count BIGINT NOT NULL DEFAULT 0
        CHECK (waiting_input_user_task_count >= 0),
    ADD COLUMN waiting_input_tenant_admin_task_count BIGINT NOT NULL DEFAULT 0
        CHECK (waiting_input_tenant_admin_task_count >= 0),
    ADD COLUMN waiting_input_external_task_count BIGINT NOT NULL DEFAULT 0
        CHECK (waiting_input_external_task_count >= 0),
    ADD COLUMN waiting_review_task_count BIGINT NOT NULL DEFAULT 0
        CHECK (waiting_review_task_count >= 0),
    ADD COLUMN waiting_signal_task_count BIGINT NOT NULL DEFAULT 0
        CHECK (waiting_signal_task_count >= 0),
    ADD COLUMN waiting_timer_task_count BIGINT NOT NULL DEFAULT 0
        CHECK (waiting_timer_task_count >= 0),
    ADD COLUMN waiting_external_task_count BIGINT NOT NULL DEFAULT 0
        CHECK (waiting_external_task_count >= 0),
    ADD COLUMN waiting_replan_task_count BIGINT NOT NULL DEFAULT 0
        CHECK (waiting_replan_task_count >= 0),
    ADD COLUMN waiting_reasons_truncated BOOLEAN NOT NULL DEFAULT FALSE,
    ADD CONSTRAINT execution_run_waiting_task_counts_check CHECK (
        waiting_task_count = waiting_input_task_count
            + waiting_review_task_count
            + waiting_signal_task_count
            + waiting_timer_task_count
            + waiting_external_task_count
            + waiting_replan_task_count
    ),
    ADD CONSTRAINT execution_run_waiting_input_audience_counts_check CHECK (
        waiting_input_task_count = waiting_input_user_task_count
            + waiting_input_tenant_admin_task_count
            + waiting_input_external_task_count
    ),
    ADD CONSTRAINT execution_run_pause_timestamp_order_check CHECK (
        paused_at IS NULL
        OR (pause_requested_at IS NOT NULL AND paused_at >= pause_requested_at)
    );

ALTER TABLE moa.execution_run
    DROP CONSTRAINT execution_run_waiting_reasons_check,
    ADD CONSTRAINT execution_run_waiting_reasons_bounded_check CHECK (
        jsonb_typeof(waiting_reasons) = 'array'
        AND jsonb_array_length(waiting_reasons) <= 64
        AND pg_column_size(waiting_reasons) <= 65536
        AND (NOT waiting_reasons_truncated OR waiting_task_count > 0)
    );

UPDATE moa.execution_run
SET admitted_identity = jsonb_build_object(
        'identity_type', CASE WHEN contact_id IS NULL THEN 'operator' ELSE 'contact' END,
        'id', COALESCE(
            contact_id::TEXT,
            CASE
                WHEN owner_user_id ~
                    '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
                    THEN owner_user_id
                ELSE run_uid::TEXT
            END
        ),
        'tenant_id', tenant_id::TEXT,
        'api_key_id', NULL,
        'acting_on_behalf_of', NULL
    ),
    activation_state = 'terminal',
    next_wake_at = NULL,
    waiting_since = NULL,
    last_progress_at = updated_at,
    ready_task_count = 0,
    active_task_count = 0;

ALTER TABLE moa.execution_run
    ALTER COLUMN admitted_identity SET NOT NULL,
    ALTER COLUMN activation_state SET DEFAULT 'queued',
    ADD CONSTRAINT execution_run_admitted_identity_check
        CHECK (moa.execution_admitted_identity_is_valid(admitted_identity, tenant_id));

CREATE OR REPLACE FUNCTION moa.enforce_execution_run_insert_confirmation()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.status NOT IN ('awaiting_confirmation', 'queued') THEN
        RAISE EXCEPTION 'execution runs must start awaiting confirmation or queued';
    END IF;
    IF NOT moa.execution_plan_snapshot_is_current(NEW.initial_plan)
       OR NOT moa.execution_plan_snapshot_is_current(NEW.active_plan) THEN
        RAISE EXCEPTION 'new execution runs require the current long-horizon plan contract';
    END IF;
    NEW.created_at := now();
    NEW.queued_at := CASE
        WHEN NEW.status = 'queued' THEN NEW.created_at
        WHEN NEW.status = 'awaiting_confirmation' THEN NULL
        ELSE NEW.queued_at
    END;
    NEW.activation_state := CASE
        WHEN NEW.status = 'queued' THEN 'queued'
        ELSE 'idle'
    END;
    NEW.last_progress_at := NEW.created_at;
    IF NEW.confirmed_plan_hash IS NOT NULL OR NEW.confirmed_at IS NOT NULL THEN
        RAISE EXCEPTION 'execution run confirmation proof must be created by confirmation';
    END IF;
    RETURN NEW;
END;
$$;

ALTER TABLE moa.execution_run
    DROP CONSTRAINT execution_run_status_check,
    ADD CONSTRAINT execution_run_status_check CHECK (status IN (
        'awaiting_confirmation', 'queued', 'running', 'waiting_input',
        'waiting_review', 'waiting_signal', 'waiting_timer', 'waiting_external',
        'waiting_replan', 'pause_requested', 'pausing', 'paused', 'compensating',
        'completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
    ));

-- The expanded long-horizon status vocabulary is also the complete set of
-- states from which a terminal fence may be staged.
ALTER TABLE moa.execution_run
    DROP CONSTRAINT execution_run_pending_terminal_check,
    ADD CONSTRAINT execution_run_pending_terminal_check CHECK (
        (
            pending_terminal_status IS NULL
            AND pending_terminal_reason IS NULL
            AND pending_terminal_cause IS NULL
            AND pending_terminal_output IS NULL
        )
        OR (
            status IN (
                'awaiting_confirmation', 'queued', 'running', 'waiting_input',
                'waiting_review', 'waiting_signal', 'waiting_timer', 'waiting_external',
                'waiting_replan', 'pause_requested', 'pausing', 'paused', 'compensating'
            )
            AND pending_terminal_status IN (
                'completed','partial','blocked','unsupported','failed','cancelled'
            )
            AND pending_terminal_reason IS NOT NULL
            AND btrim(pending_terminal_reason) <> ''
            AND moa.execution_pending_terminal_payload_is_valid(pending_terminal_cause)
            AND moa.execution_terminal_reason_for(
                pending_terminal_status,
                pending_terminal_cause #> '{terminal_evidence,cause}',
                source_kind
            ) = pending_terminal_reason
            AND (
                (
                    pending_terminal_status = 'cancelled'
                    AND cancellation_reason IS NOT NULL
                    AND btrim(cancellation_reason) <> ''
                )
                OR (
                    pending_terminal_status <> 'cancelled'
                    AND cancellation_reason IS NULL
                )
            )
        )
    );

-- Retire the two terminal-vocabulary values this architecture supersedes.
--
-- `scheduler_no_progress` was produced only by the deleted whole-plan scheduler's
-- `ScheduleDecision::NoProgress`. The incremental controller cannot reach the state it
-- named: `cancel_unmaterialized_dependents_in_tx` terminalizes the whole transitive
-- dependent closure in the same transaction as the node failure and raises on a partial
-- cascade, and any residual stall is settled by the always-armed run deadline as
-- `deadline_exceeded`, which is the more actionable terminal.
--
-- `dependency_failed` is a task-level failure class, but a dependency failure is now a
-- node-level cancellation: the cascade asserts every dependent it cancels carries zero
-- tasks, so there is no task to classify. Producing it would require synthesizing failed
-- rows for work that never ran, which would displace the real root-cause class that
-- `load_earliest_typed_task_failure` surfaces on the run terminal.
--
-- This patches the live function rather than editing V27 in place: V55 already rewrites
-- this body through `pg_get_functiondef` + `replace`, so an in-place V27 edit would reach
-- a fresh database and never reach one that already ran V27. Each removal is guarded on
-- its exact predecessor text so a drifted baseline fails loudly instead of silently
-- leaving an accepted value behind. `execution_run_terminal_evidence` needs no companion
-- change: V55 already replaced that constraint with one that delegates entirely to this
-- function and never repeats the class list.
DO $execution_long_horizon_terminal_reason$
DECLARE
    definition TEXT;
    -- Each anchor starts at the branch indent and ends with its own newline, so the
    -- removal takes the whole branch and leaves the surrounding lines intact.
    old_no_progress_validation TEXT := $old$        WHEN 'scheduler_no_progress' THEN
            terminal_cause = '{"kind":"scheduler_no_progress"}'::JSONB
$old$;
    old_no_progress_reason TEXT := $old$        WHEN 'scheduler_no_progress' THEN
            RETURN CASE status_value
                WHEN 'unsupported' THEN 'unsupported_plan'
                WHEN 'partial' THEN 'no_progress'
                WHEN 'blocked' THEN 'no_progress'
                WHEN 'failed' THEN 'no_progress'
                ELSE NULL
            END;
$old$;
    old_failure_classes TEXT :=
        $old$                'retryable','dependency_failed','invalid_input','invalid_output',$old$;
    new_failure_classes TEXT :=
        $new$                'retryable','invalid_input','invalid_output',$new$;
BEGIN
    SELECT pg_get_functiondef(
        'moa.execution_terminal_reason_for(text,jsonb,text)'::REGPROCEDURE
    ) INTO definition;
    IF position(old_no_progress_validation IN definition) = 0
       OR position(old_no_progress_reason IN definition) = 0
       OR position(old_failure_classes IN definition) = 0 THEN
        RAISE EXCEPTION
            'execution terminal reason function drifted before V59'
            USING ERRCODE = '55000';
    END IF;
    definition := replace(definition, old_no_progress_validation, '');
    definition := replace(definition, old_no_progress_reason, '');
    definition := replace(definition, old_failure_classes, new_failure_classes);
    EXECUTE definition;
END
$execution_long_horizon_terminal_reason$;

DROP INDEX moa.execution_run_nonterminal_idx;
CREATE INDEX execution_run_nonterminal_idx
    ON moa.execution_run (status, updated_at, run_uid)
    WHERE status IN (
        'awaiting_confirmation', 'queued', 'running', 'waiting_input',
        'waiting_review', 'waiting_signal', 'waiting_timer', 'waiting_external',
        'waiting_replan', 'pause_requested', 'pausing', 'paused', 'compensating'
    );

-- The exact-deadline invariant guard scans for overdue nonterminal runs each
-- reconcile. It shares this predicate with execution_run_nonterminal_idx, but
-- that index leads on status and so cannot answer a budget_deadline_at range;
-- leading on the deadline keeps the guard an index-only scan.
CREATE INDEX execution_run_overdue_deadline_idx
    ON moa.execution_run (budget_deadline_at, run_uid)
    WHERE budget_deadline_at IS NOT NULL AND status IN (
        'awaiting_confirmation', 'queued', 'running', 'waiting_input',
        'waiting_review', 'waiting_signal', 'waiting_timer', 'waiting_external',
        'waiting_replan', 'pause_requested', 'pausing', 'paused', 'compensating'
    );

CREATE INDEX execution_run_activation_idx
    ON moa.execution_run (activation_state, next_wake_at, updated_at, run_uid)
    WHERE activation_state IN ('queued', 'advancing');

CREATE INDEX execution_run_terminal_retention_idx
    ON moa.execution_run (completed_at, tenant_id, run_uid)
    WHERE status IN ('completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled');

-- Compact, immutable terminal evidence is committed before bulky run detail is
-- paged away. The restrictive run FK deliberately keeps the run identity and
-- this receipt present while task/audit detail is being retained or deleted.
CREATE TABLE moa.execution_terminal_archive (
    archive_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    run_uid UUID NOT NULL,
    contact_id UUID,
    format_version BIGINT NOT NULL CHECK (format_version >= 1),
    terminal_status TEXT NOT NULL CHECK (terminal_status IN (
        'completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
    )),
    terminal_completed_at TIMESTAMPTZ NOT NULL,
    goal_hash TEXT NOT NULL CHECK (goal_hash ~ '^[0-9a-f]{64}$'),
    initial_plan_hash TEXT NOT NULL CHECK (initial_plan_hash ~ '^[0-9a-f]{64}$'),
    active_plan_hash TEXT NOT NULL CHECK (active_plan_hash ~ '^[0-9a-f]{64}$'),
    source_record_count BIGINT NOT NULL DEFAULT 0 CHECK (source_record_count >= 0),
    source_logical_bytes BIGINT NOT NULL DEFAULT 0 CHECK (source_logical_bytes >= 0),
    segment_count BIGINT NOT NULL DEFAULT 0 CHECK (segment_count >= 0),
    source_cursor JSONB NOT NULL DEFAULT '{}'::JSONB CHECK (
        jsonb_typeof(source_cursor) = 'object'
        AND pg_column_size(source_cursor) <= 65536
    ),
    rolling_chain_digest TEXT CHECK (
        rolling_chain_digest IS NULL OR rolling_chain_digest ~ '^[0-9a-f]{64}$'
    ),
    root_digest TEXT CHECK (root_digest IS NULL OR root_digest ~ '^[0-9a-f]{64}$'),
    archive_generation BIGINT NOT NULL DEFAULT 1 CHECK (archive_generation >= 1),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    finalized_at TIMESTAMPTZ,
    details_deleted_at TIMESTAMPTZ,
    CONSTRAINT execution_terminal_archive_run_key UNIQUE (tenant_id, run_uid),
    CONSTRAINT execution_terminal_archive_id_tenant_run_key
        UNIQUE (archive_uid, tenant_id, run_uid),
    CONSTRAINT execution_terminal_archive_run_tenant_fk
        FOREIGN KEY (run_uid, tenant_id)
        REFERENCES moa.execution_run (run_uid, tenant_id) ON DELETE RESTRICT,
    CONSTRAINT execution_terminal_archive_finalization_pair_check CHECK (
        (root_digest IS NULL) = (finalized_at IS NULL)
        AND (finalized_at IS NULL OR finalized_at >= created_at)
        AND (
            finalized_at IS NULL
            OR (source_record_count > 0
                AND source_logical_bytes > 0
                AND segment_count > 0)
        )
        AND (
            (
                segment_count = 0
                AND source_record_count = 0
                AND source_logical_bytes = 0
                AND rolling_chain_digest IS NULL
            )
            OR
            (
                segment_count > 0
                AND source_record_count > 0
                AND source_logical_bytes > 0
                AND rolling_chain_digest IS NOT NULL
            )
        )
        AND (
            details_deleted_at IS NULL
            OR (finalized_at IS NOT NULL AND details_deleted_at >= finalized_at)
        )
    )
);

CREATE INDEX execution_terminal_archive_retention_idx
    ON moa.execution_terminal_archive (
        terminal_completed_at, tenant_id, run_uid
    );

CREATE TABLE moa.execution_terminal_archive_segment (
    archive_uid UUID NOT NULL,
    tenant_id UUID NOT NULL,
    run_uid UUID NOT NULL,
    segment_kind TEXT NOT NULL CHECK (btrim(segment_kind) <> ''),
    segment_sequence BIGINT NOT NULL CHECK (segment_sequence >= 1),
    format_version BIGINT NOT NULL CHECK (format_version >= 1),
    record_count BIGINT NOT NULL CHECK (record_count > 0),
    payload BYTEA NOT NULL CHECK (
        octet_length(payload) BETWEEN 1 AND 4194304
    ),
    content_digest BYTEA NOT NULL CHECK (octet_length(content_digest) = 32),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT execution_terminal_archive_segment_key
        PRIMARY KEY (archive_uid, segment_kind, segment_sequence),
    CONSTRAINT execution_terminal_archive_segment_sequence_key
        UNIQUE (archive_uid, segment_sequence),
    CONSTRAINT execution_terminal_archive_segment_tenant_key
        UNIQUE (archive_uid, tenant_id, segment_kind, segment_sequence),
    CONSTRAINT execution_terminal_archive_segment_manifest_tenant_fk
        FOREIGN KEY (archive_uid, tenant_id, run_uid)
        REFERENCES moa.execution_terminal_archive (archive_uid, tenant_id, run_uid)
        ON DELETE CASCADE
);

CREATE INDEX execution_terminal_archive_segment_scan_idx
    ON moa.execution_terminal_archive_segment (
        tenant_id, archive_uid, segment_kind, segment_sequence
    );

ALTER TABLE moa.execution_run
    ADD COLUMN terminal_archive_uid UUID,
    ADD COLUMN terminal_archive_hash TEXT
        CHECK (terminal_archive_hash IS NULL OR terminal_archive_hash ~ '^[0-9a-f]{64}$'),
    ADD COLUMN terminal_details_archived_at TIMESTAMPTZ,
    ADD CONSTRAINT execution_run_terminal_archive_pair_check CHECK (
        (
            (terminal_archive_uid IS NULL
                AND terminal_archive_hash IS NULL
                AND terminal_details_archived_at IS NULL)
            OR
            (terminal_archive_uid IS NOT NULL
                AND terminal_archive_hash IS NOT NULL
                AND terminal_details_archived_at IS NOT NULL)
        )
    );

ALTER TABLE moa.execution_task
    ADD COLUMN attempt_generation BIGINT,
    ADD COLUMN attempt_state TEXT,
    ADD COLUMN attempt_started_at TIMESTAMPTZ,
    ADD COLUMN last_progress_at TIMESTAMPTZ,
    ADD COLUMN attempt_deadline_at TIMESTAMPTZ,
    ADD COLUMN progress_step_bound_seconds INTEGER CHECK (
        progress_step_bound_seconds IS NULL OR progress_step_bound_seconds > 0
    ),
    ADD COLUMN waiting_since TIMESTAMPTZ,
    ADD COLUMN ready_at TIMESTAMPTZ,
    ADD COLUMN active_dispatch_uid UUID,
    ADD COLUMN dispatch_sequence BIGINT NOT NULL DEFAULT 0
        CHECK (dispatch_sequence >= 0),
    ADD COLUMN external_job_uid UUID,
    ADD COLUMN failure_fingerprint TEXT CHECK (
        failure_fingerprint IS NULL OR failure_fingerprint ~ '^[0-9a-f]{64}$'
    ),
    ADD CONSTRAINT execution_task_id_run_tenant_key
        UNIQUE (task_id, run_uid, tenant_id);

UPDATE moa.execution_task
SET attempt_generation = generation,
    attempt_state = CASE status
        WHEN 'running' THEN 'running'
        WHEN 'unknown_outcome' THEN 'unknown_outcome'
        WHEN 'completed' THEN 'terminal'
        WHEN 'skipped' THEN 'terminal'
        WHEN 'failed' THEN 'terminal'
        WHEN 'cancelled' THEN 'terminal'
        WHEN 'reserved' THEN 'dispatching'
        WHEN 'ready' THEN 'idle'
        WHEN 'dispatching' THEN 'dispatching'
        WHEN 'waiting_input' THEN 'waiting'
        WHEN 'waiting_review' THEN 'waiting'
        WHEN 'waiting_signal' THEN 'waiting'
        WHEN 'waiting_timer' THEN 'waiting'
        WHEN 'waiting_external' THEN 'waiting'
        ELSE 'idle'
    END,
    last_progress_at = updated_at,
    ready_at = CASE WHEN status IN ('pending', 'ready') THEN updated_at END;

ALTER TABLE moa.execution_task
    ALTER COLUMN attempt_generation SET NOT NULL,
    ALTER COLUMN attempt_generation SET DEFAULT 1,
    ALTER COLUMN attempt_state SET NOT NULL,
    ALTER COLUMN attempt_state SET DEFAULT 'idle',
    ALTER COLUMN last_progress_at SET NOT NULL,
    ALTER COLUMN last_progress_at SET DEFAULT now(),
    ADD CONSTRAINT execution_task_attempt_generation_check
        CHECK (attempt_generation >= 1),
    ADD CONSTRAINT execution_task_attempt_state_check CHECK (
        attempt_state IN (
            'idle', 'dispatching', 'running', 'cancelling', 'waiting', 'terminal',
            'unknown_outcome'
        )
    ),
    ADD CONSTRAINT execution_task_attempt_time_order_check CHECK (
        attempt_deadline_at IS NULL
        OR (attempt_started_at IS NOT NULL AND attempt_deadline_at > attempt_started_at)
    );

ALTER TABLE moa.execution_task
    DROP CONSTRAINT execution_task_status_check,
    ADD CONSTRAINT execution_task_status_check CHECK (status IN (
        'pending', 'ready', 'reserved', 'dispatching', 'running',
        'waiting_input', 'waiting_review', 'waiting_signal', 'waiting_timer',
        'waiting_external', 'waiting_replan', 'completed', 'skipped', 'failed',
        'cancelled', 'unknown_outcome'
    )),
    ADD CONSTRAINT execution_task_output_inline_size_check CHECK (
        output IS NULL OR pg_column_size(output) <= 65536
    );

DROP INDEX moa.execution_task_ready_idx;
CREATE INDEX execution_task_ready_idx
    ON moa.execution_task (tenant_id, run_uid, ready_at, node_id, item_key, task_id)
    WHERE status = 'ready';

-- Oldest live attempt, for the stuck-attempt guard. A deadline-keyed index over
-- the same rows was deliberately dropped as unused: nothing orders or filters on
-- attempt_deadline_at, and a btree leading on it could not answer this ordering
-- anyway. This one is backed by a measured index-only scan.
CREATE INDEX execution_task_active_attempt_started_idx
    ON moa.execution_task (attempt_started_at, task_id)
    WHERE status = 'running' AND attempt_state = 'running'
      AND attempt_started_at IS NOT NULL;

-- Fleet admission scans the ready queue per tenant, not per run: the fair-tenant
-- probe tests one tenant's due ready work and the per-item pick orders that
-- tenant's queue by ready_at. Leading on run_uid forces those to sort the whole
-- tenant queue once per admitted item, so give them their own ordered window.
CREATE INDEX execution_task_tenant_ready_order_idx
    ON moa.execution_task (tenant_id, ready_at, task_id)
    WHERE status = 'ready' AND ready_at IS NOT NULL;

CREATE INDEX execution_task_terminal_retention_idx
    ON moa.execution_task (tenant_id, completed_at, run_uid, task_id)
    WHERE status IN ('completed', 'skipped', 'failed', 'cancelled', 'unknown_outcome');

CREATE INDEX execution_task_nonterminal_run_idx
    ON moa.execution_task (run_uid)
    WHERE status NOT IN ('completed', 'skipped', 'failed', 'cancelled', 'unknown_outcome');

CREATE INDEX execution_task_waiting_projection_idx
    ON moa.execution_task (run_uid, waiting_since, task_id)
    WHERE status IN (
        'waiting_input', 'waiting_review', 'waiting_signal', 'waiting_timer',
        'waiting_external', 'waiting_replan'
    ) AND waiting_since IS NOT NULL;

CREATE INDEX execution_task_failure_fingerprint_idx
    ON moa.execution_task (run_uid, failure_fingerprint, task_id)
    WHERE failure_fingerprint IS NOT NULL;

ALTER TABLE moa.execution_compensation
    ADD COLUMN attempt_generation BIGINT NOT NULL DEFAULT 1
        CHECK (attempt_generation >= 1),
    ADD COLUMN attempt_state TEXT NOT NULL DEFAULT 'idle' CHECK (attempt_state IN (
        'idle', 'dispatching', 'running', 'cancelling', 'waiting_review',
        'waiting_external', 'terminal', 'unknown_outcome'
    )),
    ADD COLUMN attempt_started_at TIMESTAMPTZ,
    ADD COLUMN last_progress_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    ADD COLUMN attempt_deadline_at TIMESTAMPTZ,
    ADD COLUMN waiting_since TIMESTAMPTZ,
    ADD COLUMN active_dispatch_uid UUID,
    ADD COLUMN external_job_uid UUID,
    ADD COLUMN release_intent TEXT CHECK (release_intent IN (
        'outcome', 'retry', 'review', 'external_job', 'pause', 'watchdog',
        'deadline', 'run_terminal'
    )),
    ADD COLUMN dispatch_sequence BIGINT NOT NULL DEFAULT 0
        CHECK (dispatch_sequence >= 0),
    ADD CONSTRAINT execution_compensation_id_run_tenant_key
        UNIQUE (compensation_id, run_uid, tenant_id),
    ADD CONSTRAINT execution_compensation_attempt_time_order_check CHECK (
        attempt_deadline_at IS NULL
        OR (attempt_started_at IS NOT NULL AND attempt_deadline_at > attempt_started_at)
    ),
    ADD CONSTRAINT execution_compensation_release_intent_shape_check CHECK (
        (attempt_state = 'cancelling') = (release_intent IS NOT NULL)
    );

UPDATE moa.execution_compensation
SET attempt_generation = generation,
    attempt_state = CASE status
        WHEN 'running' THEN 'running'
        WHEN 'completed' THEN 'terminal'
        WHEN 'failed' THEN 'terminal'
        WHEN 'unknown_outcome' THEN 'unknown_outcome'
        ELSE 'idle'
    END,
    last_progress_at = updated_at;

-- Rollback twin of execution_task_active_attempt_started_idx: a compensation
-- attempt holds the same active-compute reservation, so the stuck-attempt guard
-- takes an ordered minimum from each table and reduces the two rows.
CREATE INDEX execution_compensation_active_attempt_started_idx
    ON moa.execution_compensation (attempt_started_at, compensation_id)
    WHERE status = 'running' AND attempt_state = 'running'
      AND attempt_started_at IS NOT NULL;

CREATE INDEX execution_compensation_terminal_retention_idx
    ON moa.execution_compensation (tenant_id, completed_at, run_uid, compensation_id)
    WHERE status IN ('completed', 'failed', 'unknown_outcome');

CREATE TABLE moa.execution_node_state (
    node_state_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    run_uid UUID NOT NULL,
    node_id TEXT NOT NULL CHECK (btrim(node_id) <> ''),
    node_order BIGINT NOT NULL CHECK (node_order >= 0),
    node_status TEXT NOT NULL DEFAULT 'pending' CHECK (node_status IN (
        'pending', 'ready', 'running', 'waiting', 'completed', 'skipped',
        'failed', 'cancelled'
    )),
    materialization_cursor BIGINT NOT NULL DEFAULT 0 CHECK (materialization_cursor >= 0),
    materialization_complete BOOLEAN NOT NULL DEFAULT FALSE,
    reduce_round BIGINT NOT NULL DEFAULT 1 CHECK (reduce_round >= 1),
    reduce_batch_cursor BIGINT NOT NULL DEFAULT 0 CHECK (reduce_batch_cursor >= 0),
    reduce_round_input_count BIGINT CHECK (reduce_round_input_count >= 0),
    reduce_round_task_count BIGINT NOT NULL DEFAULT 0
        CHECK (reduce_round_task_count >= 0),
    reduce_round_terminal_task_count BIGINT NOT NULL DEFAULT 0
        CHECK (reduce_round_terminal_task_count >= 0),
    dependency_count BIGINT NOT NULL DEFAULT 0 CHECK (dependency_count >= 0),
    remaining_dependency_count BIGINT NOT NULL DEFAULT 0
        CHECK (remaining_dependency_count >= 0),
    aggregate_output JSONB,
    aggregate_output_hash TEXT,
    aggregate_cursor_item_key TEXT CHECK (
        aggregate_cursor_item_key IS NULL
        OR (
            btrim(aggregate_cursor_item_key) <> ''
            AND octet_length(aggregate_cursor_item_key) <= 1024
        )
    ),
    aggregate_complete BOOLEAN NOT NULL DEFAULT FALSE,
    total_task_count BIGINT NOT NULL DEFAULT 0 CHECK (total_task_count >= 0),
    ready_task_count BIGINT NOT NULL DEFAULT 0 CHECK (ready_task_count >= 0),
    active_task_count BIGINT NOT NULL DEFAULT 0 CHECK (active_task_count >= 0),
    waiting_task_count BIGINT NOT NULL DEFAULT 0 CHECK (waiting_task_count >= 0),
    terminal_task_count BIGINT NOT NULL DEFAULT 0 CHECK (terminal_task_count >= 0),
    succeeded_task_count BIGINT NOT NULL DEFAULT 0 CHECK (succeeded_task_count >= 0),
    failed_task_count BIGINT NOT NULL DEFAULT 0 CHECK (failed_task_count >= 0),
    cancelled_task_count BIGINT NOT NULL DEFAULT 0 CHECK (cancelled_task_count >= 0),
    reduce_ready BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT execution_node_state_run_tenant_fk
        FOREIGN KEY (run_uid, tenant_id)
        REFERENCES moa.execution_run (run_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_node_state_run_node_key UNIQUE (run_uid, node_id),
    CONSTRAINT execution_node_state_id_tenant_key UNIQUE (node_state_uid, tenant_id),
    CONSTRAINT execution_node_state_dependency_bounds_check CHECK (
        remaining_dependency_count <= dependency_count
    ),
    CONSTRAINT execution_node_state_aggregate_output_shape_check CHECK (
        (aggregate_output IS NULL AND aggregate_output_hash IS NULL)
        OR (
            aggregate_output IS NOT NULL
            AND aggregate_output_hash ~ '^[0-9a-f]{64}$'
            AND pg_column_size(aggregate_output) <= 1048576
        )
    ),
    CONSTRAINT execution_node_state_reduce_cursor_check CHECK (
        reduce_round_input_count IS NULL
        OR reduce_batch_cursor <= reduce_round_input_count
    ),
    CONSTRAINT execution_node_state_reduce_round_totals_check CHECK (
        reduce_round_terminal_task_count <= reduce_round_task_count
    ),
    CONSTRAINT execution_node_state_task_totals_check CHECK (
        ready_task_count + active_task_count + waiting_task_count + terminal_task_count
        <= total_task_count
        AND succeeded_task_count + failed_task_count + cancelled_task_count
            <= terminal_task_count
    )
);

CREATE INDEX execution_node_state_drive_idx
    ON moa.execution_node_state (
        tenant_id, run_uid, node_status, remaining_dependency_count,
        node_order, node_state_uid
    )
    WHERE node_status IN ('pending', 'ready', 'running', 'waiting');

CREATE INDEX execution_node_state_actionable_idx
    ON moa.execution_node_state (run_uid, updated_at, node_order, node_state_uid)
    WHERE remaining_dependency_count = 0
      AND NOT materialization_complete
      AND node_status NOT IN ('completed', 'skipped', 'failed', 'cancelled');

CREATE INDEX execution_node_state_aggregate_actionable_idx
    ON moa.execution_node_state (run_uid, updated_at, node_order, node_state_uid)
    WHERE materialization_complete
      AND NOT aggregate_complete
      AND node_status NOT IN ('completed', 'skipped', 'failed', 'cancelled');

CREATE OR REPLACE FUNCTION moa.enforce_execution_node_aggregate_cursor_update()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.aggregate_cursor_item_key IS NOT NULL
       AND (
           NEW.aggregate_cursor_item_key IS NULL
           OR NEW.aggregate_cursor_item_key < OLD.aggregate_cursor_item_key
       ) THEN
        RAISE EXCEPTION 'execution node aggregate cursor must be monotonic';
    END IF;
    IF OLD.aggregate_complete AND NOT NEW.aggregate_complete THEN
        RAISE EXCEPTION 'execution node aggregate completion is one-way';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_node_aggregate_cursor_update_guard
BEFORE UPDATE ON moa.execution_node_state
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_node_aggregate_cursor_update();

-- Completion evaluation is itself a bounded, restartable scan. The hot row
-- holds only the current cursor and a capped accumulator, never task history.
CREATE TABLE moa.execution_completion_scan (
    tenant_id UUID NOT NULL,
    run_uid UUID NOT NULL,
    plan_revision BIGINT NOT NULL CHECK (plan_revision >= 1),
    controller_generation BIGINT NOT NULL CHECK (controller_generation >= 1),
    scan_kind TEXT NOT NULL DEFAULT 'ordinary' CHECK (
        scan_kind IN ('ordinary', 'replan_stop')
    ),
    excluded_task_id UUID,
    source_progress_at TIMESTAMPTZ NOT NULL,
    task_cursor UUID,
    node_cursor BIGINT CHECK (node_cursor >= 0),
    scanned_task_count BIGINT NOT NULL DEFAULT 0 CHECK (scanned_task_count >= 0),
    task_evidence JSONB NOT NULL DEFAULT '{}'::JSONB CHECK (
        jsonb_typeof(task_evidence) = 'object'
        AND pg_column_size(task_evidence) <= 1048576
    ),
    scan_complete BOOLEAN NOT NULL DEFAULT FALSE,
    node_scan_complete BOOLEAN NOT NULL DEFAULT FALSE,
    completion_evidence JSONB NOT NULL DEFAULT '{}'::JSONB CHECK (
        jsonb_typeof(completion_evidence) = 'object'
        AND pg_column_size(completion_evidence) <= 1048576
    ),
    verifiers_materialized BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT execution_completion_scan_key PRIMARY KEY (tenant_id, run_uid),
    CONSTRAINT execution_completion_scan_run_tenant_key UNIQUE (run_uid, tenant_id),
    CONSTRAINT execution_completion_scan_run_tenant_fk
        FOREIGN KEY (run_uid, tenant_id)
        REFERENCES moa.execution_run (run_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_completion_scan_excluded_task_tenant_fk
        FOREIGN KEY (excluded_task_id, run_uid, tenant_id)
        REFERENCES moa.execution_task (task_id, run_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_completion_scan_kind_shape_check CHECK (
        (scan_kind = 'ordinary' AND excluded_task_id IS NULL)
        OR (scan_kind = 'replan_stop' AND excluded_task_id IS NOT NULL)
    ),
    CONSTRAINT execution_completion_scan_cursor_check CHECK (
        task_cursor IS NOT NULL OR scanned_task_count = 0 OR scan_complete
    ),
    CONSTRAINT execution_completion_scan_verifier_check CHECK (
        NOT verifiers_materialized OR (scan_complete AND node_scan_complete)
    )
);

CREATE INDEX execution_completion_scan_actionable_idx
    ON moa.execution_completion_scan (updated_at, tenant_id, run_uid)
    WHERE NOT scan_complete OR NOT verifiers_materialized;

CREATE UNIQUE INDEX execution_node_state_run_order_uidx
    ON moa.execution_node_state (run_uid, node_order);

CREATE OR REPLACE FUNCTION moa.enforce_execution_completion_scan_update()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
       OR NEW.run_uid IS DISTINCT FROM OLD.run_uid
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION 'execution completion scan identity is immutable';
    END IF;
    IF NEW.plan_revision < OLD.plan_revision
       OR NEW.controller_generation < OLD.controller_generation
       OR NEW.source_progress_at < OLD.source_progress_at
       OR NEW.updated_at < OLD.updated_at THEN
        RAISE EXCEPTION 'execution completion scan progress must be monotonic';
    END IF;
    IF NEW.plan_revision IS DISTINCT FROM OLD.plan_revision
       OR NEW.controller_generation IS DISTINCT FROM OLD.controller_generation
       OR NEW.source_progress_at IS DISTINCT FROM OLD.source_progress_at THEN
        IF NEW.task_cursor IS NOT NULL
           OR NEW.node_cursor IS NOT NULL
           OR NEW.scanned_task_count <> 0
           OR NEW.task_evidence <> '{}'::JSONB
           OR NEW.completion_evidence <> '{}'::JSONB
           OR NEW.scan_complete
           OR NEW.node_scan_complete
           OR NEW.verifiers_materialized THEN
            RAISE EXCEPTION 'execution completion scan source change requires full reset';
        END IF;
    ELSE
        IF NEW.scan_kind IS DISTINCT FROM OLD.scan_kind
           OR NEW.excluded_task_id IS DISTINCT FROM OLD.excluded_task_id THEN
            RAISE EXCEPTION 'execution completion scan kind is immutable within its source';
        END IF;
        IF NEW.scanned_task_count < OLD.scanned_task_count
           OR (NEW.task_cursor IS DISTINCT FROM OLD.task_cursor
               AND NEW.scanned_task_count <= OLD.scanned_task_count)
           OR (OLD.node_cursor IS NOT NULL
               AND (NEW.node_cursor IS NULL OR NEW.node_cursor < OLD.node_cursor))
           OR (OLD.scan_complete AND NOT NEW.scan_complete)
           OR (OLD.node_scan_complete AND NOT NEW.node_scan_complete)
           OR (OLD.verifiers_materialized AND NOT NEW.verifiers_materialized) THEN
            RAISE EXCEPTION 'execution completion scan progress must be monotonic';
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_completion_scan_update_guard
BEFORE UPDATE ON moa.execution_completion_scan
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_completion_scan_update();

-- One immutable replay receipt replaces recovery-time scans of every task and
-- its JSON audit history. The current amendment contract releases at most the
-- one superseded task; replan-stop records release none.
CREATE TABLE moa.execution_amendment_receipt (
    tenant_id UUID NOT NULL,
    run_uid UUID NOT NULL,
    base_plan_revision BIGINT NOT NULL CHECK (base_plan_revision >= 1),
    amendment_hash TEXT NOT NULL CHECK (amendment_hash ~ '^[0-9a-f]{64}$'),
    receipt_kind TEXT NOT NULL CHECK (receipt_kind IN ('applied', 'replan_stop')),
    superseded_task_id UUID NOT NULL,
    task_generation BIGINT NOT NULL CHECK (task_generation >= 1),
    task_ids_to_release UUID[] NOT NULL DEFAULT '{}'::UUID[] CHECK (
        cardinality(task_ids_to_release) <= 1
    ),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT execution_amendment_receipt_key
        PRIMARY KEY (tenant_id, run_uid, base_plan_revision),
    CONSTRAINT execution_amendment_receipt_run_tenant_key
        UNIQUE (run_uid, tenant_id, base_plan_revision),
    CONSTRAINT execution_amendment_receipt_run_tenant_fk
        FOREIGN KEY (run_uid, tenant_id)
        REFERENCES moa.execution_run (run_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_amendment_receipt_task_tenant_fk
        FOREIGN KEY (superseded_task_id, run_uid, tenant_id)
        REFERENCES moa.execution_task (task_id, run_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_amendment_receipt_release_shape_check CHECK (
        (
            receipt_kind = 'applied'
            AND cardinality(task_ids_to_release) = 1
            AND task_ids_to_release[1] = superseded_task_id
        )
        OR
        (
            receipt_kind = 'replan_stop'
            AND cardinality(task_ids_to_release) = 0
        )
    )
);

CREATE INDEX execution_amendment_receipt_retention_idx
    ON moa.execution_amendment_receipt (
        tenant_id, created_at, run_uid, base_plan_revision
    );

-- Every automatic amendment-planner call first reserves budget, then records
-- its actual usage in a separate immutable settlement. Together these rows
-- preserve the authorization decision even if mutable run counters are later
-- repaired from the ledger.
CREATE TABLE moa.execution_amendment_planning_reservation (
    reservation_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    contact_id UUID,
    contact_scope_id UUID GENERATED ALWAYS AS (
        COALESCE(contact_id, '00000000-0000-0000-0000-000000000000'::UUID)
    ) STORED,
    run_uid UUID NOT NULL,
    base_plan_revision BIGINT NOT NULL CHECK (base_plan_revision >= 1),
    call_ordinal SMALLINT NOT NULL CHECK (call_ordinal BETWEEN 0 AND 255),
    reserved_cost_microusd BIGINT NOT NULL CHECK (reserved_cost_microusd >= 0),
    reserved_tokens BIGINT NOT NULL CHECK (reserved_tokens >= 0),
    created_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT execution_amendment_planning_reservation_contact_not_nil CHECK (
        contact_id IS NULL
        OR contact_id <> '00000000-0000-0000-0000-000000000000'::UUID
    ),
    CONSTRAINT execution_amendment_planning_reservation_run_fkey
        FOREIGN KEY (run_uid, tenant_id, contact_scope_id)
        REFERENCES moa.execution_run (run_uid, tenant_id, contact_scope_id),
    CONSTRAINT execution_amendment_planning_reservation_logical_key
        UNIQUE (run_uid, base_plan_revision, call_ordinal),
    CONSTRAINT execution_amendment_planning_reservation_scope_key
        UNIQUE (reservation_uid, tenant_id, contact_scope_id, run_uid)
);

CREATE TABLE moa.execution_amendment_planning_settlement (
    settlement_uid UUID PRIMARY KEY,
    reservation_uid UUID NOT NULL UNIQUE,
    tenant_id UUID NOT NULL,
    contact_id UUID,
    contact_scope_id UUID GENERATED ALWAYS AS (
        COALESCE(contact_id, '00000000-0000-0000-0000-000000000000'::UUID)
    ) STORED,
    run_uid UUID NOT NULL,
    actual_cost_microusd BIGINT NOT NULL CHECK (actual_cost_microusd >= 0),
    actual_tokens BIGINT NOT NULL CHECK (actual_tokens >= 0),
    budget_overrun BOOLEAN NOT NULL,
    settled_at TIMESTAMPTZ NOT NULL,
    CONSTRAINT execution_amendment_planning_settlement_contact_not_nil CHECK (
        contact_id IS NULL
        OR contact_id <> '00000000-0000-0000-0000-000000000000'::UUID
    ),
    CONSTRAINT execution_amendment_planning_settlement_reservation_fkey
        FOREIGN KEY (reservation_uid, tenant_id, contact_scope_id, run_uid)
        REFERENCES moa.execution_amendment_planning_reservation (
            reservation_uid, tenant_id, contact_scope_id, run_uid
        )
);

-- Planner audits carry exact normalized usage even when later compilation or
-- amendment application fails.
ALTER TABLE moa.execution_planner_call_audit
    ADD COLUMN input_tokens_uncached BIGINT NOT NULL DEFAULT 0
        CHECK (input_tokens_uncached >= 0),
    ADD COLUMN input_tokens_cache_write BIGINT NOT NULL DEFAULT 0
        CHECK (input_tokens_cache_write >= 0),
    ADD COLUMN input_tokens_cache_read BIGINT NOT NULL DEFAULT 0
        CHECK (input_tokens_cache_read >= 0),
    ADD COLUMN output_tokens BIGINT NOT NULL DEFAULT 0
        CHECK (output_tokens >= 0),
    ADD COLUMN cost_microusd BIGINT NOT NULL DEFAULT 0
        CHECK (cost_microusd >= 0);

-- Replan-stop evaluation persists one bounded controller handoff. The exact
-- compensation fence consumes this row atomically; no controller activation
-- rescans task history to reconstruct the decision.
CREATE TABLE moa.execution_replan_stop_intent (
    tenant_id UUID NOT NULL,
    run_uid UUID NOT NULL,
    controller_generation BIGINT NOT NULL CHECK (controller_generation >= 1),
    wake_epoch BIGINT NOT NULL CHECK (wake_epoch >= 1),
    origin_task_id UUID NOT NULL,
    task_generation BIGINT NOT NULL CHECK (task_generation >= 1),
    base_plan_revision BIGINT NOT NULL CHECK (base_plan_revision >= 1),
    stop_reason TEXT NOT NULL CHECK (stop_reason IN (
        'duplicate_plan', 'duplicate_amendment', 'repeated_failure',
        'no_progress', 'deadline_exceeded', 'budget_exhausted'
    )),
    detail TEXT NOT NULL CHECK (octet_length(detail) BETWEEN 1 AND 4096),
    amendment_hash TEXT NOT NULL CHECK (amendment_hash ~ '^[0-9a-f]{64}$'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT execution_replan_stop_intent_key PRIMARY KEY (tenant_id, run_uid),
    CONSTRAINT execution_replan_stop_intent_run_tenant_key UNIQUE (run_uid, tenant_id),
    CONSTRAINT execution_replan_stop_intent_generation_key
        UNIQUE (run_uid, tenant_id, controller_generation, wake_epoch),
    CONSTRAINT execution_replan_stop_intent_run_tenant_fk
        FOREIGN KEY (run_uid, tenant_id)
        REFERENCES moa.execution_run (run_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_replan_stop_intent_task_tenant_fk
        FOREIGN KEY (origin_task_id, run_uid, tenant_id)
        REFERENCES moa.execution_task (task_id, run_uid, tenant_id) ON DELETE CASCADE
);

CREATE INDEX execution_replan_stop_intent_current_idx
    ON moa.execution_replan_stop_intent (
        tenant_id, run_uid, controller_generation, wake_epoch
    );

CREATE TABLE moa.execution_schedule (
    schedule_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    owner_user_id TEXT NOT NULL CHECK (btrim(owner_user_id) <> ''),
    name TEXT NOT NULL CHECK (btrim(name) <> ''),
    timezone TEXT NOT NULL CHECK (btrim(timezone) <> ''),
    calendar_expression TEXT NOT NULL CHECK (btrim(calendar_expression) <> ''),
    template_revision_uid UUID NOT NULL,
    template_snapshot JSONB NOT NULL CHECK (jsonb_typeof(template_snapshot) = 'object'),
    template_hash TEXT NOT NULL CHECK (template_hash ~ '^[0-9a-f]{64}$'),
    run_as_identity JSONB NOT NULL,
    creation_origin JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'paused', 'completed', 'cancelled')),
    missed_fire_policy TEXT NOT NULL
        CHECK (missed_fire_policy IN ('skip', 'fire_once')),
    overlap_policy TEXT NOT NULL
        CHECK (overlap_policy IN ('skip', 'queue_one', 'allow')),
    dst_policy TEXT NOT NULL
        CHECK (dst_policy IN ('earliest', 'latest', 'skip')),
    maximum_concurrent_runs BIGINT NOT NULL DEFAULT 1
        CHECK (maximum_concurrent_runs > 0),
    occurrence_budget JSONB NOT NULL CHECK (jsonb_typeof(occurrence_budget) = 'object'),
    schedule_incarnation BIGINT NOT NULL DEFAULT 1 CHECK (schedule_incarnation >= 1),
    start_at TIMESTAMPTZ NOT NULL,
    next_occurrence_at TIMESTAMPTZ,
    next_occurrence_local TIMESTAMP WITHOUT TIME ZONE,
    last_occurrence_sequence BIGINT NOT NULL DEFAULT 0
        CHECK (last_occurrence_sequence >= 0),
    end_at TIMESTAMPTZ,
    paused_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT execution_schedule_id_tenant_key UNIQUE (schedule_uid, tenant_id),
    CONSTRAINT execution_schedule_run_as_identity_check CHECK (
        moa.execution_admitted_identity_is_valid(run_as_identity, tenant_id)
    ),
    CONSTRAINT execution_schedule_creation_origin_check CHECK (
        moa.execution_schedule_origin_is_valid(creation_origin, tenant_id)
    ),
    CONSTRAINT execution_schedule_next_occurrence_pair_check CHECK (
        (next_occurrence_at IS NULL) = (next_occurrence_local IS NULL)
    ),
    CONSTRAINT execution_schedule_end_order_check CHECK (
        end_at IS NULL OR end_at > start_at
    )
);

CREATE INDEX execution_schedule_due_idx
    ON moa.execution_schedule (
        next_occurrence_at, tenant_id, schedule_uid, schedule_incarnation
    )
    WHERE status = 'active' AND next_occurrence_at IS NOT NULL;

ALTER TABLE moa.execution_run
    ADD COLUMN schedule_uid UUID,
    ADD COLUMN schedule_incarnation BIGINT CHECK (schedule_incarnation >= 1),
    ADD COLUMN schedule_occurrence_sequence BIGINT
        CHECK (schedule_occurrence_sequence >= 1),
    ADD CONSTRAINT execution_run_schedule_tenant_fk
        FOREIGN KEY (schedule_uid, tenant_id)
        REFERENCES moa.execution_schedule (schedule_uid, tenant_id) ON DELETE CASCADE,
    ADD CONSTRAINT execution_run_schedule_occurrence_shape_check CHECK (
        (schedule_uid IS NULL AND schedule_incarnation IS NULL
            AND schedule_occurrence_sequence IS NULL)
        OR
        (schedule_uid IS NOT NULL AND schedule_incarnation IS NOT NULL
            AND schedule_occurrence_sequence IS NOT NULL)
    );

CREATE UNIQUE INDEX execution_run_schedule_occurrence_uidx
    ON moa.execution_run (
        tenant_id, schedule_uid, schedule_incarnation, schedule_occurrence_sequence
    )
    WHERE schedule_uid IS NOT NULL;

CREATE INDEX execution_run_schedule_nonterminal_idx
    ON moa.execution_run (tenant_id, schedule_uid)
    WHERE schedule_uid IS NOT NULL
      AND status NOT IN (
          'completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
      );

CREATE INDEX execution_run_schedule_queued_idx
    ON moa.execution_run (tenant_id, schedule_uid)
    WHERE schedule_uid IS NOT NULL AND status = 'queued';

CREATE TABLE moa.execution_external_job (
    external_job_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    run_uid UUID NOT NULL,
    task_id UUID,
    attempt_generation BIGINT CHECK (attempt_generation >= 1),
    compensation_id UUID,
    compensation_generation BIGINT CHECK (compensation_generation >= 1),
    compensation_attempt_generation BIGINT
        CHECK (compensation_attempt_generation >= 1),
    job_generation BIGINT NOT NULL DEFAULT 1 CHECK (job_generation >= 1),
    declared_provider TEXT NOT NULL CHECK (btrim(declared_provider) <> ''),
    provider TEXT CHECK (provider IS NULL OR btrim(provider) <> ''),
    provider_job_id TEXT CHECK (provider_job_id IS NULL OR btrim(provider_job_id) <> ''),
    idempotency_key TEXT NOT NULL CHECK (octet_length(idempotency_key) BETWEEN 1 AND 256),
    callback_auth_reference TEXT CHECK (
        callback_auth_reference IS NULL OR btrim(callback_auth_reference) <> ''
    ),
    state TEXT NOT NULL CHECK (state IN (
        'unbound', 'starting', 'running', 'waiting_reconcile', 'cancel_requested',
        'completed', 'failed', 'cancelled', 'unknown_outcome'
    )),
    progress_phase TEXT,
    cancel_supported BOOLEAN NOT NULL DEFAULT FALSE,
    next_reconcile_at TIMESTAMPTZ,
    last_provider_event_id TEXT,
    output JSONB,
    error JSONB,
    provider_contract_violation JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    CONSTRAINT execution_external_job_task_tenant_fk
        FOREIGN KEY (task_id, run_uid, tenant_id)
        REFERENCES moa.execution_task (task_id, run_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_external_job_compensation_tenant_fk
        FOREIGN KEY (compensation_id, run_uid, tenant_id)
        REFERENCES moa.execution_compensation (
            compensation_id, run_uid, tenant_id
        ) ON DELETE CASCADE,
    CONSTRAINT execution_external_job_id_tenant_key
        UNIQUE (external_job_uid, tenant_id),
    CONSTRAINT execution_external_job_owner_shape_check CHECK (
        (
            task_id IS NOT NULL AND attempt_generation IS NOT NULL
            AND compensation_id IS NULL
            AND compensation_generation IS NULL
            AND compensation_attempt_generation IS NULL
        )
        OR
        (
            task_id IS NULL AND attempt_generation IS NULL
            AND compensation_id IS NOT NULL
            AND compensation_generation IS NOT NULL
            AND compensation_attempt_generation IS NOT NULL
        )
    ),
    CONSTRAINT execution_external_job_terminal_shape_check CHECK (
        (state IN ('completed', 'failed', 'cancelled', 'unknown_outcome'))
        = (completed_at IS NOT NULL)
    ),
    CONSTRAINT execution_external_job_binding_shape_check CHECK (
        (
            state = 'unbound'
            AND provider IS NULL
            AND provider_job_id IS NULL
            AND callback_auth_reference IS NULL
            AND progress_phase IS NULL
            AND NOT cancel_supported
            AND next_reconcile_at IS NULL
            AND last_provider_event_id IS NULL
            AND output IS NULL
            AND error IS NULL
            AND completed_at IS NULL
        )
        OR
        (
            state <> 'unbound'
            AND provider IS NOT NULL
            AND provider = declared_provider
            AND provider_job_id IS NOT NULL
            AND callback_auth_reference IS NOT NULL
        )
    ),
    CONSTRAINT execution_external_job_contract_violation_shape_check CHECK (
        provider_contract_violation IS NULL
        OR (
            state IN (
                'cancel_requested', 'completed', 'failed', 'cancelled',
                'unknown_outcome'
            )
            AND jsonb_typeof(provider_contract_violation) = 'object'
            AND pg_column_size(provider_contract_violation) <= 16384
            AND moa.execution_json_object_has_exact_keys(
                provider_contract_violation, ARRAY['kind', 'observed_at', 'detail']
            )
            AND provider_contract_violation ->> 'kind' = 'provider_contract_mismatch'
            AND btrim(provider_contract_violation ->> 'observed_at') <> ''
            AND octet_length(provider_contract_violation ->> 'detail') BETWEEN 1 AND 4096
            AND (state <> 'cancel_requested' OR next_reconcile_at IS NOT NULL)
        )
    )
);

CREATE UNIQUE INDEX execution_external_job_provider_identity_key
    ON moa.execution_external_job (
        tenant_id, provider, provider_job_id, job_generation
    )
    WHERE provider IS NOT NULL;

CREATE UNIQUE INDEX execution_external_job_task_attempt_uidx
    ON moa.execution_external_job (
        tenant_id, run_uid, task_id, attempt_generation
    )
    WHERE task_id IS NOT NULL;

CREATE UNIQUE INDEX execution_external_job_compensation_attempt_uidx
    ON moa.execution_external_job (
        tenant_id, run_uid, compensation_id,
        compensation_generation, compensation_attempt_generation
    )
    WHERE compensation_id IS NOT NULL;

CREATE UNIQUE INDEX execution_external_job_callback_dedupe_uidx
    ON moa.execution_external_job (
        tenant_id, provider, last_provider_event_id, job_generation
    )
    WHERE last_provider_event_id IS NOT NULL;

CREATE INDEX execution_external_job_reconcile_idx
    ON moa.execution_external_job (next_reconcile_at, tenant_id, external_job_uid)
    WHERE state IN ('starting', 'running', 'waiting_reconcile', 'cancel_requested')
      AND next_reconcile_at IS NOT NULL;

CREATE OR REPLACE FUNCTION moa.enforce_execution_external_job_update()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    transition_allowed BOOLEAN;
BEGIN
    IF NEW.external_job_uid IS DISTINCT FROM OLD.external_job_uid
       OR NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
       OR NEW.run_uid IS DISTINCT FROM OLD.run_uid
       OR NEW.task_id IS DISTINCT FROM OLD.task_id
       OR NEW.attempt_generation IS DISTINCT FROM OLD.attempt_generation
       OR NEW.compensation_id IS DISTINCT FROM OLD.compensation_id
       OR NEW.compensation_generation IS DISTINCT FROM OLD.compensation_generation
       OR NEW.compensation_attempt_generation
            IS DISTINCT FROM OLD.compensation_attempt_generation
       OR NEW.job_generation IS DISTINCT FROM OLD.job_generation
       OR NEW.declared_provider IS DISTINCT FROM OLD.declared_provider
       OR NEW.idempotency_key IS DISTINCT FROM OLD.idempotency_key
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION 'execution external job owner and generation are immutable';
    END IF;
    IF OLD.state <> 'unbound'
       AND (
           NEW.provider IS DISTINCT FROM OLD.provider
           OR NEW.provider_job_id IS DISTINCT FROM OLD.provider_job_id
           OR NEW.callback_auth_reference IS DISTINCT FROM OLD.callback_auth_reference
       ) THEN
        RAISE EXCEPTION 'bound execution external job provider identity is immutable';
    END IF;
    IF OLD.provider_contract_violation IS NOT NULL
       AND NEW.provider_contract_violation IS DISTINCT FROM OLD.provider_contract_violation THEN
        RAISE EXCEPTION 'execution external job contract violation evidence is immutable';
    END IF;
    IF OLD.state IN ('completed', 'failed', 'cancelled', 'unknown_outcome')
       AND NEW IS DISTINCT FROM OLD THEN
        RAISE EXCEPTION 'terminal execution external job is immutable';
    END IF;
    transition_allowed := CASE OLD.state
        WHEN 'unbound' THEN NEW.state IN (
            'unbound', 'starting', 'running', 'waiting_reconcile',
            'completed', 'failed', 'cancelled', 'unknown_outcome'
        )
        WHEN 'starting' THEN NEW.state IN (
            'starting', 'running', 'waiting_reconcile', 'cancel_requested',
            'completed', 'failed', 'cancelled', 'unknown_outcome'
        )
        WHEN 'running' THEN NEW.state IN (
            'running', 'waiting_reconcile', 'cancel_requested',
            'completed', 'failed', 'cancelled', 'unknown_outcome'
        )
        WHEN 'waiting_reconcile' THEN NEW.state IN (
            'running', 'waiting_reconcile', 'cancel_requested',
            'completed', 'failed', 'cancelled', 'unknown_outcome'
        )
        WHEN 'cancel_requested' THEN NEW.state IN (
            'cancel_requested', 'completed', 'failed', 'cancelled', 'unknown_outcome'
        )
        ELSE NEW.state = OLD.state
    END;
    IF NOT transition_allowed THEN
        RAISE EXCEPTION 'invalid execution external job state transition: % -> %',
            OLD.state, NEW.state;
    END IF;
    IF NEW.updated_at < OLD.updated_at THEN
        RAISE EXCEPTION 'execution external job updated_at must be monotonic';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_external_job_update_guard
BEFORE UPDATE ON moa.execution_external_job
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_external_job_update();

CREATE TABLE moa.execution_external_job_callback_receipt (
    tenant_id UUID NOT NULL,
    external_job_uid UUID NOT NULL,
    provider TEXT NOT NULL CHECK (btrim(provider) <> ''),
    provider_event_id TEXT NOT NULL CHECK (
        octet_length(provider_event_id) BETWEEN 1 AND 512
    ),
    job_generation BIGINT NOT NULL CHECK (job_generation >= 1),
    received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT execution_external_job_callback_receipt_identity_key
        PRIMARY KEY (
            tenant_id, external_job_uid, job_generation, provider, provider_event_id
        ),
    CONSTRAINT execution_external_job_callback_receipt_job_tenant_fk
        FOREIGN KEY (external_job_uid, tenant_id)
        REFERENCES moa.execution_external_job (external_job_uid, tenant_id)
        ON DELETE CASCADE
);

CREATE INDEX execution_external_job_callback_receipt_retention_idx
    ON moa.execution_external_job_callback_receipt (
        tenant_id, received_at, external_job_uid, job_generation
    );

ALTER TABLE moa.execution_task
    ADD CONSTRAINT execution_task_external_job_tenant_fk
        FOREIGN KEY (external_job_uid, tenant_id)
        REFERENCES moa.execution_external_job (external_job_uid, tenant_id)
        ON DELETE SET NULL (external_job_uid);

ALTER TABLE moa.execution_compensation
    ADD CONSTRAINT execution_compensation_external_job_tenant_fk
        FOREIGN KEY (external_job_uid, tenant_id)
        REFERENCES moa.execution_external_job (external_job_uid, tenant_id)
        ON DELETE SET NULL (external_job_uid);

CREATE TABLE moa.execution_trigger (
    trigger_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    run_uid UUID,
    task_id UUID,
    compensation_id UUID,
    schedule_uid UUID,
    schedule_incarnation BIGINT CHECK (schedule_incarnation >= 1),
    trigger_kind TEXT NOT NULL CHECK (trigger_kind IN (
        'run_deadline', 'task_timer', 'wait_expiry', 'task_watchdog',
        'external_reconcile', 'external_start_recovery', 'schedule_occurrence',
        'compensation_watchdog'
    )),
    -- Triggers are never claimed and never dead-letter. Claiming, retry accounting, and
    -- dead-lettering live entirely on moa.execution_dispatch_outbox, and a trigger
    -- delivery always requires durable retry, so a trigger row only ever settles.
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN (
        'pending', 'delivered', 'superseded', 'cancelled'
    )),
    controller_generation BIGINT CHECK (controller_generation >= 1),
    attempt_generation BIGINT CHECK (attempt_generation >= 1),
    compensation_generation BIGINT CHECK (compensation_generation >= 1),
    compensation_attempt_generation BIGINT
        CHECK (compensation_attempt_generation >= 1),
    occurrence_sequence BIGINT CHECK (occurrence_sequence >= 1),
    due_at TIMESTAMPTZ NOT NULL,
    payload JSONB NOT NULL DEFAULT '{}'::JSONB CHECK (jsonb_typeof(payload) = 'object'),
    delivered_at TIMESTAMPTZ,
    -- Operator-facing only: rearm_external_start_recovery records why a start-recovery
    -- trigger keeps rearming. Nothing in Rust reads it back.
    last_error TEXT CHECK (last_error IS NULL OR octet_length(last_error) <= 4096),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT execution_trigger_id_tenant_key UNIQUE (trigger_uid, tenant_id),
    CONSTRAINT execution_trigger_run_tenant_fk
        FOREIGN KEY (run_uid, tenant_id)
        REFERENCES moa.execution_run (run_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_trigger_task_tenant_fk
        FOREIGN KEY (task_id, run_uid, tenant_id)
        REFERENCES moa.execution_task (task_id, run_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_trigger_compensation_tenant_fk
        FOREIGN KEY (compensation_id, run_uid, tenant_id)
        REFERENCES moa.execution_compensation (
            compensation_id, run_uid, tenant_id
        ) ON DELETE CASCADE,
    CONSTRAINT execution_trigger_schedule_tenant_fk
        FOREIGN KEY (schedule_uid, tenant_id)
        REFERENCES moa.execution_schedule (schedule_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_trigger_target_shape_check CHECK (
        (
            trigger_kind = 'schedule_occurrence'
            AND schedule_uid IS NOT NULL
            AND schedule_incarnation IS NOT NULL
            AND occurrence_sequence IS NOT NULL
            AND run_uid IS NULL
            AND task_id IS NULL
            AND compensation_id IS NULL
            AND controller_generation IS NULL
            AND attempt_generation IS NULL
            AND compensation_generation IS NULL
            AND compensation_attempt_generation IS NULL
        ) OR (
            trigger_kind = 'run_deadline'
            AND run_uid IS NOT NULL
            AND task_id IS NULL
            AND compensation_id IS NULL
            AND schedule_uid IS NULL
            AND schedule_incarnation IS NULL
            AND controller_generation IS NOT NULL
            AND attempt_generation IS NULL
            AND compensation_generation IS NULL
            AND compensation_attempt_generation IS NULL
            AND occurrence_sequence IS NULL
        ) OR (
            trigger_kind IN (
                'task_timer', 'wait_expiry', 'task_watchdog'
            )
            AND run_uid IS NOT NULL
            AND task_id IS NOT NULL
            AND compensation_id IS NULL
            AND schedule_uid IS NULL
            AND schedule_incarnation IS NULL
            AND controller_generation IS NOT NULL
            AND attempt_generation IS NOT NULL
            AND compensation_generation IS NULL
            AND compensation_attempt_generation IS NULL
            AND occurrence_sequence IS NULL
        ) OR (
            trigger_kind IN ('external_reconcile', 'external_start_recovery')
            AND run_uid IS NOT NULL
            AND schedule_uid IS NULL
            AND schedule_incarnation IS NULL
            AND controller_generation IS NOT NULL
            AND occurrence_sequence IS NULL
            AND (
                (
                    task_id IS NOT NULL
                    AND attempt_generation IS NOT NULL
                    AND compensation_id IS NULL
                    AND compensation_generation IS NULL
                    AND compensation_attempt_generation IS NULL
                )
                OR
                (
                    task_id IS NULL
                    AND attempt_generation IS NULL
                    AND compensation_id IS NOT NULL
                    AND compensation_generation IS NOT NULL
                    AND compensation_attempt_generation IS NOT NULL
                )
            )
        ) OR (
            trigger_kind = 'compensation_watchdog'
            AND run_uid IS NOT NULL
            AND task_id IS NULL
            AND compensation_id IS NOT NULL
            AND schedule_uid IS NULL
            AND schedule_incarnation IS NULL
            AND controller_generation IS NOT NULL
            AND attempt_generation IS NULL
            AND compensation_generation IS NOT NULL
            AND compensation_attempt_generation IS NOT NULL
            AND occurrence_sequence IS NULL
        )
    ),
    CONSTRAINT execution_trigger_delivery_pair_check CHECK (
        (state = 'delivered') = (delivered_at IS NOT NULL)
    ),
    CONSTRAINT execution_trigger_start_recovery_shape_check CHECK (
        trigger_kind <> 'external_start_recovery'
        OR (
            moa.execution_json_object_has_exact_keys(
                payload, ARRAY[
                    'external_job_uid', 'job_generation', 'declared_provider',
                    'idempotency_key'
                ]
            )
            AND payload ->> 'external_job_uid' ~
                '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
            AND payload ->> 'job_generation' ~ '^[1-9][0-9]*$'
            AND octet_length(payload ->> 'declared_provider') BETWEEN 1 AND 256
            AND octet_length(payload ->> 'idempotency_key') BETWEEN 1 AND 256
        )
    )
);

CREATE UNIQUE INDEX execution_trigger_current_run_generation_uidx
    ON moa.execution_trigger (
        tenant_id, run_uid,
        COALESCE(task_id, compensation_id, '00000000-0000-0000-0000-000000000000'::UUID),
        trigger_kind, controller_generation, COALESCE(attempt_generation, 0),
        COALESCE(compensation_generation, 0),
        COALESCE(compensation_attempt_generation, 0)
    )
    WHERE state = 'pending' AND run_uid IS NOT NULL;

CREATE UNIQUE INDEX execution_trigger_schedule_occurrence_uidx
    ON moa.execution_trigger (
        tenant_id, schedule_uid, schedule_incarnation, occurrence_sequence
    )
    WHERE trigger_kind = 'schedule_occurrence';

CREATE INDEX execution_trigger_due_idx
    ON moa.execution_trigger (due_at, tenant_id, trigger_uid)
    WHERE state = 'pending';

CREATE INDEX execution_trigger_run_wake_idx
    ON moa.execution_trigger (run_uid, due_at, trigger_uid)
    WHERE run_uid IS NOT NULL AND state = 'pending';

CREATE OR REPLACE FUNCTION moa.execution_attempt_cancel_payload_is_valid(
    candidate JSONB,
    expected_kind TEXT,
    expected_dispatch_uid UUID,
    expected_tenant_id UUID,
    expected_run_uid UUID,
    expected_owner_uid UUID,
    expected_controller_generation BIGINT,
    expected_attempt_generation BIGINT,
    expected_compensation_generation BIGINT
) RETURNS BOOLEAN
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT CASE expected_kind
        WHEN 'task_attempt_cancel' THEN
            moa.execution_json_object_has_exact_keys(candidate, ARRAY[
                'dispatch_uid', 'tenant_id', 'run_uid', 'task_id',
                'controller_generation', 'attempt_controller_generation',
                'task_generation', 'attempt_generation',
                'active_dispatch_uid', 'capacity_reservation_uid',
                'watchdog_trigger_uid', 'reason'
            ])
            AND candidate ->> 'task_id' = expected_owner_uid::TEXT
            AND (candidate ->> 'task_generation') ~ '^[1-9][0-9]*$'
            AND (candidate ->> 'attempt_generation')::BIGINT
                = expected_attempt_generation
        WHEN 'compensation_attempt_cancel' THEN
            moa.execution_json_object_has_exact_keys(candidate, ARRAY[
                'dispatch_uid', 'tenant_id', 'run_uid', 'compensation_id',
                'controller_generation', 'attempt_controller_generation',
                'compensation_generation',
                'compensation_attempt_generation', 'active_dispatch_uid',
                'capacity_reservation_uid', 'watchdog_trigger_uid', 'intent'
            ])
            AND candidate ->> 'compensation_id' = expected_owner_uid::TEXT
            AND (candidate ->> 'compensation_generation')::BIGINT
                = expected_compensation_generation
            AND (candidate ->> 'compensation_attempt_generation')::BIGINT
                = expected_attempt_generation
        ELSE FALSE
    END
    AND candidate ->> 'dispatch_uid' = expected_dispatch_uid::TEXT
    AND candidate ->> 'tenant_id' = expected_tenant_id::TEXT
    AND candidate ->> 'run_uid' = expected_run_uid::TEXT
    AND (candidate ->> 'controller_generation')::BIGINT
        = expected_controller_generation
    AND candidate ->> 'attempt_controller_generation' ~ '^[1-9][0-9]*$'
    AND candidate ->> 'active_dispatch_uid' ~
        '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
    AND candidate ->> 'capacity_reservation_uid' ~
        '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
    AND candidate ->> 'watchdog_trigger_uid' ~
        '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
    AND octet_length(
        candidate ->> CASE expected_kind
            WHEN 'task_attempt_cancel' THEN 'reason'
            WHEN 'compensation_attempt_cancel' THEN 'intent'
        END
    ) BETWEEN 1 AND 512
$$;

CREATE TABLE moa.execution_dispatch_outbox (
    dispatch_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    run_uid UUID,
    task_id UUID,
    compensation_id UUID,
    trigger_uid UUID,
    external_job_uid UUID,
    dispatch_kind TEXT NOT NULL CHECK (dispatch_kind IN (
        'run_activation', 'task_attempt', 'compensation_attempt',
        'task_attempt_cancel', 'compensation_attempt_cancel',
        'trigger_delivery', 'external_cancel'
    )),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN (
        'pending', 'dispatching', 'delivered', 'superseded', 'cancelled', 'dead_letter'
    )),
    controller_generation BIGINT CHECK (controller_generation >= 1),
    wake_epoch BIGINT CHECK (wake_epoch >= 1),
    attempt_generation BIGINT CHECK (attempt_generation >= 1),
    compensation_generation BIGINT CHECK (compensation_generation >= 1),
    compensation_attempt_generation BIGINT
        CHECK (compensation_attempt_generation >= 1),
    not_before_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    payload JSONB NOT NULL DEFAULT '{}'::JSONB CHECK (jsonb_typeof(payload) = 'object'),
    -- Restate persists a completed invocation's response and replays it for any
    -- later request carrying the same idempotency key. A reconciler repair that
    -- returned a row to `pending` under its original `dispatch_uid` would attach
    -- to that memoized completion instead of re-executing the target. This
    -- counter gives each repair a distinct delivery identity to fold into the
    -- key; it advances only on repair, so ordinary redelivery stays idempotent.
    repair_epoch INTEGER NOT NULL DEFAULT 0 CHECK (repair_epoch >= 0),
    claim_owner TEXT,
    claimed_at TIMESTAMPTZ,
    claim_expires_at TIMESTAMPTZ,
    delivery_attempts INTEGER NOT NULL DEFAULT 0 CHECK (delivery_attempts >= 0),
    delivered_at TIMESTAMPTZ,
    last_error TEXT CHECK (last_error IS NULL OR octet_length(last_error) <= 4096),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT execution_dispatch_outbox_id_tenant_key
        UNIQUE (dispatch_uid, tenant_id),
    CONSTRAINT execution_dispatch_outbox_task_identity_key
        UNIQUE (dispatch_uid, tenant_id, run_uid, task_id),
    CONSTRAINT execution_dispatch_outbox_compensation_identity_key
        UNIQUE (dispatch_uid, tenant_id, run_uid, compensation_id),
    CONSTRAINT execution_dispatch_outbox_run_tenant_fk
        FOREIGN KEY (run_uid, tenant_id)
        REFERENCES moa.execution_run (run_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_dispatch_outbox_task_tenant_fk
        FOREIGN KEY (task_id, run_uid, tenant_id)
        REFERENCES moa.execution_task (task_id, run_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_dispatch_outbox_compensation_tenant_fk
        FOREIGN KEY (compensation_id, run_uid, tenant_id)
        REFERENCES moa.execution_compensation (
            compensation_id, run_uid, tenant_id
        ) ON DELETE CASCADE,
    CONSTRAINT execution_dispatch_outbox_trigger_tenant_fk
        FOREIGN KEY (trigger_uid, tenant_id)
        REFERENCES moa.execution_trigger (trigger_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_dispatch_outbox_external_job_tenant_fk
        FOREIGN KEY (external_job_uid, tenant_id)
        REFERENCES moa.execution_external_job (external_job_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_dispatch_outbox_target_shape_check CHECK (
        (
            dispatch_kind = 'run_activation'
            AND run_uid IS NOT NULL AND task_id IS NULL AND trigger_uid IS NULL
            AND compensation_id IS NULL AND external_job_uid IS NULL
            AND controller_generation IS NOT NULL
            AND wake_epoch IS NOT NULL AND attempt_generation IS NULL
            AND compensation_generation IS NULL
            AND compensation_attempt_generation IS NULL
        ) OR (
            dispatch_kind = 'task_attempt'
            AND run_uid IS NOT NULL AND task_id IS NOT NULL AND trigger_uid IS NULL
            AND compensation_id IS NULL AND external_job_uid IS NULL
            AND controller_generation IS NOT NULL
            AND wake_epoch IS NULL AND attempt_generation IS NOT NULL
            AND compensation_generation IS NULL
            AND compensation_attempt_generation IS NULL
        ) OR (
            dispatch_kind = 'compensation_attempt'
            AND run_uid IS NOT NULL AND task_id IS NULL AND trigger_uid IS NULL
            AND compensation_id IS NOT NULL AND external_job_uid IS NULL
            AND controller_generation IS NOT NULL AND wake_epoch IS NULL
            AND attempt_generation IS NULL AND compensation_generation IS NOT NULL
            AND compensation_attempt_generation IS NOT NULL
        ) OR (
            dispatch_kind = 'task_attempt_cancel'
            AND run_uid IS NOT NULL AND task_id IS NOT NULL AND trigger_uid IS NULL
            AND compensation_id IS NULL AND external_job_uid IS NULL
            AND controller_generation IS NOT NULL
            AND wake_epoch IS NULL AND attempt_generation IS NOT NULL
            AND compensation_generation IS NULL
            AND compensation_attempt_generation IS NULL
            AND moa.execution_attempt_cancel_payload_is_valid(
                payload, dispatch_kind, dispatch_uid, tenant_id, run_uid, task_id,
                controller_generation, attempt_generation, NULL
            )
        ) OR (
            dispatch_kind = 'compensation_attempt_cancel'
            AND run_uid IS NOT NULL AND task_id IS NULL AND trigger_uid IS NULL
            AND compensation_id IS NOT NULL AND external_job_uid IS NULL
            AND controller_generation IS NOT NULL AND wake_epoch IS NULL
            AND attempt_generation IS NULL AND compensation_generation IS NOT NULL
            AND compensation_attempt_generation IS NOT NULL
            AND moa.execution_attempt_cancel_payload_is_valid(
                payload, dispatch_kind, dispatch_uid, tenant_id, run_uid,
                compensation_id, controller_generation,
                compensation_attempt_generation, compensation_generation
            )
        ) OR (
            dispatch_kind = 'trigger_delivery'
            AND trigger_uid IS NOT NULL AND run_uid IS NULL AND task_id IS NULL
            AND compensation_id IS NULL AND external_job_uid IS NULL
            AND controller_generation IS NULL AND wake_epoch IS NULL
            AND attempt_generation IS NULL AND compensation_generation IS NULL
            AND compensation_attempt_generation IS NULL
        ) OR (
            dispatch_kind = 'external_cancel'
            AND run_uid IS NOT NULL AND trigger_uid IS NULL
            AND external_job_uid IS NOT NULL
            AND controller_generation IS NOT NULL AND wake_epoch IS NULL
            AND (
                (
                    task_id IS NOT NULL AND attempt_generation IS NOT NULL
                    AND compensation_id IS NULL
                    AND compensation_generation IS NULL
                    AND compensation_attempt_generation IS NULL
                )
                OR
                (
                    task_id IS NULL AND attempt_generation IS NULL
                    AND compensation_id IS NOT NULL
                    AND compensation_generation IS NOT NULL
                    AND compensation_attempt_generation IS NOT NULL
                )
            )
        )
    ),
    CONSTRAINT execution_dispatch_outbox_claim_pair_check CHECK (
        (claim_owner IS NULL AND claimed_at IS NULL AND claim_expires_at IS NULL)
        OR (
            claim_owner IS NOT NULL AND btrim(claim_owner) <> ''
            AND claimed_at IS NOT NULL AND claim_expires_at IS NOT NULL
            AND claim_expires_at > claimed_at
        )
    ),
    CONSTRAINT execution_dispatch_outbox_delivery_pair_check CHECK (
        (state = 'delivered') = (delivered_at IS NOT NULL)
    )
);

CREATE UNIQUE INDEX execution_dispatch_outbox_run_activation_uidx
    ON moa.execution_dispatch_outbox (
        tenant_id, run_uid, controller_generation, wake_epoch, dispatch_kind
    )
    WHERE dispatch_kind = 'run_activation';

CREATE UNIQUE INDEX execution_dispatch_outbox_task_attempt_uidx
    ON moa.execution_dispatch_outbox (
        tenant_id, run_uid, task_id, attempt_generation, dispatch_kind
    )
    WHERE dispatch_kind = 'task_attempt';

CREATE UNIQUE INDEX execution_dispatch_outbox_compensation_attempt_uidx
    ON moa.execution_dispatch_outbox (
        tenant_id, run_uid, compensation_id, compensation_generation,
        compensation_attempt_generation, dispatch_kind
    )
    WHERE dispatch_kind = 'compensation_attempt';

CREATE UNIQUE INDEX execution_dispatch_outbox_task_attempt_cancel_uidx
    ON moa.execution_dispatch_outbox (
        tenant_id, run_uid, task_id, controller_generation,
        attempt_generation, dispatch_kind,
        (payload ->> 'active_dispatch_uid'), (payload ->> 'reason')
    )
    WHERE dispatch_kind = 'task_attempt_cancel';

CREATE UNIQUE INDEX execution_dispatch_outbox_compensation_attempt_cancel_uidx
    ON moa.execution_dispatch_outbox (
        tenant_id, run_uid, compensation_id, controller_generation,
        compensation_generation, compensation_attempt_generation, dispatch_kind,
        (payload ->> 'active_dispatch_uid'), (payload ->> 'intent')
    )
    WHERE dispatch_kind = 'compensation_attempt_cancel';

CREATE UNIQUE INDEX execution_dispatch_outbox_trigger_delivery_uidx
    ON moa.execution_dispatch_outbox (tenant_id, trigger_uid, dispatch_kind)
    WHERE dispatch_kind = 'trigger_delivery';

CREATE UNIQUE INDEX execution_dispatch_outbox_external_cancel_uidx
    ON moa.execution_dispatch_outbox (tenant_id, external_job_uid, dispatch_kind)
    WHERE dispatch_kind = 'external_cancel';

CREATE INDEX execution_dispatch_outbox_pending_idx
    ON moa.execution_dispatch_outbox (not_before_at, created_at, dispatch_uid)
    WHERE state = 'pending';

CREATE INDEX execution_dispatch_outbox_claim_expiry_idx
    ON moa.execution_dispatch_outbox (claim_expires_at, created_at, dispatch_uid)
    WHERE state = 'dispatching';

CREATE INDEX execution_dispatch_outbox_dead_letter_idx
    ON moa.execution_dispatch_outbox (created_at, tenant_id, dispatch_uid)
    WHERE state = 'dead_letter';

ALTER TABLE moa.execution_task
    ADD CONSTRAINT execution_task_active_dispatch_tenant_fk
        FOREIGN KEY (active_dispatch_uid, tenant_id, run_uid, task_id)
        REFERENCES moa.execution_dispatch_outbox (
            dispatch_uid, tenant_id, run_uid, task_id
        )
        ON DELETE SET NULL (active_dispatch_uid);

ALTER TABLE moa.execution_compensation
    ADD CONSTRAINT execution_compensation_active_dispatch_tenant_fk
        FOREIGN KEY (active_dispatch_uid, tenant_id, run_uid, compensation_id)
        REFERENCES moa.execution_dispatch_outbox (
            dispatch_uid, tenant_id, run_uid, compensation_id
        )
        ON DELETE SET NULL (active_dispatch_uid);

-- Agent and direct capability-review continuations are bounded snapshots, not
-- live workflow journals. Replacing the current checkpoint only supersedes the
-- prior immutable row; historical rows remain page-retainable.
CREATE TABLE moa.execution_task_checkpoint (
    checkpoint_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    run_uid UUID NOT NULL,
    task_id UUID NOT NULL,
    checkpoint_sequence BIGINT NOT NULL CHECK (checkpoint_sequence >= 1),
    controller_generation BIGINT CHECK (controller_generation >= 1),
    task_generation BIGINT NOT NULL CHECK (task_generation >= 1),
    attempt_generation BIGINT NOT NULL CHECK (attempt_generation >= 1),
    dispatch_uid UUID NOT NULL,
    checkpoint_kind TEXT NOT NULL CHECK (checkpoint_kind IN (
        'agent_continuation', 'capability_review', 'capability_external_start'
    )),
    schema_version BIGINT NOT NULL CHECK (schema_version >= 1),
    payload JSONB NOT NULL CHECK (
        jsonb_typeof(payload) = 'object'
        AND pg_column_size(payload) <= 1048576
    ),
    payload_hash TEXT NOT NULL CHECK (payload_hash ~ '^[0-9a-f]{64}$'),
    workspace_release_receipt JSONB CHECK (
        workspace_release_receipt IS NULL
        OR (
            jsonb_typeof(workspace_release_receipt) = 'object'
            AND pg_column_size(workspace_release_receipt) <= 262144
        )
    ),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    superseded_at TIMESTAMPTZ,
    CONSTRAINT execution_task_checkpoint_id_tenant_key
        UNIQUE (checkpoint_uid, tenant_id),
    CONSTRAINT execution_task_checkpoint_sequence_key
        UNIQUE (tenant_id, run_uid, task_id, checkpoint_sequence),
    CONSTRAINT execution_task_checkpoint_task_tenant_fk
        FOREIGN KEY (task_id, run_uid, tenant_id)
        REFERENCES moa.execution_task (task_id, run_uid, tenant_id)
        ON DELETE CASCADE,
    CONSTRAINT execution_task_checkpoint_dispatch_tenant_fk
        FOREIGN KEY (dispatch_uid, tenant_id, run_uid, task_id)
        REFERENCES moa.execution_dispatch_outbox (
            dispatch_uid, tenant_id, run_uid, task_id
        ) ON DELETE RESTRICT,
    CONSTRAINT execution_task_checkpoint_supersession_order_check CHECK (
        superseded_at IS NULL OR superseded_at >= created_at
    )
);

CREATE UNIQUE INDEX execution_task_checkpoint_current_uidx
    ON moa.execution_task_checkpoint (tenant_id, run_uid, task_id)
    WHERE superseded_at IS NULL;

CREATE INDEX execution_task_checkpoint_retention_idx
    ON moa.execution_task_checkpoint (
        superseded_at, created_at, tenant_id, run_uid, task_id, checkpoint_sequence
    )
    WHERE superseded_at IS NOT NULL;

CREATE OR REPLACE FUNCTION moa.enforce_execution_task_checkpoint_update()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.checkpoint_uid IS DISTINCT FROM OLD.checkpoint_uid
       OR NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
       OR NEW.run_uid IS DISTINCT FROM OLD.run_uid
       OR NEW.task_id IS DISTINCT FROM OLD.task_id
       OR NEW.checkpoint_sequence IS DISTINCT FROM OLD.checkpoint_sequence
       OR NEW.controller_generation IS DISTINCT FROM OLD.controller_generation
       OR NEW.task_generation IS DISTINCT FROM OLD.task_generation
       OR NEW.attempt_generation IS DISTINCT FROM OLD.attempt_generation
       OR NEW.dispatch_uid IS DISTINCT FROM OLD.dispatch_uid
       OR NEW.checkpoint_kind IS DISTINCT FROM OLD.checkpoint_kind
       OR NEW.schema_version IS DISTINCT FROM OLD.schema_version
       OR NEW.payload IS DISTINCT FROM OLD.payload
       OR NEW.payload_hash IS DISTINCT FROM OLD.payload_hash
       OR NEW.workspace_release_receipt IS DISTINCT FROM OLD.workspace_release_receipt
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
       OR OLD.superseded_at IS NOT NULL
       OR NEW.superseded_at IS NULL THEN
        RAISE EXCEPTION 'execution task checkpoints are append-only and may only be superseded once';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_task_checkpoint_update_guard
BEFORE UPDATE ON moa.execution_task_checkpoint
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_task_checkpoint_update();

CREATE TABLE moa.execution_capacity_bucket (
    capacity_bucket_uid UUID PRIMARY KEY,
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('fleet', 'tenant')),
    tenant_id UUID,
    resource_dimension TEXT NOT NULL CHECK (resource_dimension IN (
        'active_runs', 'active_tasks', 'parked_runs', 'scheduled_triggers',
        'external_jobs'
    )),
    limit_value BIGINT NOT NULL CHECK (limit_value > 0),
    reserved_quantity BIGINT NOT NULL DEFAULT 0
        CHECK (reserved_quantity >= 0),
    version BIGINT NOT NULL DEFAULT 1 CHECK (version >= 1),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT execution_capacity_bucket_scope_check CHECK (
        (scope_kind = 'fleet' AND tenant_id IS NULL)
        OR (scope_kind = 'tenant' AND tenant_id IS NOT NULL)
    ),
    CONSTRAINT execution_capacity_bucket_tenant_resource_key
        UNIQUE (scope_kind, tenant_id, resource_dimension)
);

CREATE UNIQUE INDEX execution_capacity_bucket_fleet_resource_uidx
    ON moa.execution_capacity_bucket (resource_dimension)
    WHERE scope_kind = 'fleet';

CREATE INDEX execution_capacity_bucket_lock_order_idx
    ON moa.execution_capacity_bucket (
        resource_dimension, scope_kind, tenant_id, capacity_bucket_uid
    );

CREATE FUNCTION moa.enforce_execution_capacity_bucket_owner_immutable()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.capacity_bucket_uid IS DISTINCT FROM OLD.capacity_bucket_uid
       OR NEW.scope_kind IS DISTINCT FROM OLD.scope_kind
       OR NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
       OR NEW.resource_dimension IS DISTINCT FROM OLD.resource_dimension THEN
        RAISE EXCEPTION 'execution capacity bucket owner coordinates are immutable';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_capacity_bucket_owner_immutable
BEFORE UPDATE OF capacity_bucket_uid, scope_kind, tenant_id, resource_dimension
ON moa.execution_capacity_bucket
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_capacity_bucket_owner_immutable();

CREATE TABLE moa.execution_tenant_dispatch_state (
    tenant_id UUID PRIMARY KEY,
    weight NUMERIC(20, 6) NOT NULL DEFAULT 1 CHECK (weight > 0),
    virtual_finish NUMERIC(30, 6) NOT NULL DEFAULT 0 CHECK (virtual_finish >= 0),
    last_dispatched_at TIMESTAMPTZ,
    version BIGINT NOT NULL DEFAULT 1 CHECK (version >= 1),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX execution_tenant_dispatch_fairness_idx
    ON moa.execution_tenant_dispatch_state (
        virtual_finish, last_dispatched_at, tenant_id
    );

CREATE TABLE moa.execution_capacity_reservation (
    reservation_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    run_uid UUID,
    task_id UUID,
    compensation_id UUID,
    trigger_uid UUID,
    external_job_uid UUID,
    controller_generation BIGINT CHECK (controller_generation >= 1),
    attempt_generation BIGINT CHECK (attempt_generation >= 1),
    compensation_generation BIGINT CHECK (compensation_generation >= 1),
    compensation_attempt_generation BIGINT
        CHECK (compensation_attempt_generation >= 1),
    resource_dimension TEXT NOT NULL CHECK (resource_dimension IN (
        'active_runs', 'active_tasks', 'parked_runs', 'scheduled_triggers',
        'external_jobs'
    )),
    quantity BIGINT NOT NULL CHECK (quantity > 0),
    state TEXT NOT NULL DEFAULT 'reserved'
        CHECK (state IN ('reserved', 'released', 'reconciling')),
    expires_at TIMESTAMPTZ,
    released_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT execution_capacity_reservation_id_tenant_key
        UNIQUE (reservation_uid, tenant_id),
    CONSTRAINT execution_capacity_reservation_run_tenant_fk
        FOREIGN KEY (run_uid, tenant_id)
        REFERENCES moa.execution_run (run_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_capacity_reservation_task_tenant_fk
        FOREIGN KEY (task_id, run_uid, tenant_id)
        REFERENCES moa.execution_task (task_id, run_uid, tenant_id) ON DELETE CASCADE,
    CONSTRAINT execution_capacity_reservation_compensation_tenant_fk
        FOREIGN KEY (compensation_id, run_uid, tenant_id)
        REFERENCES moa.execution_compensation (
            compensation_id, run_uid, tenant_id
        ) ON DELETE CASCADE,
    CONSTRAINT execution_capacity_reservation_trigger_tenant_fk
        FOREIGN KEY (trigger_uid, tenant_id)
        REFERENCES moa.execution_trigger (trigger_uid, tenant_id)
        ON DELETE CASCADE,
    CONSTRAINT execution_capacity_reservation_external_job_tenant_fk
        FOREIGN KEY (external_job_uid, tenant_id)
        REFERENCES moa.execution_external_job (external_job_uid, tenant_id)
        ON DELETE CASCADE,
    CONSTRAINT execution_capacity_reservation_owner_shape_check CHECK (
        (resource_dimension = 'active_tasks'
         AND run_uid IS NOT NULL AND controller_generation IS NOT NULL
         AND (
             (
                 task_id IS NOT NULL AND attempt_generation IS NOT NULL
                 AND compensation_id IS NULL
                 AND compensation_generation IS NULL
                 AND compensation_attempt_generation IS NULL
                 AND trigger_uid IS NULL AND external_job_uid IS NULL
             )
             OR (
                 task_id IS NULL AND attempt_generation IS NULL
                 AND compensation_id IS NOT NULL
                 AND compensation_generation IS NOT NULL
                 AND compensation_attempt_generation IS NOT NULL
                 AND trigger_uid IS NULL AND external_job_uid IS NULL
             )
         ))
        OR
        (resource_dimension IN ('active_runs', 'parked_runs')
         AND run_uid IS NOT NULL AND controller_generation IS NOT NULL
         AND task_id IS NULL AND attempt_generation IS NULL
         AND compensation_id IS NULL
         AND compensation_generation IS NULL
         AND compensation_attempt_generation IS NULL
         AND trigger_uid IS NULL AND external_job_uid IS NULL)
        OR
        (resource_dimension = 'scheduled_triggers'
         AND trigger_uid IS NOT NULL AND external_job_uid IS NULL
         AND (run_uid IS NULL) = (controller_generation IS NULL)
         AND task_id IS NULL AND attempt_generation IS NULL
         AND compensation_id IS NULL
         AND compensation_generation IS NULL
         AND compensation_attempt_generation IS NULL)
        OR
        (resource_dimension = 'external_jobs'
         AND external_job_uid IS NOT NULL AND trigger_uid IS NULL
         AND run_uid IS NOT NULL AND controller_generation IS NOT NULL
         AND task_id IS NULL AND attempt_generation IS NULL
         AND compensation_id IS NULL
         AND compensation_generation IS NULL
         AND compensation_attempt_generation IS NULL)
    ),
    CONSTRAINT execution_capacity_reservation_release_pair_check CHECK (
        (state = 'released') = (released_at IS NOT NULL)
    )
);

CREATE UNIQUE INDEX execution_capacity_reservation_active_run_owner_uidx
    ON moa.execution_capacity_reservation (
        tenant_id, run_uid, resource_dimension
    )
    WHERE resource_dimension = 'active_runs';

CREATE UNIQUE INDEX execution_capacity_reservation_parked_run_owner_uidx
    ON moa.execution_capacity_reservation (
        tenant_id, run_uid, resource_dimension
    )
    WHERE resource_dimension = 'parked_runs'
      AND state IN ('reserved', 'reconciling');

CREATE UNIQUE INDEX execution_capacity_reservation_task_owner_uidx
    ON moa.execution_capacity_reservation (
        tenant_id, run_uid, task_id, resource_dimension,
        controller_generation, attempt_generation
    )
    WHERE task_id IS NOT NULL;

CREATE UNIQUE INDEX execution_capacity_reservation_compensation_owner_uidx
    ON moa.execution_capacity_reservation (
        tenant_id, run_uid, compensation_id, resource_dimension,
        controller_generation, compensation_generation,
        compensation_attempt_generation
    )
    WHERE compensation_id IS NOT NULL;

CREATE UNIQUE INDEX execution_capacity_reservation_trigger_owner_uidx
    ON moa.execution_capacity_reservation (
        tenant_id, trigger_uid, resource_dimension
    )
    WHERE resource_dimension = 'scheduled_triggers';

CREATE UNIQUE INDEX execution_capacity_reservation_external_job_owner_uidx
    ON moa.execution_capacity_reservation (
        tenant_id, external_job_uid, resource_dimension
    )
    WHERE resource_dimension = 'external_jobs';

CREATE INDEX execution_capacity_reservation_active_idx
    ON moa.execution_capacity_reservation (
        tenant_id, resource_dimension, expires_at, reservation_uid
    )
    WHERE state IN ('reserved', 'reconciling');

-- Provider calls are forbidden until the exact external-job capacity receipt
-- exists, and every nonterminal bound job retains that receipt. Deferral permits
-- intent insertion followed by reservation in one transaction and exact
-- terminalization followed by release in another.
CREATE OR REPLACE FUNCTION moa.enforce_execution_external_job_intent_capacity()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM moa.execution_external_job AS job
        WHERE job.external_job_uid = NEW.external_job_uid
          AND job.tenant_id = NEW.tenant_id
          AND job.state IN (
              'unbound', 'starting', 'running', 'waiting_reconcile',
              'cancel_requested'
          )
    ) AND NOT EXISTS (
        SELECT 1
        FROM moa.execution_capacity_reservation AS reservation
        WHERE reservation.tenant_id = NEW.tenant_id
          AND reservation.external_job_uid = NEW.external_job_uid
          AND reservation.resource_dimension = 'external_jobs'
          AND reservation.state IN ('reserved', 'reconciling')
    ) THEN
        RAISE EXCEPTION 'nonterminal execution external job requires active capacity';
    END IF;
    RETURN NEW;
END;
$$;

CREATE CONSTRAINT TRIGGER execution_external_job_intent_capacity_guard
AFTER INSERT OR UPDATE OF state ON moa.execution_external_job
DEFERRABLE INITIALLY DEFERRED
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_external_job_intent_capacity();

-- Preserve the existing immutable-field and evidence guards while extending
-- only their finite transition tables for the hard-break state model.
DO $execution_run_long_horizon_transitions$
DECLARE
    definition TEXT;
    old_block TEXT := $old$
    transition_allowed := CASE OLD.status
        WHEN 'awaiting_confirmation' THEN NEW.status IN ('queued', 'cancelled')
        WHEN 'queued' THEN NEW.status IN (
            'running', 'compensating', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        WHEN 'running' THEN NEW.status IN (
            'waiting_input', 'waiting_review', 'waiting_replan', 'compensating',
            'completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        WHEN 'waiting_input' THEN NEW.status IN (
            'running', 'compensating', 'partial', 'blocked', 'unsupported',
            'failed', 'cancelled'
        )
        WHEN 'waiting_review' THEN NEW.status IN (
            'running', 'compensating', 'partial', 'blocked', 'unsupported',
            'failed', 'cancelled'
        )
        WHEN 'waiting_replan' THEN NEW.status IN (
            'running', 'compensating', 'partial', 'blocked', 'unsupported',
            'failed', 'cancelled'
        )
        WHEN 'compensating' THEN NEW.status IN (
            'completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        ELSE FALSE
    END;
$old$;
    new_block TEXT := $new$
    transition_allowed := CASE OLD.status
        WHEN 'awaiting_confirmation' THEN NEW.status IN ('queued', 'cancelled')
        WHEN 'queued' THEN NEW.status IN (
            'running', 'waiting_review', 'waiting_signal', 'waiting_timer',
            'pause_requested', 'compensating', 'blocked', 'unsupported',
            'failed', 'cancelled'
        )
        WHEN 'running' THEN NEW.status IN (
            'waiting_input', 'waiting_review', 'waiting_signal', 'waiting_timer',
            'waiting_external', 'waiting_replan', 'pause_requested', 'compensating',
            'completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        WHEN 'waiting_input' THEN NEW.status IN (
            'running', 'waiting_review', 'waiting_signal', 'waiting_timer',
            'waiting_external', 'waiting_replan', 'pause_requested', 'compensating',
            'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        WHEN 'waiting_review' THEN NEW.status IN (
            'running', 'waiting_input', 'waiting_signal', 'waiting_timer',
            'waiting_external', 'waiting_replan', 'pause_requested', 'compensating',
            'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        WHEN 'waiting_signal' THEN NEW.status IN (
            'running', 'waiting_input', 'waiting_review', 'waiting_timer',
            'waiting_external', 'waiting_replan', 'pause_requested', 'compensating',
            'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        WHEN 'waiting_timer' THEN NEW.status IN (
            'running', 'waiting_input', 'waiting_review', 'waiting_signal',
            'waiting_external', 'waiting_replan', 'pause_requested', 'compensating',
            'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        WHEN 'waiting_external' THEN NEW.status IN (
            'running', 'waiting_input', 'waiting_review', 'waiting_signal',
            'waiting_timer', 'waiting_replan', 'pause_requested', 'compensating',
            'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        WHEN 'waiting_replan' THEN NEW.status IN (
            'running', 'waiting_input', 'waiting_review', 'waiting_signal',
            'waiting_timer', 'waiting_external', 'pause_requested', 'compensating',
            'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        WHEN 'pause_requested' THEN NEW.status IN (
            'pausing', 'paused', 'running', 'cancelled'
        )
        WHEN 'pausing' THEN NEW.status IN ('paused', 'failed', 'cancelled')
        WHEN 'paused' THEN NEW.status IN ('queued', 'cancelled')
        WHEN 'compensating' THEN NEW.status IN (
            'completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        ELSE FALSE
    END;
$new$;
BEGIN
    SELECT pg_get_functiondef('moa.enforce_execution_run_update()'::REGPROCEDURE)
    INTO definition;
    IF position(old_block IN definition) = 0 THEN
        RAISE EXCEPTION 'execution run transition table drifted before V59'
            USING ERRCODE = '55000';
    END IF;
    EXECUTE replace(definition, old_block, new_block);
END
$execution_run_long_horizon_transitions$;

CREATE OR REPLACE FUNCTION moa.enforce_execution_task_update()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    transition_allowed BOOLEAN;
BEGIN
    IF NEW.task_id IS DISTINCT FROM OLD.task_id
       OR NEW.run_uid IS DISTINCT FROM OLD.run_uid
       OR NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
       OR NEW.contact_id IS DISTINCT FROM OLD.contact_id
       OR NEW.node_id IS DISTINCT FROM OLD.node_id
       OR NEW.item_key IS DISTINCT FROM OLD.item_key
       OR NEW.requirement_ids IS DISTINCT FROM OLD.requirement_ids
       OR NEW.plan_revision IS DISTINCT FROM OLD.plan_revision
       OR NEW.input IS DISTINCT FROM OLD.input
       OR NEW.task_kind IS DISTINCT FROM OLD.task_kind
       OR NEW.compensation_contract IS DISTINCT FROM OLD.compensation_contract
       OR NEW.retry_policy IS DISTINCT FROM OLD.retry_policy
       OR NEW.estimate_cost_microusd IS DISTINCT FROM OLD.estimate_cost_microusd
       OR NEW.estimate_tokens IS DISTINCT FROM OLD.estimate_tokens
       OR NEW.estimate_tasks IS DISTINCT FROM OLD.estimate_tasks
       OR NEW.estimate_tool_calls IS DISTINCT FROM OLD.estimate_tool_calls
       OR NEW.estimate_retrieved_bytes IS DISTINCT FROM OLD.estimate_retrieved_bytes
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION 'execution task immutable fields cannot change';
    END IF;

    IF NOT moa.execution_jsonb_array_has_prefix(
               NEW.resume_input_history, OLD.resume_input_history
           )
       OR NOT moa.execution_jsonb_array_has_prefix(
               NEW.generation_history, OLD.generation_history
           )
       OR NOT moa.execution_jsonb_array_has_prefix(NEW.outcome_audit, OLD.outcome_audit) THEN
        RAISE EXCEPTION 'execution task histories are append-only';
    END IF;

    IF OLD.status = 'waiting_input' AND NEW.status = 'ready' THEN
        IF NEW.attempt <> OLD.attempt
           OR NEW.generation <> OLD.generation + 1
           OR NEW.attempt_generation <> OLD.attempt_generation + 1 THEN
            RAISE EXCEPTION 'execution input resume must advance generation fences exactly once';
        END IF;
    ELSIF OLD.status = 'running'
          AND NEW.status = 'ready'
          AND (
              NEW.attempt IS DISTINCT FROM OLD.attempt
              OR NEW.generation IS DISTINCT FROM OLD.generation
          ) THEN
        IF NEW.attempt <> OLD.attempt + 1
           OR NEW.generation <> OLD.generation + 1
           OR NEW.attempt_generation <> OLD.attempt_generation + 1 THEN
            RAISE EXCEPTION 'execution retry must advance attempt and generation fences exactly once';
        END IF;
    ELSIF NEW.attempt IS DISTINCT FROM OLD.attempt
          OR NEW.generation IS DISTINCT FROM OLD.generation THEN
        RAISE EXCEPTION 'execution task counters changed outside retry or input resume';
    END IF;

    IF NEW.attempt_generation IS DISTINCT FROM OLD.attempt_generation
       AND (
           NEW.attempt_generation <> OLD.attempt_generation + 1
           OR OLD.status = 'ready'
           OR NEW.status <> 'ready'
           OR NEW.attempt_state <> 'idle'
       ) THEN
        RAISE EXCEPTION 'execution task attempt generation must advance once into ready idle';
    END IF;
    IF OLD.attempt_state = 'cancelling'
       AND NEW.attempt_state NOT IN (
           'cancelling', 'idle', 'waiting', 'terminal', 'unknown_outcome'
       ) THEN
        RAISE EXCEPTION 'execution task cancelling state cannot become dispatchable';
    END IF;
    IF NEW.dispatch_sequence < OLD.dispatch_sequence THEN
        RAISE EXCEPTION 'execution task dispatch sequence must be monotonic';
    END IF;
    IF OLD.last_progress_at IS NOT NULL
       AND NEW.last_progress_at < OLD.last_progress_at THEN
        RAISE EXCEPTION 'execution task last progress timestamp must be monotonic';
    END IF;

    IF NEW.status IS NOT DISTINCT FROM OLD.status THEN
        RETURN NEW;
    END IF;

    transition_allowed := CASE OLD.status
        -- 'failed' without an attempt: a relative wait can resolve at wait entry
        -- to a due time at or past the run deadline, which fails the task on its
        -- own node rather than entering a wait that could never settle.
        WHEN 'pending' THEN NEW.status IN (
            'ready', 'reserved', 'waiting_review', 'waiting_signal',
            'waiting_timer', 'failed', 'skipped', 'cancelled'
        )
        WHEN 'ready' THEN NEW.status IN ('dispatching', 'reserved', 'cancelled')
        WHEN 'reserved' THEN NEW.status IN ('dispatching', 'running', 'cancelled')
        WHEN 'dispatching' THEN NEW.status IN ('running', 'ready', 'failed', 'cancelled')
        WHEN 'running' THEN NEW.status IN (
            'ready', 'waiting_input', 'waiting_review', 'waiting_signal',
            'waiting_timer', 'waiting_external', 'waiting_replan', 'completed',
            'failed', 'cancelled', 'unknown_outcome'
        )
        WHEN 'waiting_input' THEN NEW.status IN ('ready', 'cancelled')
        WHEN 'waiting_review' THEN NEW.status IN ('running', 'ready', 'cancelled')
        WHEN 'waiting_signal' THEN NEW.status IN ('running', 'ready', 'cancelled')
        WHEN 'waiting_timer' THEN NEW.status IN ('running', 'ready', 'cancelled')
        WHEN 'waiting_external' THEN NEW.status IN (
            'ready', 'completed', 'failed', 'cancelled', 'unknown_outcome'
        )
        WHEN 'waiting_replan' THEN NEW.status IN ('ready', 'cancelled')
        ELSE FALSE
    END;
    IF NOT transition_allowed THEN
        RAISE EXCEPTION 'invalid execution task status transition: % -> %',
            OLD.status, NEW.status;
    END IF;
    RETURN NEW;
END;
$$;

CREATE OR REPLACE FUNCTION moa.enforce_execution_run_long_horizon_update()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.admitted_identity IS DISTINCT FROM OLD.admitted_identity THEN
        RAISE EXCEPTION 'execution run admitted identity is immutable';
    END IF;
    IF OLD.terminal_archive_uid IS NOT NULL
       AND (
           NEW.terminal_archive_uid IS DISTINCT FROM OLD.terminal_archive_uid
           OR NEW.terminal_archive_hash IS DISTINCT FROM OLD.terminal_archive_hash
           OR NEW.terminal_details_archived_at
               IS DISTINCT FROM OLD.terminal_details_archived_at
       ) THEN
        RAISE EXCEPTION 'execution run terminal archive binding is immutable';
    END IF;
    IF NEW.terminal_archive_uid IS NOT NULL
       AND NOT EXISTS (
           SELECT 1
           FROM moa.execution_terminal_archive AS archive
           WHERE archive.archive_uid = NEW.terminal_archive_uid
             AND archive.tenant_id = NEW.tenant_id
             AND archive.run_uid = NEW.run_uid
             AND archive.root_digest = NEW.terminal_archive_hash
             AND archive.finalized_at IS NOT NULL
       ) THEN
        RAISE EXCEPTION 'execution run terminal archive binding is not exact';
    END IF;
    IF NEW.schedule_uid IS DISTINCT FROM OLD.schedule_uid
       OR NEW.schedule_incarnation IS DISTINCT FROM OLD.schedule_incarnation
       OR NEW.schedule_occurrence_sequence IS DISTINCT FROM OLD.schedule_occurrence_sequence THEN
        RAISE EXCEPTION 'execution run schedule occurrence identity is immutable';
    END IF;
    IF NEW.active_plan IS DISTINCT FROM OLD.active_plan
       AND NOT moa.execution_plan_snapshot_is_current(NEW.active_plan) THEN
        RAISE EXCEPTION 'execution run amendment must use the current plan contract';
    END IF;
    IF NEW.controller_generation < OLD.controller_generation THEN
        RAISE EXCEPTION 'execution run controller generation must be monotonic';
    END IF;
    IF NEW.ready_task_count < 0 OR NEW.active_task_count < 0 THEN
        RAISE EXCEPTION 'execution run task counters cannot be negative';
    END IF;
    -- Draining the last active attempt completes a pause. Attempt settlement only
    -- adjusts counters, so the promotion has to happen here. It must never rewrite
    -- a status the writer chose: a terminal write that also zeroes the counter
    -- (deadline, budget, terminal fence) would otherwise be swallowed into
    -- 'paused', and the run could then never leave it, because 'paused' admits
    -- only 'queued' and 'cancelled'.
    IF OLD.status = 'pausing'
       AND NEW.status = OLD.status
       AND NEW.active_task_count = 0 THEN
        NEW.status := 'paused';
        NEW.activation_state := 'paused';
        NEW.paused_at := COALESCE(NEW.paused_at, now());
    END IF;
    IF OLD.last_progress_at IS NOT NULL
       AND NEW.last_progress_at < OLD.last_progress_at THEN
        RAISE EXCEPTION 'execution run last progress timestamp must be monotonic';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_run_long_horizon_update_guard
BEFORE UPDATE ON moa.execution_run
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_run_long_horizon_update();

CREATE OR REPLACE FUNCTION moa.enforce_execution_compensation_long_horizon_update()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.attempt_generation < OLD.attempt_generation THEN
        RAISE EXCEPTION 'execution compensation attempt generation must be monotonic';
    END IF;
    IF OLD.attempt_state = 'cancelling'
       AND NEW.attempt_state NOT IN (
           'cancelling', 'idle', 'waiting_review', 'terminal', 'unknown_outcome'
       ) THEN
        RAISE EXCEPTION 'execution compensation cancelling state cannot become dispatchable';
    END IF;
    IF OLD.attempt_state = 'cancelling'
       AND NEW.attempt_state = 'cancelling'
       AND NEW.release_intent IS DISTINCT FROM OLD.release_intent THEN
        RAISE EXCEPTION 'execution compensation release intent is immutable while cancelling';
    END IF;
    IF NEW.dispatch_sequence < OLD.dispatch_sequence THEN
        RAISE EXCEPTION 'execution compensation dispatch sequence must be monotonic';
    END IF;
    IF OLD.last_progress_at IS NOT NULL
       AND NEW.last_progress_at < OLD.last_progress_at THEN
        RAISE EXCEPTION 'execution compensation last progress timestamp must be monotonic';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_compensation_long_horizon_update_guard
BEFORE UPDATE ON moa.execution_compensation
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_compensation_long_horizon_update();

CREATE OR REPLACE FUNCTION moa.enforce_execution_dispatch_fairness_update()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.virtual_finish < OLD.virtual_finish THEN
        RAISE EXCEPTION 'execution tenant virtual finish must be monotonic';
    END IF;
    IF NEW.version < OLD.version THEN
        RAISE EXCEPTION 'execution tenant dispatch version must be monotonic';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_tenant_dispatch_fairness_update_guard
BEFORE UPDATE ON moa.execution_tenant_dispatch_state
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_dispatch_fairness_update();

CREATE OR REPLACE FUNCTION moa.enforce_execution_schedule_update()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.template_revision_uid IS DISTINCT FROM OLD.template_revision_uid
       OR NEW.template_snapshot IS DISTINCT FROM OLD.template_snapshot
       OR NEW.template_hash IS DISTINCT FROM OLD.template_hash
       OR NEW.run_as_identity IS DISTINCT FROM OLD.run_as_identity
       OR NEW.creation_origin IS DISTINCT FROM OLD.creation_origin THEN
        RAISE EXCEPTION 'execution schedule template, run-as identity, and origin are immutable';
    END IF;
    IF NEW.schedule_incarnation < OLD.schedule_incarnation THEN
        RAISE EXCEPTION 'execution schedule incarnation must be monotonic';
    END IF;
    IF NEW.last_occurrence_sequence < 0 THEN
        RAISE EXCEPTION 'execution schedule occurrence sequence cannot be negative';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_schedule_update_guard
BEFORE UPDATE ON moa.execution_schedule
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_schedule_update();

CREATE OR REPLACE FUNCTION moa.reject_execution_terminal_archive_mutation()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'UPDATE'
       AND OLD.finalized_at IS NOT NULL
       AND OLD.details_deleted_at IS NULL
       AND NEW.details_deleted_at IS NOT NULL
       AND NEW.archive_uid = OLD.archive_uid
       AND NEW.tenant_id = OLD.tenant_id
       AND NEW.run_uid = OLD.run_uid
       AND NEW.contact_id IS NOT DISTINCT FROM OLD.contact_id
       AND NEW.format_version = OLD.format_version
       AND NEW.terminal_status = OLD.terminal_status
       AND NEW.terminal_completed_at = OLD.terminal_completed_at
       AND NEW.goal_hash = OLD.goal_hash
       AND NEW.initial_plan_hash = OLD.initial_plan_hash
       AND NEW.active_plan_hash = OLD.active_plan_hash
       AND NEW.source_record_count = OLD.source_record_count
       AND NEW.source_logical_bytes = OLD.source_logical_bytes
       AND NEW.segment_count = OLD.segment_count
       AND NEW.source_cursor = OLD.source_cursor
       AND NEW.rolling_chain_digest = OLD.rolling_chain_digest
       AND NEW.root_digest = OLD.root_digest
       AND NEW.archive_generation = OLD.archive_generation
       AND NEW.created_at = OLD.created_at
       AND NEW.finalized_at = OLD.finalized_at
       AND NOT EXISTS (
           SELECT 1 FROM moa.legal_hold AS hold
           WHERE hold.tenant_id = OLD.tenant_id
             AND hold.released_at IS NULL
             AND (hold.subject_id IS NULL OR hold.subject_id = OLD.contact_id)
       ) THEN
        RETURN NEW;
    END IF;
    IF TG_OP = 'UPDATE'
       AND OLD.finalized_at IS NULL
       AND NEW.finalized_at IS NULL
       AND NEW.root_digest IS NULL
       AND NEW.details_deleted_at IS NULL
       AND NEW.archive_uid = OLD.archive_uid
       AND NEW.tenant_id = OLD.tenant_id
       AND NEW.run_uid = OLD.run_uid
       AND NEW.contact_id IS NOT DISTINCT FROM OLD.contact_id
       AND NEW.format_version = OLD.format_version
       AND NEW.terminal_status = OLD.terminal_status
       AND NEW.terminal_completed_at = OLD.terminal_completed_at
       AND NEW.goal_hash = OLD.goal_hash
       AND NEW.initial_plan_hash = OLD.initial_plan_hash
       AND NEW.active_plan_hash = OLD.active_plan_hash
       AND NEW.archive_generation = OLD.archive_generation
       AND NEW.created_at = OLD.created_at
       AND (
           (
               NEW.segment_count = OLD.segment_count + 1
               AND NEW.source_record_count > OLD.source_record_count
               AND NEW.source_logical_bytes > OLD.source_logical_bytes
               AND NEW.rolling_chain_digest IS NOT NULL
               AND NEW.rolling_chain_digest IS DISTINCT FROM OLD.rolling_chain_digest
           )
           OR
           (
               NEW.segment_count = OLD.segment_count
               AND NEW.source_record_count = OLD.source_record_count
               AND NEW.source_logical_bytes = OLD.source_logical_bytes
               AND NEW.rolling_chain_digest IS NOT DISTINCT FROM OLD.rolling_chain_digest
               AND NEW.source_cursor IS DISTINCT FROM OLD.source_cursor
           )
       )
       AND NOT EXISTS (
           SELECT 1 FROM moa.legal_hold AS hold
           WHERE hold.tenant_id = OLD.tenant_id
             AND hold.released_at IS NULL
             AND (hold.subject_id IS NULL OR hold.subject_id = OLD.contact_id)
       ) THEN
        RETURN NEW;
    END IF;
    IF TG_OP = 'UPDATE'
       AND OLD.finalized_at IS NULL
       AND NEW.finalized_at IS NOT NULL
       AND NEW.root_digest IS NOT NULL
       AND NEW.details_deleted_at IS NULL
       AND NEW.archive_uid = OLD.archive_uid
       AND NEW.tenant_id = OLD.tenant_id
       AND NEW.run_uid = OLD.run_uid
       AND NEW.contact_id IS NOT DISTINCT FROM OLD.contact_id
       AND NEW.format_version = OLD.format_version
       AND NEW.terminal_status = OLD.terminal_status
       AND NEW.terminal_completed_at = OLD.terminal_completed_at
       AND NEW.goal_hash = OLD.goal_hash
       AND NEW.initial_plan_hash = OLD.initial_plan_hash
       AND NEW.active_plan_hash = OLD.active_plan_hash
       AND NEW.source_record_count = OLD.source_record_count
       AND NEW.source_logical_bytes = OLD.source_logical_bytes
       AND NEW.segment_count = OLD.segment_count
       AND NEW.source_cursor = OLD.source_cursor
       AND NEW.rolling_chain_digest = OLD.rolling_chain_digest
       AND NEW.root_digest = OLD.rolling_chain_digest
       AND NEW.archive_generation = OLD.archive_generation
       AND NEW.created_at = OLD.created_at
       AND NOT EXISTS (
           SELECT 1 FROM moa.legal_hold AS hold
           WHERE hold.tenant_id = OLD.tenant_id
             AND hold.released_at IS NULL
             AND (hold.subject_id IS NULL OR hold.subject_id = OLD.contact_id)
       ) THEN
        RETURN NEW;
    END IF;
    IF TG_OP = 'DELETE' AND EXISTS (
        SELECT 1
        FROM moa.destruction_operation_fence
        WHERE tenant_id = OLD.tenant_id
          AND subject_id IS NULL
    ) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'execution terminal archive rows are immutable';
END;
$$;

CREATE OR REPLACE FUNCTION moa.enforce_execution_terminal_archive_insert()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM moa.legal_hold AS hold
        WHERE hold.tenant_id = NEW.tenant_id
          AND hold.released_at IS NULL
          AND (hold.subject_id IS NULL OR hold.subject_id = NEW.contact_id)
    ) THEN
        RAISE EXCEPTION 'execution terminal archive blocked by active legal hold';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_terminal_archive_insert_guard
BEFORE INSERT ON moa.execution_terminal_archive
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_terminal_archive_insert();

CREATE TRIGGER execution_terminal_archive_immutable_guard
BEFORE UPDATE OR DELETE ON moa.execution_terminal_archive
FOR EACH ROW EXECUTE FUNCTION moa.reject_execution_terminal_archive_mutation();

CREATE OR REPLACE FUNCTION moa.enforce_execution_terminal_archive_segment_mutation()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'INSERT' AND EXISTS (
        SELECT 1
        FROM moa.execution_terminal_archive AS archive
        WHERE archive.archive_uid = NEW.archive_uid
          AND archive.tenant_id = NEW.tenant_id
          AND archive.run_uid = NEW.run_uid
          AND archive.finalized_at IS NOT NULL
    ) THEN
        RAISE EXCEPTION 'cannot append to a finalized execution terminal archive';
    END IF;
    IF TG_OP = 'INSERT' THEN
        RETURN NEW;
    END IF;
    IF TG_OP = 'DELETE' AND EXISTS (
        SELECT 1
        FROM moa.destruction_operation_fence
        WHERE tenant_id = OLD.tenant_id
          AND subject_id IS NULL
    ) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'execution terminal archive segments are immutable';
END;
$$;

CREATE TRIGGER execution_terminal_archive_segment_mutation_guard
BEFORE INSERT OR UPDATE OR DELETE ON moa.execution_terminal_archive_segment
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_terminal_archive_segment_mutation();

CREATE OR REPLACE FUNCTION moa.reject_execution_archived_detail_write()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.run_uid IS NOT NULL AND EXISTS (
        SELECT 1
        FROM moa.execution_terminal_archive AS archive
        WHERE archive.run_uid = NEW.run_uid
          AND archive.tenant_id = NEW.tenant_id
    ) THEN
        RAISE EXCEPTION 'cannot write execution detail after terminal archival has started';
    END IF;
    RETURN NEW;
END;
$$;

DO $execution_archived_detail_write_fences$
DECLARE
    table_name TEXT;
BEGIN
    FOREACH table_name IN ARRAY ARRAY[
        'execution_planner_call_audit',
        'execution_compile_audit',
        'execution_node_materialization',
        'execution_action_review_outbox',
        'execution_compensation',
        'execution_task',
        'execution_node_state',
        'execution_completion_scan',
        'execution_amendment_receipt',
        'execution_amendment_planning_reservation',
        'execution_amendment_planning_settlement',
        'execution_replan_stop_intent',
        'execution_external_job',
        'execution_trigger',
        'execution_dispatch_outbox',
        'execution_capacity_reservation',
        'execution_task_checkpoint'
    ]
    LOOP
        EXECUTE format(
            'CREATE TRIGGER execution_archived_detail_write_fence '
            'BEFORE INSERT OR UPDATE ON moa.%I '
            'FOR EACH ROW EXECUTE FUNCTION moa.reject_execution_archived_detail_write()',
            table_name
        );
    END LOOP;
END
$execution_archived_detail_write_fences$;

-- Once an exact compact archive is bound to the durable run, immutable bulky
-- analytics rows may be page-deleted unless a tenant/contact legal hold is
-- active. Tenant destruction retains its existing separate authority.
CREATE OR REPLACE FUNCTION moa.reject_execution_immutable_payload()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'DELETE' AND EXISTS (
        SELECT 1
        FROM moa.destruction_operation_fence
        WHERE tenant_id = OLD.tenant_id
          AND subject_id IS NULL
    ) THEN
        RETURN OLD;
    END IF;
    IF TG_OP = 'DELETE'
       AND OLD.run_uid IS NOT NULL
       AND EXISTS (
           SELECT 1
           FROM moa.execution_run AS run
           JOIN moa.execution_terminal_archive AS archive
             ON archive.archive_uid = run.terminal_archive_uid
            AND archive.tenant_id = run.tenant_id
            AND archive.run_uid = run.run_uid
            AND archive.root_digest = run.terminal_archive_hash
            AND archive.finalized_at IS NOT NULL
           WHERE run.tenant_id = OLD.tenant_id
             AND run.run_uid = OLD.run_uid
             AND run.status IN (
                 'completed', 'partial', 'blocked', 'unsupported',
                 'failed', 'cancelled'
             )
             AND NOT EXISTS (
                 SELECT 1
                 FROM moa.legal_hold AS hold
                 WHERE hold.tenant_id = run.tenant_id
                   AND hold.released_at IS NULL
                   AND (hold.subject_id IS NULL OR hold.subject_id = run.contact_id)
             )
       ) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'execution analytics rows are immutable';
END;
$$;

CREATE OR REPLACE FUNCTION moa.reject_execution_replan_stop_intent_mutation()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'UPDATE'
       AND NEW.tenant_id = OLD.tenant_id
       AND NEW.run_uid = OLD.run_uid
       AND NEW.controller_generation = OLD.controller_generation
       AND NEW.wake_epoch > OLD.wake_epoch
       AND NEW.origin_task_id = OLD.origin_task_id
       AND NEW.task_generation = OLD.task_generation
       AND NEW.base_plan_revision = OLD.base_plan_revision
       AND NEW.stop_reason = OLD.stop_reason
       AND NEW.detail = OLD.detail
       AND NEW.amendment_hash = OLD.amendment_hash
       AND NEW.created_at = OLD.created_at
       AND NEW.updated_at > OLD.updated_at THEN
        RETURN NEW;
    END IF;
    IF TG_OP = 'DELETE' AND EXISTS (
        SELECT 1
        FROM moa.destruction_operation_fence
        WHERE tenant_id = OLD.tenant_id
          AND subject_id IS NULL
    ) THEN
        RETURN OLD;
    END IF;
    IF TG_OP = 'DELETE' AND EXISTS (
        SELECT 1
        FROM moa.execution_amendment_receipt AS receipt
        WHERE receipt.tenant_id = OLD.tenant_id
          AND receipt.run_uid = OLD.run_uid
          AND receipt.base_plan_revision = OLD.base_plan_revision
          AND receipt.receipt_kind = 'replan_stop'
          AND receipt.superseded_task_id = OLD.origin_task_id
          AND receipt.task_generation = OLD.task_generation
          AND receipt.amendment_hash = OLD.amendment_hash
    ) THEN
        RETURN OLD;
    END IF;
    IF TG_OP = 'DELETE' AND EXISTS (
        SELECT 1
        FROM moa.execution_run AS run
        JOIN moa.execution_terminal_archive AS archive
          ON archive.archive_uid = run.terminal_archive_uid
         AND archive.tenant_id = run.tenant_id
         AND archive.run_uid = run.run_uid
         AND archive.root_digest = run.terminal_archive_hash
         AND archive.finalized_at IS NOT NULL
        WHERE run.tenant_id = OLD.tenant_id
          AND run.run_uid = OLD.run_uid
          AND run.status IN (
              'completed', 'partial', 'blocked', 'unsupported',
              'failed', 'cancelled'
          )
          AND NOT EXISTS (
              SELECT 1
              FROM moa.legal_hold AS hold
              WHERE hold.tenant_id = run.tenant_id
                AND hold.released_at IS NULL
                AND (hold.subject_id IS NULL OR hold.subject_id = run.contact_id)
          )
    ) THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'execution replan-stop intent is immutable until exact fencing';
END;
$$;

-- All new relations are tenant-owned. Composite foreign keys make it
-- impossible to attach an orchestration row to a differently scoped parent.
CREATE TRIGGER execution_node_state_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.execution_node_state
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();
CREATE TRIGGER execution_completion_scan_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.execution_completion_scan
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();
CREATE TRIGGER execution_amendment_receipt_immutable_guard
BEFORE UPDATE OR DELETE ON moa.execution_amendment_receipt
FOR EACH ROW EXECUTE FUNCTION moa.reject_execution_immutable_payload();
CREATE TRIGGER execution_amendment_planning_reservation_immutable_guard
BEFORE UPDATE OR DELETE ON moa.execution_amendment_planning_reservation
FOR EACH ROW EXECUTE FUNCTION moa.reject_execution_immutable_payload();
CREATE TRIGGER execution_amendment_planning_settlement_immutable_guard
BEFORE UPDATE OR DELETE ON moa.execution_amendment_planning_settlement
FOR EACH ROW EXECUTE FUNCTION moa.reject_execution_immutable_payload();
CREATE TRIGGER execution_replan_stop_intent_immutable_guard
BEFORE UPDATE OR DELETE ON moa.execution_replan_stop_intent
FOR EACH ROW EXECUTE FUNCTION moa.reject_execution_replan_stop_intent_mutation();
CREATE TRIGGER execution_trigger_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.execution_trigger
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();
CREATE TRIGGER execution_dispatch_outbox_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.execution_dispatch_outbox
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();
CREATE TRIGGER execution_external_job_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.execution_external_job
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();
CREATE TRIGGER execution_external_job_callback_receipt_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.execution_external_job_callback_receipt
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();
CREATE TRIGGER execution_capacity_reservation_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.execution_capacity_reservation
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();
CREATE TRIGGER execution_schedule_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.execution_schedule
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();
CREATE TRIGGER execution_capacity_bucket_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.execution_capacity_bucket
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();
CREATE TRIGGER execution_tenant_dispatch_state_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.execution_tenant_dispatch_state
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();
CREATE TRIGGER execution_task_checkpoint_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.execution_task_checkpoint
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();
CREATE TRIGGER execution_terminal_archive_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.execution_terminal_archive
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();
CREATE TRIGGER execution_terminal_archive_segment_tenant_immutable
BEFORE UPDATE OF tenant_id ON moa.execution_terminal_archive_segment
FOR EACH ROW EXECUTE FUNCTION moa.reject_tenant_id_change();

SELECT moa.apply_tenant_rls('moa.execution_node_state');
SELECT moa.apply_tenant_rls('moa.execution_completion_scan');
SELECT moa.apply_tenant_rls('moa.execution_amendment_receipt');
SELECT moa.apply_contact_rls('moa.execution_amendment_planning_reservation'::REGCLASS);
SELECT moa.apply_contact_rls('moa.execution_amendment_planning_settlement'::REGCLASS);
SELECT moa.apply_tenant_rls('moa.execution_replan_stop_intent');
SELECT moa.apply_tenant_rls('moa.execution_trigger');
SELECT moa.apply_tenant_rls('moa.execution_dispatch_outbox');
SELECT moa.apply_tenant_rls('moa.execution_external_job');
SELECT moa.apply_tenant_rls('moa.execution_external_job_callback_receipt');
SELECT moa.apply_tenant_rls('moa.execution_capacity_reservation');
SELECT moa.apply_tenant_rls('moa.execution_schedule');
SELECT moa.apply_tenant_rls('moa.execution_capacity_bucket');
DROP POLICY tenant_isolation ON moa.execution_capacity_bucket;
CREATE POLICY execution_capacity_bucket_control_plane
ON moa.execution_capacity_bucket
FOR ALL TO moa_app
USING (moa.current_control_plane())
WITH CHECK (moa.current_control_plane());
CREATE POLICY execution_capacity_bucket_tenant_read
ON moa.execution_capacity_bucket
FOR SELECT TO moa_app
USING (
    (scope_kind = 'fleet' AND tenant_id IS NULL)
    OR (scope_kind = 'tenant' AND tenant_id = moa.current_tenant_id())
);
CREATE POLICY execution_capacity_bucket_tenant_insert
ON moa.execution_capacity_bucket
FOR INSERT TO moa_app
WITH CHECK (
    (scope_kind = 'fleet' AND tenant_id IS NULL)
    OR (scope_kind = 'tenant' AND tenant_id = moa.current_tenant_id())
);
CREATE POLICY execution_capacity_bucket_tenant_update
ON moa.execution_capacity_bucket
FOR UPDATE TO moa_app
USING (
    (scope_kind = 'fleet' AND tenant_id IS NULL)
    OR (scope_kind = 'tenant' AND tenant_id = moa.current_tenant_id())
)
WITH CHECK (
    (scope_kind = 'fleet' AND tenant_id IS NULL)
    OR (scope_kind = 'tenant' AND tenant_id = moa.current_tenant_id())
);
SELECT moa.apply_tenant_rls('moa.execution_tenant_dispatch_state');
SELECT moa.apply_tenant_rls('moa.execution_task_checkpoint');
SELECT moa.apply_tenant_rls('moa.execution_terminal_archive');
SELECT moa.apply_tenant_rls('moa.execution_terminal_archive_segment');

DO $execution_long_horizon_purge_fences$
DECLARE
    table_name TEXT;
BEGIN
    FOREACH table_name IN ARRAY ARRAY[
        'execution_node_state',
        'execution_completion_scan',
        'execution_amendment_receipt',
        'execution_amendment_planning_reservation',
        'execution_amendment_planning_settlement',
        'execution_replan_stop_intent',
        'execution_trigger',
        'execution_dispatch_outbox',
        'execution_external_job',
        'execution_external_job_callback_receipt',
        'execution_capacity_reservation',
        'execution_schedule',
        'execution_capacity_bucket',
        'execution_tenant_dispatch_state',
        'execution_task_checkpoint',
        'execution_terminal_archive',
        'execution_terminal_archive_segment'
    ]
    LOOP
        EXECUTE format(
            'CREATE TRIGGER moa_tenant_purge_fence_insert '
            'AFTER INSERT ON moa.%I '
            'REFERENCING NEW TABLE AS tenant_purge_new_rows '
            'FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement(''tenant_id'')',
            table_name
        );
        EXECUTE format(
            'CREATE TRIGGER moa_tenant_purge_fence_update '
            'AFTER UPDATE ON moa.%I '
            'REFERENCING OLD TABLE AS tenant_purge_old_rows '
            'NEW TABLE AS tenant_purge_new_rows '
            'FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement(''tenant_id'')',
            table_name
        );
    END LOOP;
END
$execution_long_horizon_purge_fences$;

-- Delete execution-owned children before planner audits, tasks, and runs. Shift through a
-- remote range to preserve the catalog's unique stage ordering.
UPDATE moa.tenant_purge_catalog
SET stage_order = stage_order + 1000
WHERE stage_order >= (
    SELECT stage_order FROM moa.tenant_purge_catalog
    WHERE stage_name = 'moa.execution_planner_call_audit'
);

UPDATE moa.tenant_purge_catalog
SET stage_order = stage_order - 983
WHERE stage_order >= 1000;

INSERT INTO moa.tenant_purge_catalog (
    stage_order, stage_name, table_schema, table_name, scope_mode, action_mode
)
SELECT planner_audit.stage_order - execution_stage.stage_offset,
       execution_stage.stage_name,
       'moa',
       execution_stage.table_name,
       'tenant_id',
       'delete'
FROM moa.tenant_purge_catalog AS planner_audit
CROSS JOIN (VALUES
    (17, 'moa.execution_amendment_planning_settlement',
        'execution_amendment_planning_settlement'),
    (16, 'moa.execution_amendment_planning_reservation',
        'execution_amendment_planning_reservation'),
    (15, 'moa.execution_replan_stop_intent', 'execution_replan_stop_intent'),
    (14, 'moa.execution_amendment_receipt', 'execution_amendment_receipt'),
    (13, 'moa.execution_task_checkpoint', 'execution_task_checkpoint'),
    (12, 'moa.execution_dispatch_outbox', 'execution_dispatch_outbox'),
    (11, 'moa.execution_trigger', 'execution_trigger'),
    (10, 'moa.execution_capacity_reservation', 'execution_capacity_reservation'),
    (9, 'moa.execution_external_job_callback_receipt',
        'execution_external_job_callback_receipt'),
    (8, 'moa.execution_external_job', 'execution_external_job'),
    (7, 'moa.execution_completion_scan', 'execution_completion_scan'),
    (6, 'moa.execution_node_state', 'execution_node_state'),
    (5, 'moa.execution_schedule', 'execution_schedule'),
    (4, 'moa.execution_tenant_dispatch_state', 'execution_tenant_dispatch_state'),
    (3, 'moa.execution_capacity_bucket', 'execution_capacity_bucket'),
    (2, 'moa.execution_terminal_archive_segment',
        'execution_terminal_archive_segment'),
    (1, 'moa.execution_terminal_archive', 'execution_terminal_archive')
) AS execution_stage(stage_offset, stage_name, table_name)
WHERE planner_audit.stage_name = 'moa.execution_planner_call_audit';

COMMENT ON TABLE moa.tenant_purge_catalog IS
    'Closed 159-table tenant-offboarding residue surface. Fleet capacity-bucket rows, sandbox provider accounts, and inventory findings are global maintenance authority; the two nullable-scope simulator certification authority tables are also intentionally global and absent.';

DO $execution_long_horizon_purge_function$
DECLARE
    predecessor TEXT;
    replacement TEXT;
BEGIN
    SELECT pg_get_functiondef('moa.run_tenant_purge_batch(uuid,text)'::REGPROCEDURE)
    INTO predecessor;
    IF predecessor NOT LIKE '%catalog_count <> 142%'
       OR predecessor NOT LIKE '%exactly 142 tables%' THEN
        RAISE EXCEPTION 'unexpected V58 tenant purge function definition'
            USING ERRCODE = '55000';
    END IF;
    replacement := replace(predecessor, 'catalog_count <> 142', 'catalog_count <> 159');
    replacement := replace(replacement, 'exactly 142 tables', 'exactly 159 tables');
    EXECUTE replacement;
END
$execution_long_horizon_purge_function$;

ALTER FUNCTION moa.run_tenant_purge_batch(UUID, TEXT) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.run_tenant_purge_batch(UUID, TEXT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.run_tenant_purge_batch(UUID, TEXT)
    TO moa_app, moa_promoter, moa_workspace_maintenance;
