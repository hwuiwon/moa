-- Durable execution-plan compensation state.

CREATE OR REPLACE FUNCTION moa.execution_plan_definition_is_valid(candidate JSONB)
RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
    node JSONB;
BEGIN
    IF NOT moa.execution_json_object_has_exact_keys(
           candidate,
           ARRAY['cancel_policy', 'input_schema', 'output_schema', 'nodes']
       )
       OR candidate ->> 'cancel_policy' NOT IN (
           'retain_effects', 'compensate_committed'
       )
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
    END LOOP;
    RETURN TRUE;
EXCEPTION
    WHEN OTHERS THEN RETURN FALSE;
END;
$$;

CREATE OR REPLACE FUNCTION moa.execution_plan_snapshot_is_valid(candidate JSONB)
RETURNS BOOLEAN
LANGUAGE sql
IMMUTABLE
AS $$
    SELECT moa.execution_json_object_has_exact_keys(
               candidate,
               ARRAY['definition', 'plan_hash', 'catalog_hash', 'estimate', 'report']
           )
       AND moa.execution_plan_definition_is_valid(candidate -> 'definition')
       AND jsonb_typeof(candidate -> 'plan_hash') = 'string'
       AND candidate ->> 'plan_hash' ~ '^[0-9a-f]{64}$'
$$;

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
    RETURN moa.execution_plan_definition_is_valid(plan);
EXCEPTION
    WHEN OTHERS THEN RETURN FALSE;
END;
$$;

CREATE OR REPLACE FUNCTION moa.execution_pending_terminal_payload_is_valid(candidate JSONB)
RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
    evidence JSONB;
    completion_result JSONB;
    gap JSONB;
BEGIN
    IF NOT moa.execution_json_object_has_exact_keys(
        candidate,
        ARRAY['terminal_evidence', 'completion_check_results', 'terminal_gaps']
    ) THEN
        RETURN FALSE;
    END IF;
    evidence := candidate -> 'terminal_evidence';
    IF NOT moa.execution_json_object_has_exact_keys(
        evidence,
        ARRAY['cause', 'satisfied_requirement_count', 'requirement_count']
    )
    OR jsonb_typeof(evidence -> 'cause') <> 'object'
    OR evidence ->> 'satisfied_requirement_count' !~ '^[0-9]+$'
    OR evidence ->> 'requirement_count' !~ '^[0-9]+$'
    OR (evidence ->> 'satisfied_requirement_count')::NUMERIC
        > (evidence ->> 'requirement_count')::NUMERIC
    OR jsonb_typeof(candidate -> 'completion_check_results') <> 'array'
    OR jsonb_typeof(candidate -> 'terminal_gaps') <> 'array' THEN
        RETURN FALSE;
    END IF;
    FOR completion_result IN
        SELECT value FROM jsonb_array_elements(candidate -> 'completion_check_results')
    LOOP
        IF NOT moa.execution_json_object_has_exact_keys(
            completion_result,
            ARRAY['check_id', 'passed', 'evidence']
        )
        OR jsonb_typeof(completion_result -> 'check_id') <> 'string'
        OR btrim(completion_result ->> 'check_id') = ''
        OR jsonb_typeof(completion_result -> 'passed') <> 'boolean' THEN
            RETURN FALSE;
        END IF;
    END LOOP;
    FOR gap IN SELECT value FROM jsonb_array_elements(candidate -> 'terminal_gaps') LOOP
        IF jsonb_typeof(gap) <> 'string' OR btrim(gap #>> '{}') = '' THEN
            RETURN FALSE;
        END IF;
    END LOOP;
    RETURN TRUE;
EXCEPTION
    WHEN OTHERS THEN RETURN FALSE;
END;
$$;

ALTER TABLE moa.execution_run
    ADD COLUMN next_compensation_sequence BIGINT NOT NULL DEFAULT 1
        CHECK (next_compensation_sequence >= 1),
    ADD COLUMN pending_terminal_status TEXT,
    ADD COLUMN pending_terminal_reason TEXT,
    ADD COLUMN pending_terminal_cause JSONB,
    ADD COLUMN pending_terminal_output JSONB,
    ADD COLUMN manual_repair_required BOOLEAN NOT NULL DEFAULT FALSE,
    ADD CONSTRAINT execution_run_initial_plan_check
        CHECK (moa.execution_plan_snapshot_is_valid(initial_plan)),
    ADD CONSTRAINT execution_run_active_plan_check
        CHECK (moa.execution_plan_snapshot_is_valid(active_plan)),
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
                'waiting_review', 'waiting_replan', 'compensating'
            )
            AND
            pending_terminal_status IN (
                'completed','partial','blocked','unsupported','failed','cancelled'
            )
            AND pending_terminal_reason IS NOT NULL
            AND btrim(pending_terminal_reason) <> ''
            AND moa.execution_pending_terminal_payload_is_valid(
                pending_terminal_cause
            )
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

ALTER TABLE moa.execution_run
    DROP CONSTRAINT execution_run_status_check,
    ADD CONSTRAINT execution_run_status_check CHECK (status IN (
        'awaiting_confirmation', 'queued', 'running', 'waiting_input',
        'waiting_review', 'waiting_replan', 'compensating', 'completed',
        'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
    ));

DROP INDEX moa.execution_run_nonterminal_idx;
CREATE INDEX execution_run_nonterminal_idx
    ON moa.execution_run (status, updated_at)
    WHERE status IN (
        'awaiting_confirmation', 'queued', 'running', 'waiting_input',
        'waiting_review', 'waiting_replan', 'compensating'
    );

ALTER TABLE moa.artifact_revision
    ADD CONSTRAINT artifact_revision_skill_execution_template_check
        CHECK (moa.skill_execution_template_is_valid(definition));

ALTER TABLE public.tenant_action_reviews
    DROP CONSTRAINT tenant_action_reviews_status_check,
    ADD CONSTRAINT tenant_action_reviews_status_check CHECK (
        status IN ('pending', 'cleared', 'denied', 'timeout', 'revoked')
    );

CREATE OR REPLACE FUNCTION moa.enforce_tenant_action_review_status_update()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.status IS NOT DISTINCT FROM OLD.status THEN
        RETURN NEW;
    END IF;
    IF OLD.status = 'pending'
       AND NEW.status IN ('cleared', 'denied', 'timeout', 'revoked') THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'invalid tenant action review status transition: % -> %',
        OLD.status, NEW.status;
END;
$$;

CREATE TRIGGER tenant_action_reviews_status_update_guard
BEFORE UPDATE OF status ON public.tenant_action_reviews
FOR EACH ROW EXECUTE FUNCTION moa.enforce_tenant_action_review_status_update();

ALTER TABLE moa.execution_task
    ADD COLUMN compensation_contract JSONB,
    ADD CONSTRAINT execution_task_compensation_contract_check CHECK (
        compensation_contract IS NULL
        OR jsonb_typeof(compensation_contract) = 'object'
    ),
    ADD CONSTRAINT execution_task_id_run_key UNIQUE (task_id, run_uid);

CREATE OR REPLACE FUNCTION moa.execution_compensation_outcome_is_valid(
    candidate JSONB,
    allowed_result_kinds TEXT[],
    allow_null_result BOOLEAN
)
RETURNS BOOLEAN
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
    result JSONB;
    result_kind TEXT;
    usage JSONB;
    audit_entry JSONB;
BEGIN
    IF NOT moa.execution_json_object_has_exact_keys(
        candidate,
        ARRAY['result', 'review_audit']
    ) OR jsonb_typeof(candidate -> 'review_audit') <> 'array' THEN
        RETURN FALSE;
    END IF;
    FOR audit_entry IN
        SELECT value FROM jsonb_array_elements(candidate -> 'review_audit')
    LOOP
        IF NOT moa.execution_json_object_has_exact_keys(
            audit_entry,
            ARRAY[
                'review_uid', 'generation', 'accepted', 'resolution',
                'expires_at', 'recorded_at'
            ]
        )
        OR audit_entry ->> 'review_uid'
            !~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
        OR audit_entry ->> 'generation' !~ '^[1-9][0-9]*$'
        OR jsonb_typeof(audit_entry -> 'accepted') <> 'boolean'
        OR (
            (audit_entry ->> 'accepted')::BOOLEAN
            AND jsonb_typeof(audit_entry -> 'resolution') <> 'object'
        )
        OR (
            NOT (audit_entry ->> 'accepted')::BOOLEAN
            AND audit_entry -> 'resolution' <> 'null'::JSONB
        )
        OR jsonb_typeof(audit_entry -> 'expires_at') <> 'string'
        OR btrim(audit_entry ->> 'expires_at') = ''
        OR jsonb_typeof(audit_entry -> 'recorded_at') <> 'string'
        OR btrim(audit_entry ->> 'recorded_at') = '' THEN
            RETURN FALSE;
        END IF;
    END LOOP;
    result := candidate -> 'result';
    IF result = 'null'::JSONB THEN
        RETURN allow_null_result;
    END IF;
    IF jsonb_typeof(result) <> 'object' THEN
        RETURN FALSE;
    END IF;
    result_kind := result ->> 'kind';
    IF result_kind IS NULL OR NOT result_kind = ANY (allowed_result_kinds) THEN
        RETURN FALSE;
    END IF;
    IF result_kind = 'completed' THEN
        IF NOT moa.execution_json_object_has_exact_keys(
            result,
            ARRAY['kind', 'output', 'usage']
        ) THEN
            RETURN FALSE;
        END IF;
    ELSIF result_kind = 'failed' THEN
        IF NOT moa.execution_json_object_has_exact_keys(
            result,
            ARRAY['kind', 'message', 'retryable', 'usage']
        )
        OR jsonb_typeof(result -> 'message') <> 'string'
        OR btrim(result ->> 'message') = ''
        OR jsonb_typeof(result -> 'retryable') <> 'boolean' THEN
            RETURN FALSE;
        END IF;
    ELSIF result_kind = 'unknown_outcome' THEN
        IF NOT moa.execution_json_object_has_exact_keys(
            result,
            ARRAY['kind', 'message', 'usage']
        )
        OR jsonb_typeof(result -> 'message') <> 'string'
        OR btrim(result ->> 'message') = '' THEN
            RETURN FALSE;
        END IF;
    ELSE
        RETURN FALSE;
    END IF;
    usage := result -> 'usage';
    RETURN moa.execution_json_object_has_exact_keys(
        usage,
        ARRAY['cost_microusd', 'tokens', 'tool_calls', 'retrieved_bytes']
    )
    AND usage ->> 'cost_microusd' ~ '^[0-9]+$'
    AND usage ->> 'tokens' ~ '^[0-9]+$'
    AND usage ->> 'tool_calls' ~ '^[0-9]+$'
    AND usage ->> 'retrieved_bytes' ~ '^[0-9]+$';
EXCEPTION
    WHEN OTHERS THEN RETURN FALSE;
END;
$$;

CREATE TABLE moa.execution_compensation (
    compensation_id UUID PRIMARY KEY,
    run_uid UUID NOT NULL,
    forward_task_id UUID NOT NULL UNIQUE,
    tenant_id UUID NOT NULL,
    contact_id UUID,
    contact_scope_id UUID GENERATED ALWAYS AS (
        COALESCE(
            contact_id,
            '00000000-0000-0000-0000-000000000000'::UUID
        )
    ) STORED,
    registered_sequence BIGINT NOT NULL CHECK (registered_sequence >= 1),
    forward_generation BIGINT NOT NULL CHECK (forward_generation >= 1),
    compensator JSONB NOT NULL CHECK (jsonb_typeof(compensator) = 'object'),
    mapped_input JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN (
        'pending', 'running', 'completed', 'failed', 'unknown_outcome'
    )),
    attempt BIGINT NOT NULL DEFAULT 1 CHECK (attempt >= 1),
    generation BIGINT NOT NULL DEFAULT 1 CHECK (generation >= 1),
    outcome JSONB,
    error JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    CONSTRAINT execution_compensation_contact_not_nil CHECK (
        contact_id IS NULL
        OR contact_id <> '00000000-0000-0000-0000-000000000000'::UUID
    ),
    CONSTRAINT execution_compensation_run_scope_fkey
        FOREIGN KEY (run_uid, tenant_id, contact_scope_id)
        REFERENCES moa.execution_run (run_uid, tenant_id, contact_scope_id)
        ON DELETE CASCADE,
    CONSTRAINT execution_compensation_forward_task_fkey
        FOREIGN KEY (forward_task_id, run_uid)
        REFERENCES moa.execution_task (task_id, run_uid)
        ON DELETE CASCADE,
    CONSTRAINT execution_compensation_registered_sequence_key
        UNIQUE (run_uid, registered_sequence),
    CONSTRAINT execution_compensation_counter_check CHECK (
        attempt = generation
    ),
    CONSTRAINT execution_compensation_result_check CHECK (
        CASE status
            WHEN 'pending' THEN completed_at IS NULL AND (
                (
                    attempt = 1 AND error IS NULL AND started_at IS NULL
                    AND (
                        outcome IS NULL
                        OR (
                            moa.execution_compensation_outcome_is_valid(
                                outcome, ARRAY['failed'], TRUE
                            )
                            AND outcome -> 'result' = 'null'::JSONB
                        )
                    )
                )
                OR (
                    attempt > 1 AND started_at IS NOT NULL
                    AND moa.execution_compensation_outcome_is_valid(
                        outcome, ARRAY['failed'], FALSE
                    )
                    AND outcome #>> '{result,retryable}' = 'true'
                    AND error ->> 'class' = 'retryable'
                    AND jsonb_typeof(error -> 'message') = 'string'
                    AND btrim(error ->> 'message') <> ''
                )
            )
            WHEN 'running' THEN started_at IS NOT NULL AND completed_at IS NULL
                AND (
                    (outcome IS NULL AND error IS NULL)
                    OR (
                        moa.execution_compensation_outcome_is_valid(
                            outcome, ARRAY['failed'], TRUE
                        )
                        AND (
                            (
                                outcome -> 'result' = 'null'::JSONB
                                AND error IS NULL
                            )
                            OR (
                                outcome #>> '{result,retryable}' = 'true'
                                AND error ->> 'class' = 'retryable'
                                AND jsonb_typeof(error -> 'message') = 'string'
                                AND btrim(error ->> 'message') <> ''
                            )
                        )
                    )
                )
            WHEN 'completed' THEN
                moa.execution_compensation_outcome_is_valid(
                    outcome, ARRAY['completed'], FALSE
                ) AND error IS NULL
                AND started_at IS NOT NULL AND completed_at IS NOT NULL
            WHEN 'failed' THEN
                moa.execution_compensation_outcome_is_valid(
                    outcome, ARRAY['failed'], FALSE
                ) AND error IS NOT NULL
                AND started_at IS NOT NULL AND completed_at IS NOT NULL
            WHEN 'unknown_outcome' THEN
                moa.execution_compensation_outcome_is_valid(
                    outcome, ARRAY['unknown_outcome'], FALSE
                ) AND error IS NOT NULL
                AND started_at IS NOT NULL AND completed_at IS NOT NULL
            ELSE FALSE
        END
    )
);

CREATE INDEX execution_compensation_reverse_order_idx
    ON moa.execution_compensation (run_uid, registered_sequence DESC)
    WHERE status <> 'completed';
CREATE INDEX execution_compensation_scope_idx
    ON moa.execution_compensation (tenant_id, contact_id, run_uid);

ALTER TABLE moa.execution_action_review_outbox
    RENAME COLUMN task_id TO operation_id;
ALTER TABLE moa.execution_action_review_outbox
    ADD COLUMN owner_kind TEXT;
UPDATE moa.execution_action_review_outbox
SET owner_kind = 'task';
ALTER TABLE moa.execution_action_review_outbox
    ALTER COLUMN owner_kind SET NOT NULL,
    ADD CONSTRAINT execution_action_review_outbox_owner_kind_check
        CHECK (owner_kind IN ('task', 'compensation')),
    DROP CONSTRAINT execution_action_review_outbox_task_normalized_scope_fkey,
    ADD CONSTRAINT execution_action_review_outbox_run_normalized_scope_fkey
        FOREIGN KEY (run_uid, tenant_id, contact_scope_id)
        REFERENCES moa.execution_run (run_uid, tenant_id, contact_scope_id)
        ON DELETE CASCADE;

CREATE OR REPLACE FUNCTION moa.enforce_execution_action_review_owner()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    owner_exists BOOLEAN;
BEGIN
    owner_exists := CASE NEW.owner_kind
        WHEN 'task' THEN EXISTS (
            SELECT 1
            FROM moa.execution_task AS task
            WHERE task.task_id = NEW.operation_id
              AND task.run_uid = NEW.run_uid
              AND task.tenant_id = NEW.tenant_id
              AND task.contact_scope_id = NEW.contact_scope_id
        )
        WHEN 'compensation' THEN EXISTS (
            SELECT 1
            FROM moa.execution_compensation AS compensation
            WHERE compensation.compensation_id = NEW.operation_id
              AND compensation.run_uid = NEW.run_uid
              AND compensation.tenant_id = NEW.tenant_id
              AND compensation.contact_scope_id = NEW.contact_scope_id
        )
        ELSE FALSE
    END;
    IF NOT owner_exists THEN
        RAISE EXCEPTION
            'execution action review owner % does not match scoped % operation',
            NEW.operation_id, NEW.owner_kind;
    END IF;
    RETURN NEW;
END;
$$;

CREATE CONSTRAINT TRIGGER execution_action_review_outbox_owner_guard
AFTER INSERT OR UPDATE OF operation_id, owner_kind, run_uid, tenant_id, contact_id
ON moa.execution_action_review_outbox
DEFERRABLE INITIALLY IMMEDIATE
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_action_review_owner();

CREATE OR REPLACE FUNCTION moa.enforce_execution_compensation_update()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    transition_allowed BOOLEAN;
BEGIN
    IF NEW.compensation_id IS DISTINCT FROM OLD.compensation_id
       OR NEW.run_uid IS DISTINCT FROM OLD.run_uid
       OR NEW.forward_task_id IS DISTINCT FROM OLD.forward_task_id
       OR NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
       OR NEW.contact_id IS DISTINCT FROM OLD.contact_id
       OR NEW.registered_sequence IS DISTINCT FROM OLD.registered_sequence
       OR NEW.forward_generation IS DISTINCT FROM OLD.forward_generation
       OR NEW.compensator IS DISTINCT FROM OLD.compensator
       OR NEW.mapped_input IS DISTINCT FROM OLD.mapped_input
       OR NEW.created_at IS DISTINCT FROM OLD.created_at THEN
        RAISE EXCEPTION 'execution compensation immutable fields cannot change';
    END IF;
    IF OLD.started_at IS NOT NULL
       AND NEW.started_at IS DISTINCT FROM OLD.started_at THEN
        RAISE EXCEPTION 'execution compensation started_at is immutable once set';
    END IF;
    IF OLD.completed_at IS NOT NULL
       AND NEW.completed_at IS DISTINCT FROM OLD.completed_at THEN
        RAISE EXCEPTION 'execution compensation completed_at is immutable once set';
    END IF;
    IF OLD.status IN ('completed', 'failed', 'unknown_outcome') THEN
        IF NEW.status IS NOT DISTINCT FROM OLD.status
           AND NEW.attempt IS NOT DISTINCT FROM OLD.attempt
           AND NEW.generation IS NOT DISTINCT FROM OLD.generation
           AND NEW.error IS NOT DISTINCT FROM OLD.error
           AND NEW.outcome -> 'result' IS NOT DISTINCT FROM OLD.outcome -> 'result'
           AND moa.execution_jsonb_array_has_prefix(
               NEW.outcome -> 'review_audit',
               OLD.outcome -> 'review_audit'
           )
           AND jsonb_array_length(NEW.outcome -> 'review_audit')
               = jsonb_array_length(OLD.outcome -> 'review_audit') + 1
           AND NEW.updated_at >= OLD.updated_at THEN
            RETURN NEW;
        END IF;
        RAISE EXCEPTION 'terminal execution compensation permits only one appended review audit';
    END IF;
    IF OLD.status = 'running' AND NEW.status = 'pending' THEN
        IF NEW.attempt <> OLD.attempt + 1
           OR NEW.generation <> OLD.generation + 1 THEN
            RAISE EXCEPTION 'execution compensation retry must increment attempt and generation';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.attempt IS DISTINCT FROM OLD.attempt
       OR NEW.generation IS DISTINCT FROM OLD.generation THEN
        RAISE EXCEPTION 'execution compensation counters changed outside retry';
    END IF;
    IF NEW.status IS NOT DISTINCT FROM OLD.status THEN
        RETURN NEW;
    END IF;
    transition_allowed := CASE OLD.status
        WHEN 'pending' THEN
            NEW.status = 'running'
            OR (
                NEW.status = 'failed'
                AND NEW.error ->> 'class' = 'budget_exceeded'
                AND moa.execution_json_object_has_exact_keys(
                    NEW.error, ARRAY['class', 'message']
                )
                AND NEW.error ->> 'message'
                    = 'approved execution budget cannot reserve compensation'
                AND moa.execution_compensation_outcome_is_valid(
                    NEW.outcome, ARRAY['failed'], FALSE
                )
                AND NEW.outcome #>> '{result,kind}' = 'failed'
                AND NEW.outcome #>> '{result,retryable}' = 'false'
            )
        WHEN 'running' THEN NEW.status IN (
            'completed', 'failed', 'unknown_outcome'
        )
        ELSE FALSE
    END;
    IF NOT transition_allowed THEN
        RAISE EXCEPTION 'invalid execution compensation status transition: % -> %',
            OLD.status, NEW.status;
    END IF;
    IF OLD.status = 'pending' AND NEW.status = 'running' THEN
        NEW.started_at := COALESCE(NEW.started_at, NOW());
    ELSIF OLD.status = 'pending' AND NEW.status = 'failed' THEN
        NEW.started_at := COALESCE(NEW.started_at, NEW.completed_at, NOW());
        NEW.completed_at := COALESCE(NEW.completed_at, NEW.started_at);
    ELSIF OLD.status = 'running'
          AND NEW.status IN ('completed', 'failed', 'unknown_outcome') THEN
        NEW.completed_at := COALESCE(NEW.completed_at, NOW());
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER execution_compensation_update_guard
BEFORE UPDATE ON moa.execution_compensation
FOR EACH ROW EXECUTE FUNCTION moa.enforce_execution_compensation_update();

-- Extend the normalized terminal contract with the one exact manual-repair
-- cause. Keep the V27 cases byte-for-byte and fail if their definition drifted.
DO $execution_compensation_terminal_reason$
DECLARE
    definition TEXT;
    old_validation TEXT := $old$
        WHEN 'internal_failure' THEN
            terminal_cause = '{"kind":"internal_failure"}'::JSONB
        ELSE FALSE
$old$;
    new_validation TEXT := $new$
        WHEN 'internal_failure' THEN
            terminal_cause = '{"kind":"internal_failure"}'::JSONB
        WHEN 'compensation_failure' THEN
            moa.execution_json_object_has_exact_keys(
                terminal_cause,
                ARRAY[
                    'kind','original_status','original_reason','original_cause',
                    'compensation_id','outcome'
                ]
            )
            AND status_value = 'failed'
            AND jsonb_typeof(terminal_cause -> 'original_status') = 'string'
            AND terminal_cause ->> 'original_status' IN (
                'completed','partial','blocked','unsupported','failed','cancelled'
            )
            AND jsonb_typeof(terminal_cause -> 'original_reason') = 'string'
            AND terminal_cause ->> 'original_reason' IN (
                'completed','goal_incomplete','budget_exceeded','deadline_exceeded',
                'cancelled','no_progress','duplicate_plan','duplicate_amendment',
                'repeated_failure','budget_exhausted','task_failure',
                'unsupported_plan','blocked','internal_failure'
            )
            AND jsonb_typeof(terminal_cause -> 'original_cause') = 'object'
            AND terminal_cause ->> 'compensation_id' ~
                '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
            AND jsonb_typeof(terminal_cause -> 'outcome') = 'object'
            AND CASE terminal_cause #>> '{outcome,kind}'
                WHEN 'failed' THEN
                    moa.execution_json_object_has_exact_keys(
                        terminal_cause -> 'outcome',
                        ARRAY['kind','message','retryable','usage']
                    )
                    AND btrim(terminal_cause #>> '{outcome,message}') <> ''
                    AND jsonb_typeof(terminal_cause #> '{outcome,retryable}') = 'boolean'
                    AND jsonb_typeof(terminal_cause #> '{outcome,usage}') = 'object'
                WHEN 'unknown_outcome' THEN
                    moa.execution_json_object_has_exact_keys(
                        terminal_cause -> 'outcome',
                        ARRAY['kind','message','usage']
                    )
                    AND btrim(terminal_cause #>> '{outcome,message}') <> ''
                    AND jsonb_typeof(terminal_cause #> '{outcome,usage}') = 'object'
                ELSE FALSE
            END
        ELSE FALSE
$new$;
    old_reason TEXT := $old$
        WHEN 'internal_failure' THEN
            RETURN CASE WHEN status_value = 'failed' THEN 'internal_failure' END;
        WHEN 'replan_stop' THEN
$old$;
    new_reason TEXT := $new$
        WHEN 'internal_failure' THEN
            RETURN CASE WHEN status_value = 'failed' THEN 'internal_failure' END;
        WHEN 'compensation_failure' THEN
            RETURN CASE WHEN status_value = 'failed' THEN 'compensation_failed' END;
        WHEN 'replan_stop' THEN
$new$;
BEGIN
    SELECT pg_get_functiondef(
        'moa.execution_terminal_reason_for(text,jsonb,text)'::REGPROCEDURE
    ) INTO definition;
    IF position(old_validation IN definition) = 0
       OR position(old_reason IN definition) = 0 THEN
        RAISE EXCEPTION 'execution terminal reason function drifted before V55';
    END IF;
    definition := replace(definition, old_validation, new_validation);
    definition := replace(definition, old_reason, new_reason);
    EXECUTE definition;
END
$execution_compensation_terminal_reason$;

ALTER TABLE moa.execution_run
    DROP CONSTRAINT execution_run_terminal_evidence,
    ADD CONSTRAINT execution_run_terminal_evidence CHECK (
        CASE WHEN status IN (
            'completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
        ) THEN
            terminal_cause IS NOT NULL
            AND terminal_satisfied_requirement_count IS NOT NULL
            AND terminal_requirement_count IS NOT NULL
            AND terminal_satisfied_requirement_count >= 0
            AND terminal_requirement_count >= 0
            AND terminal_satisfied_requirement_count <= terminal_requirement_count
            AND jsonb_typeof(terminal_cause) = 'object'
            AND moa.execution_terminal_reason_for(
                status, terminal_cause, source_kind
            ) IS NOT NULL
        ELSE
            terminal_cause IS NULL
            AND terminal_satisfied_requirement_count IS NULL
            AND terminal_requirement_count IS NULL
        END
    );

-- Preserve every V27 run guard while adding the single compensating state to
-- the transition table. Failing if the exact old block is absent prevents a
-- silent partial replacement when the baseline changes.
DO $$
DECLARE
    definition TEXT;
    old_block TEXT := $old$
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
$old$;
    new_block TEXT := $new$
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
$new$;
BEGIN
    SELECT pg_get_functiondef('moa.enforce_execution_run_update()'::REGPROCEDURE)
    INTO definition;
    IF position(old_block IN definition) = 0 THEN
        RAISE EXCEPTION 'execution run transition block drifted before V55';
    END IF;
    definition := replace(definition, old_block, new_block);
    EXECUTE definition;
END;
$$;

SELECT moa.apply_contact_rls('moa.execution_compensation'::REGCLASS);

-- Compensation rows must drain before their forward task. Shift through a
-- remote range to preserve every existing dependency edge without collisions.
UPDATE moa.tenant_purge_catalog
SET stage_order = stage_order + 1000
WHERE stage_order >= (
    SELECT stage_order FROM moa.tenant_purge_catalog
    WHERE stage_name = 'moa.execution_task'
);
UPDATE moa.tenant_purge_catalog
SET stage_order = stage_order - 999
WHERE stage_order >= 1000;

INSERT INTO moa.tenant_purge_catalog (
    stage_order, stage_name, table_schema, table_name, scope_mode, action_mode
)
SELECT
    task.stage_order - 1,
    'moa.execution_compensation',
    'moa',
    'execution_compensation',
    'tenant_id',
    'delete'
FROM moa.tenant_purge_catalog AS task
WHERE task.stage_name = 'moa.execution_task';

COMMENT ON TABLE moa.tenant_purge_catalog IS
    'Closed 134-table tenant-offboarding residue surface. The two nullable-scope simulator certification authority tables are intentionally global and absent.';

CREATE TRIGGER moa_tenant_purge_fence_insert
AFTER INSERT ON moa.execution_compensation
REFERENCING NEW TABLE AS tenant_purge_new_rows
FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement('tenant_id');
CREATE TRIGGER moa_tenant_purge_fence_update
AFTER UPDATE ON moa.execution_compensation
REFERENCING OLD TABLE AS tenant_purge_old_rows NEW TABLE AS tenant_purge_new_rows
FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement('tenant_id');

DO $execution_compensation_purge$
DECLARE
    predecessor TEXT;
    replacement TEXT;
BEGIN
    SELECT pg_get_functiondef('moa.run_tenant_purge_batch(uuid,text)'::REGPROCEDURE)
    INTO predecessor;
    IF predecessor NOT LIKE '%catalog_count <> 133%'
       OR predecessor NOT LIKE '%exactly 133 tables%'
    THEN
        RAISE EXCEPTION 'unexpected V52 tenant purge function definition'
            USING ERRCODE = '55000';
    END IF;
    replacement := replace(predecessor, 'catalog_count <> 133', 'catalog_count <> 134');
    replacement := replace(replacement, 'exactly 133 tables', 'exactly 134 tables');
    EXECUTE replacement;
END
$execution_compensation_purge$;

ALTER FUNCTION moa.run_tenant_purge_batch(UUID, TEXT) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.run_tenant_purge_batch(UUID, TEXT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.run_tenant_purge_batch(UUID, TEXT)
    TO moa_app, moa_promoter;
