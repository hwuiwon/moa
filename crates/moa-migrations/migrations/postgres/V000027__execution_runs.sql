-- Durable execution-run persistence with the final normalized scope and
-- provenance contract.

CREATE OR REPLACE FUNCTION moa.execution_json_object_has_exact_keys(
    candidate JSONB,
    expected TEXT[]
) RETURNS BOOLEAN
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT jsonb_typeof(candidate) = 'object'
       AND COALESCE(
               (SELECT array_agg(key ORDER BY key COLLATE "C")
                FROM jsonb_object_keys(candidate) key),
               ARRAY[]::TEXT[]
           ) = COALESCE(
               (SELECT array_agg(value ORDER BY value COLLATE "C")
                FROM unnest(expected) value),
               ARRAY[]::TEXT[]
           )
$$;

CREATE OR REPLACE FUNCTION moa.execution_skill_ref_is_canonical(candidate TEXT)
RETURNS BOOLEAN
LANGUAGE sql
IMMUTABLE
STRICT
AS $$
    SELECT octet_length(candidate) BETWEEN 9 AND 520
       AND candidate ~ '^skill://[A-Za-z0-9][A-Za-z0-9_.~:/@#-]*$'
       AND substring(candidate FROM 9) NOT LIKE '%://%'
$$;

CREATE OR REPLACE FUNCTION moa.execution_terminal_reason_for(
    status_value TEXT,
    terminal_cause JSONB,
    source_kind_value TEXT
) RETURNS TEXT
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
    cause_kind TEXT := terminal_cause ->> 'kind';
    cause_reason TEXT := terminal_cause ->> 'reason';
    failure_class TEXT := terminal_cause ->> 'class';
    limit_stop TEXT := terminal_cause ->> 'limit_stop';
BEGIN
    IF status_value NOT IN (
        'completed','partial','blocked','unsupported','failed','cancelled'
    ) OR terminal_cause IS NULL
       OR jsonb_typeof(terminal_cause) <> 'object' THEN
        RETURN NULL;
    END IF;
    IF NOT (CASE cause_kind
        WHEN 'completion' THEN
            moa.execution_json_object_has_exact_keys(
                terminal_cause, ARRAY['kind','limit_stop']
            )
            AND (
                terminal_cause -> 'limit_stop' = 'null'::JSONB
                OR (
                    jsonb_typeof(terminal_cause -> 'limit_stop') = 'string'
                    AND limit_stop IN (
                        'deadline_exceeded','budget_exceeded'
                    )
                )
            )
        WHEN 'task_failure' THEN
            moa.execution_json_object_has_exact_keys(
                terminal_cause, ARRAY['kind','class']
            )
            AND jsonb_typeof(terminal_cause -> 'class') = 'string'
        WHEN 'limit_stop' THEN
            moa.execution_json_object_has_exact_keys(
                terminal_cause, ARRAY['kind','reason']
            )
            AND jsonb_typeof(terminal_cause -> 'reason') = 'string'
        WHEN 'scheduler_no_progress' THEN
            terminal_cause = '{"kind":"scheduler_no_progress"}'::JSONB
        WHEN 'replan_stop' THEN
            moa.execution_json_object_has_exact_keys(
                terminal_cause, ARRAY['kind','reason']
            )
            AND jsonb_typeof(terminal_cause -> 'reason') = 'string'
        WHEN 'cancellation' THEN
            terminal_cause = '{"kind":"cancellation"}'::JSONB
        WHEN 'internal_failure' THEN
            terminal_cause = '{"kind":"internal_failure"}'::JSONB
        ELSE FALSE
    END) THEN
        RETURN NULL;
    END IF;
    IF source_kind_value NOT IN (
        'generated_plan','skill_template','experiment_template'
    ) THEN
        RETURN NULL;
    END IF;
    CASE cause_kind
        WHEN 'cancellation' THEN
            RETURN CASE WHEN status_value = 'cancelled' THEN 'cancelled' END;
        WHEN 'internal_failure' THEN
            RETURN CASE WHEN status_value = 'failed' THEN 'internal_failure' END;
        WHEN 'replan_stop' THEN
            IF status_value NOT IN ('partial','blocked') THEN
                RETURN NULL;
            END IF;
            RETURN CASE cause_reason
                WHEN 'duplicate_plan' THEN 'duplicate_plan'
                WHEN 'duplicate_amendment' THEN 'duplicate_amendment'
                WHEN 'repeated_failure' THEN 'repeated_failure'
                WHEN 'no_progress' THEN 'no_progress'
                WHEN 'deadline_exceeded' THEN 'deadline_exceeded'
                WHEN 'budget_exhausted' THEN 'budget_exhausted'
                ELSE NULL
            END;
        WHEN 'limit_stop' THEN
            IF status_value NOT IN ('partial','failed') THEN
                RETURN NULL;
            END IF;
            RETURN CASE cause_reason
                WHEN 'deadline_exceeded' THEN 'deadline_exceeded'
                WHEN 'budget_exceeded' THEN 'budget_exceeded'
                ELSE NULL
            END;
        WHEN 'scheduler_no_progress' THEN
            RETURN CASE status_value
                WHEN 'unsupported' THEN 'unsupported_plan'
                WHEN 'partial' THEN 'no_progress'
                WHEN 'blocked' THEN 'no_progress'
                WHEN 'failed' THEN 'no_progress'
                ELSE NULL
            END;
        WHEN 'task_failure' THEN
            IF failure_class NOT IN (
                'retryable','dependency_failed','invalid_input','invalid_output',
                'authorization_denied','budget_exceeded','deadline_exceeded',
                'cancelled','unsupported','terminal'
            ) THEN
                RETURN NULL;
            END IF;
            RETURN CASE status_value
                WHEN 'unsupported' THEN 'unsupported_plan'
                WHEN 'blocked' THEN 'blocked'
                WHEN 'partial' THEN CASE failure_class
                    WHEN 'deadline_exceeded' THEN 'deadline_exceeded'
                    WHEN 'budget_exceeded' THEN 'budget_exceeded'
                    ELSE 'goal_incomplete'
                END
                WHEN 'failed' THEN CASE failure_class
                    WHEN 'deadline_exceeded' THEN 'deadline_exceeded'
                    WHEN 'budget_exceeded' THEN 'budget_exceeded'
                    ELSE 'task_failure'
                END
                ELSE NULL
            END;
        WHEN 'completion' THEN
            RETURN CASE status_value
                WHEN 'completed' THEN
                    CASE WHEN terminal_cause -> 'limit_stop' = 'null'::JSONB
                         THEN 'completed' END
                WHEN 'blocked' THEN
                    CASE WHEN terminal_cause -> 'limit_stop' = 'null'::JSONB
                         THEN 'blocked' END
                WHEN 'unsupported' THEN
                    CASE WHEN terminal_cause -> 'limit_stop' = 'null'::JSONB
                         THEN 'unsupported_plan' END
                WHEN 'partial' THEN CASE limit_stop
                    WHEN 'deadline_exceeded' THEN 'deadline_exceeded'
                    WHEN 'budget_exceeded' THEN 'budget_exceeded'
                    ELSE 'goal_incomplete'
                END
                WHEN 'failed' THEN CASE limit_stop
                    WHEN 'deadline_exceeded' THEN 'deadline_exceeded'
                    WHEN 'budget_exceeded' THEN 'budget_exceeded'
                    ELSE 'goal_incomplete'
                END
                ELSE NULL
            END;
        ELSE
            RETURN NULL;
    END CASE;
END;
$$;

CREATE TABLE moa.execution_planning_context (
    planning_context_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    contact_id UUID,
    contact_scope_id UUID GENERATED ALWAYS AS (
        COALESCE(
            contact_id,
            '00000000-0000-0000-0000-000000000000'::UUID
        )
    ) STORED,
    session_id UUID NOT NULL,
    originating_user_sequence_num BIGINT NOT NULL
        CHECK (originating_user_sequence_num >= 0),
    originating_user_event_hash TEXT NOT NULL
        CHECK (originating_user_event_hash ~ '^[0-9a-f]{64}$'),
    owner_user_id TEXT NOT NULL,
    planning_context_hash TEXT NOT NULL
        CHECK (planning_context_hash ~ '^[0-9a-f]{64}$'),
    snapshot JSONB NOT NULL CHECK (jsonb_typeof(snapshot) = 'object'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT execution_planning_context_origin_uidx
        UNIQUE (tenant_id, session_id, originating_user_sequence_num),
    CONSTRAINT execution_planning_context_contact_not_nil CHECK (
        contact_id IS NULL
        OR contact_id <>
            '00000000-0000-0000-0000-000000000000'::UUID
    ),
    CONSTRAINT execution_planning_context_normalized_scope_key
        UNIQUE (planning_context_uid, tenant_id, contact_scope_id)
);

CREATE OR REPLACE FUNCTION moa.reject_execution_planning_context_mutation()
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
    RAISE EXCEPTION 'execution planning contexts are immutable and append-only';
END;
$$;

CREATE TRIGGER execution_planning_context_immutable_guard
BEFORE UPDATE OR DELETE ON moa.execution_planning_context
FOR EACH ROW EXECUTE FUNCTION moa.reject_execution_planning_context_mutation();

CREATE TABLE moa.execution_run (
    run_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    contact_id UUID,
    contact_scope_id UUID GENERATED ALWAYS AS (
        COALESCE(
            contact_id,
            '00000000-0000-0000-0000-000000000000'::UUID
        )
    ) STORED,
    session_id UUID NOT NULL,
    originating_user_sequence_num BIGINT NOT NULL
        CHECK (originating_user_sequence_num >= 0),
    planning_context_uid UUID NOT NULL,
    planning_context_hash TEXT NOT NULL
        CHECK (planning_context_hash ~ '^[0-9a-f]{64}$'),
    owner_user_id TEXT NOT NULL,
    goal_contract JSONB NOT NULL,
    initial_plan JSONB NOT NULL,
    active_plan JSONB NOT NULL,
    initial_plan_hash TEXT NOT NULL CHECK (initial_plan_hash ~ '^[0-9a-f]{64}$'),
    active_plan_hash TEXT NOT NULL CHECK (active_plan_hash ~ '^[0-9a-f]{64}$'),
    confirmed_plan_hash TEXT
        CHECK (confirmed_plan_hash IS NULL OR confirmed_plan_hash ~ '^[0-9a-f]{64}$'),
    plan_revision BIGINT NOT NULL DEFAULT 1 CHECK (plan_revision >= 1),
    plan_history JSONB NOT NULL DEFAULT '[]'::JSONB
        CHECK (jsonb_typeof(plan_history) = 'array'),
    capability_catalog JSONB NOT NULL,
    authorization_envelope JSONB NOT NULL,
    pinned_instruction_skills JSONB NOT NULL DEFAULT '[]'::JSONB
        CHECK (jsonb_typeof(pinned_instruction_skills) = 'array'),
    source_provenance JSONB NOT NULL,
    source_kind TEXT NOT NULL,
    skill_template_ref TEXT,
    skill_template_revision_uid UUID,
    input JSONB NOT NULL,
    output JSONB,
    completion_check_results JSONB NOT NULL DEFAULT '[]'::JSONB
        CHECK (jsonb_typeof(completion_check_results) = 'array'),
    terminal_gaps JSONB NOT NULL DEFAULT '[]'::JSONB
        CHECK (jsonb_typeof(terminal_gaps) = 'array'),
    terminal_cause JSONB,
    terminal_reason TEXT,
    terminal_satisfied_requirement_count BIGINT,
    terminal_requirement_count BIGINT,
    status TEXT NOT NULL CHECK (status IN (
        'awaiting_confirmation', 'queued', 'running', 'waiting_input',
        'waiting_review', 'waiting_replan', 'completed', 'partial', 'blocked',
        'unsupported', 'failed', 'cancelled'
    )),
    budget_max_cost_microusd BIGINT CHECK (budget_max_cost_microusd >= 0),
    budget_max_tokens BIGINT CHECK (budget_max_tokens >= 0),
    budget_max_tasks BIGINT CHECK (budget_max_tasks >= 0),
    budget_max_tool_calls BIGINT CHECK (budget_max_tool_calls >= 0),
    budget_max_retrieved_bytes BIGINT CHECK (budget_max_retrieved_bytes >= 0),
    budget_deadline_at TIMESTAMPTZ,
    reserved_cost_microusd BIGINT NOT NULL DEFAULT 0 CHECK (reserved_cost_microusd >= 0),
    reserved_tokens BIGINT NOT NULL DEFAULT 0 CHECK (reserved_tokens >= 0),
    reserved_tasks BIGINT NOT NULL DEFAULT 0 CHECK (reserved_tasks >= 0),
    reserved_tool_calls BIGINT NOT NULL DEFAULT 0 CHECK (reserved_tool_calls >= 0),
    reserved_retrieved_bytes BIGINT NOT NULL DEFAULT 0 CHECK (reserved_retrieved_bytes >= 0),
    consumed_cost_microusd BIGINT NOT NULL DEFAULT 0 CHECK (consumed_cost_microusd >= 0),
    consumed_tokens BIGINT NOT NULL DEFAULT 0 CHECK (consumed_tokens >= 0),
    consumed_tasks BIGINT NOT NULL DEFAULT 0 CHECK (consumed_tasks >= 0),
    consumed_tool_calls BIGINT NOT NULL DEFAULT 0 CHECK (consumed_tool_calls >= 0),
    consumed_retrieved_bytes BIGINT NOT NULL DEFAULT 0 CHECK (consumed_retrieved_bytes >= 0),
    budget_overrun BOOLEAN NOT NULL DEFAULT FALSE,
    progress_total_tasks BIGINT NOT NULL DEFAULT 0 CHECK (progress_total_tasks >= 0),
    progress_completed_tasks BIGINT NOT NULL DEFAULT 0 CHECK (progress_completed_tasks >= 0),
    progress_failed_tasks BIGINT NOT NULL DEFAULT 0 CHECK (progress_failed_tasks >= 0),
    progress_cancelled_tasks BIGINT NOT NULL DEFAULT 0 CHECK (progress_cancelled_tasks >= 0),
    waiting_reasons JSONB NOT NULL DEFAULT '[]'::JSONB
        CHECK (jsonb_typeof(waiting_reasons) = 'array'),
    wake_epoch BIGINT NOT NULL DEFAULT 1 CHECK (wake_epoch >= 1),
    processed_wake_epoch BIGINT NOT NULL DEFAULT 0
        CHECK (processed_wake_epoch >= 0 AND processed_wake_epoch <= wake_epoch),
    idempotency_key TEXT,
    cancellation_reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    queued_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    confirmed_at TIMESTAMPTZ,
    CONSTRAINT execution_run_confirmation_pair CHECK (
        (confirmed_plan_hash IS NULL) = (confirmed_at IS NULL)
    ),
    CONSTRAINT execution_run_contact_not_nil CHECK (
        contact_id IS NULL
        OR contact_id <>
            '00000000-0000-0000-0000-000000000000'::UUID
    ),
    CONSTRAINT execution_run_uid_tenant_key UNIQUE (run_uid, tenant_id),
    CONSTRAINT execution_run_normalized_scope_key
        UNIQUE (run_uid, tenant_id, contact_scope_id),
    CONSTRAINT execution_run_planning_context_normalized_scope_fkey
        FOREIGN KEY (planning_context_uid, tenant_id, contact_scope_id)
        REFERENCES moa.execution_planning_context (
            planning_context_uid, tenant_id, contact_scope_id
        ),
    CONSTRAINT execution_run_source_kind_check CHECK (
        source_kind IN (
            'generated_plan','skill_template','experiment_template'
        )
    ),
    CONSTRAINT execution_run_source_route_template_check CHECK (
        CASE source_kind
            WHEN 'generated_plan' THEN
                skill_template_ref IS NULL
                AND skill_template_revision_uid IS NULL
            WHEN 'skill_template' THEN
                skill_template_ref IS NOT NULL
                AND skill_template_revision_uid IS NOT NULL
                AND moa.execution_skill_ref_is_canonical(skill_template_ref)
            WHEN 'experiment_template' THEN
                skill_template_ref IS NOT NULL
                AND skill_template_revision_uid IS NOT NULL
                AND moa.execution_skill_ref_is_canonical(skill_template_ref)
            ELSE FALSE
        END
    ),
    CONSTRAINT execution_run_queued_at CHECK (
        CASE
            WHEN status = 'awaiting_confirmation' THEN queued_at IS NULL
            WHEN status = 'cancelled' AND queued_at IS NULL THEN
                confirmed_at IS NULL
                AND confirmed_plan_hash IS NULL
                AND started_at IS NULL
            ELSE queued_at IS NOT NULL
        END
    ),
    CONSTRAINT execution_run_terminal_evidence CHECK (
        CASE WHEN status IN ('completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled')
            THEN terminal_cause IS NOT NULL
                AND terminal_satisfied_requirement_count IS NOT NULL
                AND terminal_requirement_count IS NOT NULL
                AND terminal_satisfied_requirement_count >= 0
                AND terminal_requirement_count >= 0
                AND terminal_satisfied_requirement_count <= terminal_requirement_count
                AND jsonb_typeof(terminal_cause) = 'object'
                AND CASE terminal_cause ->> 'kind'
                    WHEN 'completion' THEN
                        terminal_cause = jsonb_build_object(
                            'kind', 'completion',
                            'limit_stop', terminal_cause -> 'limit_stop'
                        )
                        AND terminal_cause ? 'limit_stop'
                        AND (
                            terminal_cause -> 'limit_stop' = 'null'::JSONB
                            OR terminal_cause ->> 'limit_stop' IN (
                                'deadline_exceeded', 'budget_exceeded'
                            )
                        )
                        AND (
                            terminal_cause -> 'limit_stop' = 'null'::JSONB
                            OR status IN ('partial', 'failed')
                        )
                        AND status <> 'cancelled'
                    WHEN 'task_failure' THEN
                        terminal_cause = jsonb_build_object(
                            'kind', 'task_failure',
                            'class', terminal_cause -> 'class'
                        )
                        AND terminal_cause ->> 'class' IN (
                            'retryable', 'dependency_failed', 'invalid_input',
                            'invalid_output', 'authorization_denied', 'budget_exceeded',
                            'deadline_exceeded', 'cancelled', 'unsupported', 'terminal'
                        )
                        AND status IN ('partial', 'blocked', 'unsupported', 'failed')
                    WHEN 'limit_stop' THEN
                        terminal_cause = jsonb_build_object(
                            'kind', 'limit_stop',
                            'reason', terminal_cause -> 'reason'
                        )
                        AND terminal_cause ->> 'reason' IN (
                            'deadline_exceeded', 'budget_exceeded'
                        )
                        AND status IN ('partial', 'failed')
                    WHEN 'scheduler_no_progress' THEN
                        terminal_cause = '{"kind":"scheduler_no_progress"}'::JSONB
                        AND status IN ('partial', 'blocked', 'unsupported', 'failed')
                    WHEN 'replan_stop' THEN
                        terminal_cause = jsonb_build_object(
                            'kind', 'replan_stop',
                            'reason', terminal_cause -> 'reason'
                        )
                        AND terminal_cause ->> 'reason' IN (
                            'duplicate_plan', 'duplicate_amendment', 'repeated_failure',
                            'no_progress', 'deadline_exceeded', 'budget_exhausted'
                        )
                        AND status IN ('partial', 'blocked')
                    WHEN 'cancellation' THEN
                        terminal_cause = '{"kind":"cancellation"}'::JSONB
                        AND status = 'cancelled'
                    WHEN 'internal_failure' THEN
                        terminal_cause = '{"kind":"internal_failure"}'::JSONB
                        AND status = 'failed'
                    ELSE FALSE
                END
            ELSE terminal_cause IS NULL
                AND terminal_satisfied_requirement_count IS NULL
                AND terminal_requirement_count IS NULL
        END
    ),
    CONSTRAINT execution_run_terminal_reason_check CHECK (
        CASE
            WHEN status IN (
                'completed','partial','blocked','unsupported','failed','cancelled'
            ) THEN terminal_reason IS NOT NULL
                 AND terminal_reason = moa.execution_terminal_reason_for(
                     status, terminal_cause, source_kind
                 )
            ELSE terminal_reason IS NULL
        END
    ),
    CONSTRAINT execution_run_terminal_reservations_released CHECK (
        status NOT IN (
            'completed','partial','blocked','unsupported','failed','cancelled'
        )
        OR (
            reserved_cost_microusd = 0
            AND reserved_tokens = 0
            AND reserved_tasks = 0
            AND reserved_tool_calls = 0
            AND reserved_retrieved_bytes = 0
        )
    )
);

CREATE UNIQUE INDEX execution_run_scoped_idempotency_uidx
    ON moa.execution_run (
        tenant_id,
        COALESCE(contact_id, '00000000-0000-0000-0000-000000000000'::UUID),
        idempotency_key
    )
    WHERE idempotency_key IS NOT NULL;

CREATE INDEX execution_run_scope_created_idx
    ON moa.execution_run (tenant_id, contact_id, created_at DESC, run_uid DESC);
CREATE INDEX execution_run_nonterminal_idx
    ON moa.execution_run (status, updated_at)
    WHERE status IN (
        'awaiting_confirmation', 'queued', 'running', 'waiting_input',
        'waiting_review', 'waiting_replan'
    );

-- Experiment links preserve the established tenant boundary while clearing
-- only the optional execution identity when its run is deleted.
ALTER TABLE moa.experiment_run
    ADD CONSTRAINT experiment_run_execution_scope_fkey
        FOREIGN KEY (execution_run_uid, tenant_id)
        REFERENCES moa.execution_run (run_uid, tenant_id)
        ON DELETE SET NULL (execution_run_uid);

ALTER TABLE moa.experiment_trial
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

CREATE TABLE moa.execution_task (
    task_id UUID PRIMARY KEY,
    run_uid UUID NOT NULL,
    tenant_id UUID NOT NULL,
    contact_id UUID,
    contact_scope_id UUID GENERATED ALWAYS AS (
        COALESCE(
            contact_id,
            '00000000-0000-0000-0000-000000000000'::UUID
        )
    ) STORED,
    node_id TEXT NOT NULL,
    item_key TEXT NOT NULL,
    requirement_ids JSONB NOT NULL DEFAULT '[]'::JSONB
        CHECK (jsonb_typeof(requirement_ids) = 'array'),
    plan_revision BIGINT NOT NULL CHECK (plan_revision >= 1),
    status TEXT NOT NULL CHECK (status IN (
        'pending', 'reserved', 'running', 'waiting_input', 'waiting_replan',
        'completed', 'skipped', 'failed', 'cancelled'
    )),
    attempt INTEGER NOT NULL DEFAULT 1 CHECK (attempt >= 1),
    generation BIGINT NOT NULL DEFAULT 1 CHECK (generation >= 1),
    input JSONB NOT NULL,
    resume_input_history JSONB NOT NULL DEFAULT '[]'::JSONB
        CHECK (jsonb_typeof(resume_input_history) = 'array'),
    task_kind JSONB NOT NULL,
    retry_policy JSONB NOT NULL,
    estimate_cost_microusd BIGINT NOT NULL CHECK (estimate_cost_microusd >= 0),
    estimate_tokens BIGINT NOT NULL CHECK (estimate_tokens >= 0),
    estimate_tasks BIGINT NOT NULL CHECK (estimate_tasks = 1),
    estimate_tool_calls BIGINT NOT NULL CHECK (estimate_tool_calls >= 0),
    estimate_retrieved_bytes BIGINT NOT NULL CHECK (estimate_retrieved_bytes >= 0),
    reserved_cost_microusd BIGINT NOT NULL DEFAULT 0 CHECK (reserved_cost_microusd >= 0),
    reserved_tokens BIGINT NOT NULL DEFAULT 0 CHECK (reserved_tokens >= 0),
    reserved_tasks BIGINT NOT NULL DEFAULT 0 CHECK (reserved_tasks IN (0, 1)),
    reserved_tool_calls BIGINT NOT NULL DEFAULT 0 CHECK (reserved_tool_calls >= 0),
    reserved_retrieved_bytes BIGINT NOT NULL DEFAULT 0 CHECK (reserved_retrieved_bytes >= 0),
    actual_cost_microusd BIGINT NOT NULL DEFAULT 0 CHECK (actual_cost_microusd >= 0),
    actual_tokens BIGINT NOT NULL DEFAULT 0 CHECK (actual_tokens >= 0),
    actual_tasks BIGINT NOT NULL DEFAULT 0 CHECK (actual_tasks IN (0, 1)),
    actual_tool_calls BIGINT NOT NULL DEFAULT 0 CHECK (actual_tool_calls >= 0),
    actual_retrieved_bytes BIGINT NOT NULL DEFAULT 0 CHECK (actual_retrieved_bytes >= 0),
    current_outcome JSONB,
    output JSONB,
    error JSONB,
    citations JSONB NOT NULL DEFAULT '[]'::JSONB CHECK (jsonb_typeof(citations) = 'array'),
    generation_history JSONB NOT NULL DEFAULT '[]'::JSONB
        CHECK (jsonb_typeof(generation_history) = 'array'),
    outcome_audit JSONB NOT NULL DEFAULT '[]'::JSONB
        CHECK (jsonb_typeof(outcome_audit) = 'array'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    reserved_at TIMESTAMPTZ,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    CONSTRAINT execution_task_contact_not_nil CHECK (
        contact_id IS NULL
        OR contact_id <>
            '00000000-0000-0000-0000-000000000000'::UUID
    ),
    CONSTRAINT execution_task_run_normalized_scope_fkey
        FOREIGN KEY (run_uid, tenant_id, contact_scope_id)
        REFERENCES moa.execution_run (run_uid, tenant_id, contact_scope_id)
        ON DELETE CASCADE,
    CONSTRAINT execution_task_normalized_scope_key
        UNIQUE (task_id, run_uid, tenant_id, contact_scope_id),
    CONSTRAINT execution_task_logical_key UNIQUE (run_uid, node_id, item_key)
);

CREATE INDEX execution_task_run_created_idx
    ON moa.execution_task (run_uid, node_id, item_key, task_id);
CREATE INDEX execution_task_ready_idx
    ON moa.execution_task (run_uid, status, created_at, task_id)
    WHERE status IN ('pending', 'reserved', 'running', 'waiting_input', 'waiting_replan');
CREATE INDEX execution_task_cancellation_idx
    ON moa.execution_task (run_uid, task_id);
CREATE INDEX execution_task_scope_idx
    ON moa.execution_task (tenant_id, contact_id, run_uid);

CREATE TABLE moa.execution_action_review_outbox (
    review_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    contact_id UUID,
    contact_scope_id UUID GENERATED ALWAYS AS (
        COALESCE(
            contact_id,
            '00000000-0000-0000-0000-000000000000'::UUID
        )
    ) STORED,
    run_uid UUID NOT NULL,
    task_id UUID NOT NULL,
    generation BIGINT NOT NULL CHECK (generation >= 1),
    resolution JSONB NOT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    claimed_at TIMESTAMPTZ,
    delivered_at TIMESTAMPTZ,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT execution_action_review_outbox_contact_not_nil CHECK (
        contact_id IS NULL
        OR contact_id <>
            '00000000-0000-0000-0000-000000000000'::UUID
    ),
    CONSTRAINT execution_action_review_outbox_task_normalized_scope_fkey
        FOREIGN KEY (task_id, run_uid, tenant_id, contact_scope_id)
        REFERENCES moa.execution_task (
            task_id, run_uid, tenant_id, contact_scope_id
        )
        ON DELETE CASCADE
);

CREATE INDEX execution_action_review_outbox_pending_idx
    ON moa.execution_action_review_outbox (next_attempt_at, created_at, review_uid)
    WHERE delivered_at IS NULL;

CREATE TABLE moa.execution_template_admission (
    operation_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    contact_id UUID,
    contact_scope_id UUID GENERATED ALWAYS AS (
        COALESCE(
            contact_id,
            '00000000-0000-0000-0000-000000000000'::UUID
        )
    ) STORED,
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
        OR contact_id <>
            '00000000-0000-0000-0000-000000000000'::UUID
    ),
    CONSTRAINT execution_template_admission_run_normalized_scope_fkey
        FOREIGN KEY (execution_run_uid, tenant_id, contact_scope_id)
        REFERENCES moa.execution_run (
            run_uid, tenant_id, contact_scope_id
        )
);

CREATE UNIQUE INDEX execution_template_admission_tenant_key_uidx
    ON moa.execution_template_admission (tenant_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE INDEX execution_template_admission_session_idx
    ON moa.execution_template_admission (tenant_id, session_id, created_at);

CREATE OR REPLACE FUNCTION moa.execution_jsonb_array_has_prefix(
    candidate JSONB,
    prefix JSONB
) RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
    index_value INTEGER;
BEGIN
    IF jsonb_typeof(candidate) <> 'array' OR jsonb_typeof(prefix) <> 'array' THEN
        RETURN FALSE;
    END IF;
    IF jsonb_array_length(candidate) < jsonb_array_length(prefix) THEN
        RETURN FALSE;
    END IF;
    FOR index_value IN 0..jsonb_array_length(prefix) - 1 LOOP
        IF candidate -> index_value IS DISTINCT FROM prefix -> index_value THEN
            RETURN FALSE;
        END IF;
    END LOOP;
    RETURN TRUE;
END;
$$;

CREATE OR REPLACE FUNCTION moa.enforce_execution_run_insert_confirmation() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.status NOT IN ('awaiting_confirmation', 'queued') THEN
        RAISE EXCEPTION 'execution runs must start awaiting confirmation or queued';
    END IF;
    NEW.created_at := NOW();
    NEW.queued_at := CASE
        WHEN NEW.status = 'queued' THEN NEW.created_at
        WHEN NEW.status = 'awaiting_confirmation' THEN NULL
        ELSE NEW.queued_at
    END;
    IF NEW.confirmed_plan_hash IS NOT NULL OR NEW.confirmed_at IS NOT NULL THEN
        RAISE EXCEPTION 'execution run confirmation proof must be created by confirmation';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_run_insert_confirmation_guard
BEFORE INSERT ON moa.execution_run
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_run_insert_confirmation();

CREATE OR REPLACE FUNCTION moa.enforce_execution_run_update() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    transition_allowed BOOLEAN;
    plan_changed BOOLEAN;
BEGIN
    IF NEW.run_uid IS DISTINCT FROM OLD.run_uid
       OR NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
       OR NEW.contact_id IS DISTINCT FROM OLD.contact_id
       OR NEW.session_id IS DISTINCT FROM OLD.session_id
       OR NEW.originating_user_sequence_num IS DISTINCT FROM OLD.originating_user_sequence_num
       OR NEW.planning_context_uid IS DISTINCT FROM OLD.planning_context_uid
       OR NEW.planning_context_hash IS DISTINCT FROM OLD.planning_context_hash
       OR NEW.owner_user_id IS DISTINCT FROM OLD.owner_user_id
       OR NEW.goal_contract IS DISTINCT FROM OLD.goal_contract
       OR NEW.initial_plan IS DISTINCT FROM OLD.initial_plan
       OR NEW.initial_plan_hash IS DISTINCT FROM OLD.initial_plan_hash
       OR NEW.capability_catalog IS DISTINCT FROM OLD.capability_catalog
       OR NEW.authorization_envelope IS DISTINCT FROM OLD.authorization_envelope
       OR NEW.pinned_instruction_skills IS DISTINCT FROM OLD.pinned_instruction_skills
       OR NEW.source_provenance IS DISTINCT FROM OLD.source_provenance
       OR NEW.input IS DISTINCT FROM OLD.input
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION 'execution run immutable fields cannot change';
    END IF;

    IF NEW.source_kind IS DISTINCT FROM OLD.source_kind
       OR NEW.skill_template_ref IS DISTINCT FROM OLD.skill_template_ref
       OR NEW.skill_template_revision_uid IS DISTINCT FROM OLD.skill_template_revision_uid THEN
        RAISE EXCEPTION 'execution run normalized source fields are immutable';
    END IF;

    IF OLD.terminal_reason IS NOT NULL
       AND NEW.terminal_reason IS DISTINCT FROM OLD.terminal_reason THEN
        RAISE EXCEPTION 'execution run terminal reason is immutable';
    END IF;
    IF OLD.terminal_reason IS NULL AND NEW.terminal_reason IS NOT NULL
       AND NOT (
           OLD.status NOT IN (
               'completed','partial','blocked','unsupported','failed','cancelled'
           )
           AND NEW.status IN (
               'completed','partial','blocked','unsupported','failed','cancelled'
           )
           AND NEW.terminal_reason = moa.execution_terminal_reason_for(
               NEW.status, NEW.terminal_cause, NEW.source_kind
           )
       ) THEN
        RAISE EXCEPTION 'execution run terminal reason requires its matching terminal transition';
    END IF;

    IF NEW.wake_epoch < OLD.wake_epoch
       OR NEW.processed_wake_epoch < OLD.processed_wake_epoch
       OR NEW.processed_wake_epoch > NEW.wake_epoch THEN
        RAISE EXCEPTION 'execution run wake epochs must be monotonic and acknowledged in order';
    END IF;

    IF NEW.queued_at IS DISTINCT FROM OLD.queued_at
       AND NOT (
           OLD.status = 'awaiting_confirmation'
           AND NEW.status = 'queued'
           AND OLD.queued_at IS NULL
           AND NEW.queued_at IS NOT NULL
       ) THEN
        RAISE EXCEPTION 'execution run queued timestamp is immutable';
    END IF;

    IF OLD.status IN ('completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled')
       AND (
           NEW.terminal_cause IS DISTINCT FROM OLD.terminal_cause
           OR NEW.terminal_satisfied_requirement_count IS DISTINCT FROM OLD.terminal_satisfied_requirement_count
           OR NEW.terminal_requirement_count IS DISTINCT FROM OLD.terminal_requirement_count
       ) THEN
        RAISE EXCEPTION 'execution run terminal evidence is immutable';
    END IF;

    IF OLD.status = 'awaiting_confirmation' AND NEW.status = 'queued' THEN
        IF OLD.confirmed_plan_hash IS NOT NULL
           OR OLD.confirmed_at IS NOT NULL
           OR NEW.confirmed_plan_hash IS DISTINCT FROM OLD.active_plan_hash
           OR NEW.confirmed_at IS NULL THEN
            RAISE EXCEPTION 'execution run confirmation requires an exact active-plan proof';
        END IF;
    ELSIF NEW.confirmed_plan_hash IS DISTINCT FROM OLD.confirmed_plan_hash
       OR NEW.confirmed_at IS DISTINCT FROM OLD.confirmed_at THEN
        RAISE EXCEPTION 'execution run confirmation proof cannot change';
    END IF;

    IF NOT moa.execution_jsonb_array_has_prefix(NEW.plan_history, OLD.plan_history) THEN
        RAISE EXCEPTION 'execution run plan history is append-only';
    END IF;

    plan_changed := NEW.active_plan IS DISTINCT FROM OLD.active_plan
        OR NEW.active_plan_hash IS DISTINCT FROM OLD.active_plan_hash
        OR NEW.plan_revision IS DISTINCT FROM OLD.plan_revision
        OR NEW.plan_history IS DISTINCT FROM OLD.plan_history;
    IF plan_changed AND NOT (
        NEW.active_plan IS DISTINCT FROM OLD.active_plan
        AND NEW.active_plan_hash IS DISTINCT FROM OLD.active_plan_hash
        AND NEW.plan_revision = OLD.plan_revision + 1
        AND jsonb_array_length(NEW.plan_history) = jsonb_array_length(OLD.plan_history) + 1
    ) THEN
        RAISE EXCEPTION 'execution run plan changes require one fenced history append';
    END IF;

    IF NEW.status IS NOT DISTINCT FROM OLD.status THEN
        RETURN NEW;
    END IF;

    transition_allowed := CASE OLD.status
        WHEN 'awaiting_confirmation' THEN NEW.status IN ('queued', 'cancelled')
        WHEN 'queued' THEN NEW.status IN ('running', 'blocked', 'unsupported', 'failed', 'cancelled')
        WHEN 'running' THEN NEW.status IN (
            'waiting_input', 'waiting_review', 'waiting_replan', 'completed',
            'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        WHEN 'waiting_input' THEN NEW.status IN (
            'running', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        WHEN 'waiting_review' THEN NEW.status IN (
            'running', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        WHEN 'waiting_replan' THEN NEW.status IN (
            'running', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        )
        ELSE FALSE
    END;
    IF NOT transition_allowed THEN
        RAISE EXCEPTION 'invalid execution run status transition: % -> %', OLD.status, NEW.status;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_run_update_guard
BEFORE UPDATE ON moa.execution_run
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_run_update();

CREATE OR REPLACE FUNCTION moa.enforce_execution_task_update() RETURNS TRIGGER
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
       OR NEW.retry_policy IS DISTINCT FROM OLD.retry_policy
       OR NEW.estimate_cost_microusd IS DISTINCT FROM OLD.estimate_cost_microusd
       OR NEW.estimate_tokens IS DISTINCT FROM OLD.estimate_tokens
       OR NEW.estimate_tasks IS DISTINCT FROM OLD.estimate_tasks
       OR NEW.estimate_tool_calls IS DISTINCT FROM OLD.estimate_tool_calls
       OR NEW.estimate_retrieved_bytes IS DISTINCT FROM OLD.estimate_retrieved_bytes
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION 'execution task immutable fields cannot change';
    END IF;

    IF NOT moa.execution_jsonb_array_has_prefix(NEW.resume_input_history, OLD.resume_input_history)
       OR NOT moa.execution_jsonb_array_has_prefix(NEW.generation_history, OLD.generation_history)
       OR NOT moa.execution_jsonb_array_has_prefix(NEW.outcome_audit, OLD.outcome_audit) THEN
        RAISE EXCEPTION 'execution task histories are append-only';
    END IF;

    IF OLD.status = 'running' AND NEW.status = 'running'
       AND (NEW.attempt IS DISTINCT FROM OLD.attempt OR NEW.generation IS DISTINCT FROM OLD.generation) THEN
        IF NEW.attempt <> OLD.attempt + 1 OR NEW.generation <> OLD.generation + 1 THEN
            RAISE EXCEPTION 'execution retry must increment attempt and generation together';
        END IF;
    ELSIF OLD.status = 'waiting_input' AND NEW.status = 'running' THEN
        IF NEW.attempt <> OLD.attempt OR NEW.generation <> OLD.generation + 1 THEN
            RAISE EXCEPTION 'execution input resume must increment only generation';
        END IF;
    ELSIF NEW.attempt IS DISTINCT FROM OLD.attempt OR NEW.generation IS DISTINCT FROM OLD.generation THEN
        RAISE EXCEPTION 'execution task counters changed outside retry or input resume';
    END IF;

    IF NEW.status IS NOT DISTINCT FROM OLD.status THEN
        RETURN NEW;
    END IF;

    transition_allowed := CASE OLD.status
        WHEN 'pending' THEN NEW.status IN ('reserved', 'skipped', 'cancelled')
        WHEN 'reserved' THEN NEW.status IN ('running', 'cancelled')
        WHEN 'running' THEN NEW.status IN (
            'waiting_input', 'waiting_replan', 'completed', 'failed', 'cancelled'
        )
        WHEN 'waiting_input' THEN NEW.status IN ('running', 'cancelled')
        WHEN 'waiting_replan' THEN NEW.status = 'cancelled'
        ELSE FALSE
    END;
    IF NOT transition_allowed THEN
        RAISE EXCEPTION 'invalid execution task status transition: % -> %', OLD.status, NEW.status;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_task_update_guard
BEFORE UPDATE ON moa.execution_task
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_task_update();

SELECT moa.apply_contact_rls('moa.execution_run'::REGCLASS);
SELECT moa.apply_contact_rls('moa.execution_task'::REGCLASS);
SELECT moa.apply_contact_rls('moa.execution_action_review_outbox'::REGCLASS);
SELECT moa.apply_contact_rls('moa.execution_planning_context'::REGCLASS);
SELECT moa.apply_contact_rls('moa.execution_template_admission'::REGCLASS);
