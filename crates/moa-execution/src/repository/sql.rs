//! SQL statements owned by execution repository transactions.

pub(super) const RESERVE_EXECUTION_TEMPLATE_ADMISSION_SQL: &str = r#"
    INSERT INTO moa.execution_template_admission (
        operation_uid,
        tenant_id,
        contact_id,
        session_id,
        idempotency_key,
        request_fingerprint
    )
    VALUES ($1, $2, $3, $4, $5, $6)
    ON CONFLICT DO NOTHING
"#;

pub(super) const RECORD_EXECUTION_TEMPLATE_ADMISSION_ORIGIN_SQL: &str = r#"
    UPDATE moa.execution_template_admission
    SET originating_user_sequence_num = $4,
        updated_at = NOW()
    WHERE operation_uid = $1
      AND tenant_id = $2
      AND request_fingerprint = $3
      AND originating_user_sequence_num IS NULL
"#;

pub(super) const RECORD_EXECUTION_TEMPLATE_ADMISSION_RUN_SQL: &str = r#"
    UPDATE moa.execution_template_admission
    SET execution_run_uid = $4,
        updated_at = NOW()
    WHERE operation_uid = $1
      AND tenant_id = $2
      AND request_fingerprint = $3
      AND originating_user_sequence_num IS NOT NULL
      AND execution_run_uid IS NULL
"#;

pub(super) const LOAD_EXECUTION_TEMPLATE_ADMISSION_SQL: &str = r#"
    SELECT
        operation_uid,
        request_fingerprint,
        originating_user_sequence_num,
        execution_run_uid
    FROM moa.execution_template_admission
    WHERE operation_uid = $1 AND tenant_id = $2
"#;

pub(super) const INSERT_ROUTE_AUDIT_SQL: &str = r#"
    INSERT INTO moa.execution_route_audit (
        audit_uid, tenant_id, contact_id, session_id, originating_sequence,
        stage, decision, strategy, source, classifier_outcome,
        provider_model, prompt_version, objective_hash, response_hash,
        confidence_bps, missing_input_count, input_tokens_uncached,
        input_tokens_cache_write, input_tokens_cache_read, output_tokens,
        cost_microusd, duration_micros, accepted_at, created_at
    )
    VALUES (
        $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
        $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $23
    )
    ON CONFLICT DO NOTHING
    RETURNING
        audit_uid, stage, decision, strategy, source, classifier_outcome,
        provider_model, prompt_version, objective_hash, response_hash,
        confidence_bps, missing_input_count, input_tokens_uncached,
        input_tokens_cache_write, input_tokens_cache_read, output_tokens,
        cost_microusd, duration_micros, accepted_at
"#;

pub(super) const LOAD_ROUTE_AUDIT_SQL: &str = r#"
    SELECT
        audit_uid, stage, decision, strategy, source, classifier_outcome,
        provider_model, prompt_version, objective_hash, response_hash,
        confidence_bps, missing_input_count, input_tokens_uncached,
        input_tokens_cache_write, input_tokens_cache_read, output_tokens,
        cost_microusd, duration_micros, accepted_at
    FROM moa.execution_route_audit
    WHERE tenant_id = $1
      AND contact_id IS NOT DISTINCT FROM $2
      AND session_id = $3
      AND originating_sequence = $4
      AND stage = $5
"#;

pub(super) const INSERT_PLANNER_AUDIT_SQL: &str = r#"
    INSERT INTO moa.execution_planner_call_audit (
        audit_uid, tenant_id, contact_id, session_id, originating_sequence,
        run_uid, plan_revision, call_kind, call_ordinal, outcome,
        provider_model, prompt_version, candidate_hash, candidate_json,
        compiler_report, duration_micros, created_at
    )
    VALUES (
        $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
        $11, $12, $13, $14::JSON, $15::JSON, $16, $17
    )
    ON CONFLICT DO NOTHING
    RETURNING
        audit_uid, run_uid, plan_revision, call_kind, call_ordinal, outcome,
        provider_model, prompt_version, candidate_hash,
        candidate_json::TEXT AS candidate_json,
        compiler_report::TEXT AS compiler_report,
        duration_micros
"#;

pub(super) const LOAD_PLANNER_AUDIT_SQL: &str = r#"
    SELECT
        audit_uid, run_uid, plan_revision, call_kind, call_ordinal, outcome,
        provider_model, prompt_version, candidate_hash,
        candidate_json::TEXT AS candidate_json,
        compiler_report::TEXT AS compiler_report,
        duration_micros
    FROM moa.execution_planner_call_audit
    WHERE tenant_id = $1
      AND contact_id IS NOT DISTINCT FROM $2
      AND session_id = $3
      AND originating_sequence = $4
      AND run_uid IS NOT DISTINCT FROM $5
      AND plan_revision IS NOT DISTINCT FROM $6
      AND call_kind = $7
      AND call_ordinal = $8
"#;

pub(super) const INSERT_COMPILE_AUDIT_SQL: &str = r#"
    INSERT INTO moa.execution_compile_audit (
        audit_uid, tenant_id, contact_id, session_id, originating_sequence,
        run_uid, plan_revision, source, operation_key, outcome, candidate_hash,
        final_plan_hash, validation_report, duration_micros, created_at
    )
    VALUES (
        $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
        $11, $12, $13::JSON, $14, $15
    )
    ON CONFLICT DO NOTHING
    RETURNING
        audit_uid, session_id, originating_sequence, run_uid, plan_revision,
        source, operation_key, outcome, candidate_hash, final_plan_hash,
        validation_report::TEXT AS validation_report, duration_micros
"#;

pub(super) const LOAD_COMPILE_AUDIT_SQL: &str = r#"
    SELECT
        audit_uid, session_id, originating_sequence, run_uid, plan_revision,
        source, operation_key, outcome, candidate_hash, final_plan_hash,
        validation_report::TEXT AS validation_report, duration_micros
    FROM moa.execution_compile_audit
    WHERE tenant_id = $1
      AND contact_id IS NOT DISTINCT FROM $2
      AND source = $3
      AND operation_key = $4
"#;

pub(super) const CREATE_PLANNING_CONTEXT_SQL: &str = r#"
    INSERT INTO moa.execution_planning_context (
        planning_context_uid, tenant_id, contact_id, session_id,
        originating_user_sequence_num, originating_user_event_hash,
        owner_user_id, planning_context_hash, snapshot
    ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
    ON CONFLICT (tenant_id, session_id, originating_user_sequence_num) DO NOTHING
    RETURNING planning_context_uid, snapshot, planning_context_hash, created_at
"#;

pub(super) const LOAD_PLANNING_CONTEXT_SQL: &str = r#"
    SELECT planning_context_uid, snapshot, planning_context_hash, created_at
    FROM moa.execution_planning_context
    WHERE planning_context_uid = $1
"#;

pub(super) const LOAD_PLANNING_CONTEXT_BY_ORIGIN_SQL: &str = r#"
    SELECT planning_context_uid, snapshot, planning_context_hash, created_at
    FROM moa.execution_planning_context
    WHERE tenant_id = $1
      AND session_id = $2
      AND originating_user_sequence_num = $3
"#;

pub(super) const CREATE_RUN_SQL: &str = r#"
    INSERT INTO moa.execution_run (
        run_uid, tenant_id, contact_id, session_id, originating_user_sequence_num,
        planning_context_uid, planning_context_hash, owner_user_id,
        goal_contract, initial_plan, active_plan, initial_plan_hash, active_plan_hash,
        capability_catalog, authorization_envelope, pinned_instruction_skills,
        source_provenance, source_kind, skill_template_ref,
        skill_template_revision_uid, input, status,
        budget_max_cost_microusd, budget_max_tokens, budget_max_tasks,
        budget_max_tool_calls, budget_max_retrieved_bytes, budget_deadline_at,
        progress_total_tasks, idempotency_key
    )
    VALUES (
        $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
        $14, $15, $16, $17, $18, $19, $20, $21, $22, $23,
        $24, $25, $26, $27, $28, $29, $30
    )
    ON CONFLICT (
        tenant_id,
        COALESCE(contact_id, '00000000-0000-0000-0000-000000000000'::UUID),
        idempotency_key
    ) WHERE idempotency_key IS NOT NULL
    DO NOTHING
    RETURNING *
"#;

pub(super) const LOAD_RUN_SQL: &str = r#"
    SELECT *
    FROM moa.execution_run
    WHERE run_uid = $1
"#;

pub(super) const LIST_RUNS_SQL: &str = r#"
    SELECT *
    FROM moa.execution_run
    WHERE (
        $1::TIMESTAMPTZ IS NULL
        OR (created_at, run_uid) < ($1, $2)
    )
    ORDER BY created_at DESC, run_uid DESC
    LIMIT $3
"#;

pub(super) const LOAD_RUN_BY_IDEMPOTENCY_SQL: &str = r#"
    SELECT *
    FROM moa.execution_run
    WHERE tenant_id = $1
      AND contact_id IS NOT DISTINCT FROM $2
      AND idempotency_key = $3
"#;

pub(super) const LOAD_RUN_FOR_UPDATE_SQL: &str = r#"
    SELECT *
    FROM moa.execution_run
    WHERE run_uid = $1
    FOR UPDATE
"#;

pub(super) const CONFIRM_RUN_SQL: &str = r#"
    UPDATE moa.execution_run
    SET status = 'queued',
        queued_at = COALESCE(queued_at, NOW()),
        budget_max_cost_microusd = $3,
        budget_max_tokens = $4,
        budget_max_tasks = $5,
        budget_max_tool_calls = $6,
        budget_max_retrieved_bytes = $7,
        budget_deadline_at = $8,
        confirmed_plan_hash = $2,
        confirmed_at = NOW(),
        wake_epoch = wake_epoch + 1,
        updated_at = NOW()
    WHERE run_uid = $1
      AND status = 'awaiting_confirmation'
      AND active_plan_hash = $2
    RETURNING *
"#;

pub(super) const INSERT_NODE_MATERIALIZATION_SQL: &str = r#"
    INSERT INTO moa.execution_node_materialization (
        run_uid, tenant_id, contact_id, plan_revision, node_id,
        kind, fanout_items, reducer_depth
    )
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
    ON CONFLICT (run_uid, plan_revision, node_id) DO NOTHING
    RETURNING kind, fanout_items, reducer_depth
"#;

pub(super) const LOAD_NODE_MATERIALIZATION_SQL: &str = r#"
    SELECT kind, fanout_items, reducer_depth
    FROM moa.execution_node_materialization
    WHERE run_uid = $1 AND plan_revision = $2 AND node_id = $3
"#;

pub(super) const INSERT_TASK_BATCH_SQL: &str = r#"
    WITH input AS (
        SELECT *
        FROM jsonb_to_recordset($1::JSONB) AS row(
            ordinal BIGINT,
            task_id UUID,
            node_id TEXT,
            item_key TEXT,
            requirement_ids JSONB,
            generation BIGINT,
            input JSONB,
            task_kind JSONB,
            compensation_contract JSONB,
            retry_policy JSONB,
            estimate_cost_microusd BIGINT,
            estimate_tokens BIGINT,
            estimate_tasks BIGINT,
            estimate_tool_calls BIGINT,
            estimate_retrieved_bytes BIGINT,
            generation_history JSONB
        )
    )
    INSERT INTO moa.execution_task (
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, task_kind, compensation_contract, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes, generation_history
    )
    SELECT
        input.task_id, $2, $3, $4, input.node_id, input.item_key,
        input.requirement_ids, $5, 'pending', 1, input.generation,
        input.input, input.task_kind, input.compensation_contract, input.retry_policy,
        input.estimate_cost_microusd, input.estimate_tokens,
        input.estimate_tasks, input.estimate_tool_calls,
        input.estimate_retrieved_bytes, input.generation_history
    FROM input
    ORDER BY input.ordinal
    ON CONFLICT (run_uid, node_id, item_key) DO NOTHING
    RETURNING task_id
"#;

pub(super) const LOAD_TASK_BATCH_SQL: &str = r#"
    WITH input AS (
        SELECT *
        FROM jsonb_to_recordset($1::JSONB) AS row(
            ordinal BIGINT,
            task_id UUID,
            node_id TEXT,
            item_key TEXT
        )
    )
    SELECT
        task.task_id, task.run_uid, task.tenant_id, task.contact_id,
        task.node_id, task.item_key, task.requirement_ids, task.plan_revision,
        task.status, task.attempt, task.generation, task.input,
        task.resume_input_history, task.task_kind, task.compensation_contract, task.retry_policy,
        task.estimate_cost_microusd, task.estimate_tokens, task.estimate_tasks,
        task.estimate_tool_calls, task.estimate_retrieved_bytes,
        task.reserved_cost_microusd, task.reserved_tokens, task.reserved_tasks,
        task.reserved_tool_calls, task.reserved_retrieved_bytes,
        task.actual_cost_microusd, task.actual_tokens, task.actual_tasks,
        task.actual_tool_calls, task.actual_retrieved_bytes,
        task.current_outcome, task.output, task.error, task.citations,
        task.generation_history, task.outcome_audit, task.created_at,
        task.updated_at, task.reserved_at, task.started_at, task.completed_at
    FROM input
    JOIN moa.execution_task AS task
      ON task.run_uid = $2
     AND task.task_id = input.task_id
     AND task.node_id = input.node_id
     AND task.item_key = input.item_key
    ORDER BY input.ordinal
"#;

pub(super) const LOAD_TASK_FOR_UPDATE_SQL: &str = r#"
    SELECT
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, compensation_contract, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
    FROM moa.execution_task
    WHERE run_uid = $1 AND task_id = $2
    FOR UPDATE
"#;

pub(super) const LOAD_TASK_SQL: &str = r#"
    SELECT
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, compensation_contract, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
    FROM moa.execution_task
    WHERE run_uid = $1 AND task_id = $2
"#;

pub(super) const RESERVE_RUN_BUDGET_SQL: &str = r#"
    UPDATE moa.execution_run
    SET reserved_cost_microusd = reserved_cost_microusd + $2,
        reserved_tokens = reserved_tokens + $3,
        reserved_tasks = reserved_tasks + $4,
        reserved_tool_calls = reserved_tool_calls + $5,
        reserved_retrieved_bytes = reserved_retrieved_bytes + $6,
        updated_at = NOW()
    WHERE run_uid = $1
      AND status IN ('queued', 'running')
      AND pending_terminal_status IS NULL
      AND (budget_deadline_at IS NULL OR NOW() <= budget_deadline_at)
      AND reserved_cost_microusd <= 9223372036854775807 - $2
      AND reserved_tokens <= 9223372036854775807 - $3
      AND reserved_tasks <= 9223372036854775807 - $4
      AND reserved_tool_calls <= 9223372036854775807 - $5
      AND reserved_retrieved_bytes <= 9223372036854775807 - $6
      AND (
          budget_max_cost_microusd IS NULL
          OR consumed_cost_microusd::NUMERIC + reserved_cost_microusd::NUMERIC + $2::NUMERIC
             <= budget_max_cost_microusd::NUMERIC
      )
      AND (
          budget_max_tokens IS NULL
          OR consumed_tokens::NUMERIC + reserved_tokens::NUMERIC + $3::NUMERIC
             <= budget_max_tokens::NUMERIC
      )
      AND (
          budget_max_tasks IS NULL
          OR consumed_tasks::NUMERIC + reserved_tasks::NUMERIC + $4::NUMERIC
             <= budget_max_tasks::NUMERIC
      )
      AND (
          budget_max_tool_calls IS NULL
          OR consumed_tool_calls::NUMERIC + reserved_tool_calls::NUMERIC + $5::NUMERIC
             <= budget_max_tool_calls::NUMERIC
      )
      AND (
          budget_max_retrieved_bytes IS NULL
          OR consumed_retrieved_bytes::NUMERIC + reserved_retrieved_bytes::NUMERIC + $6::NUMERIC
             <= budget_max_retrieved_bytes::NUMERIC
      )
      AND NOT budget_overrun
"#;

pub(super) const RESERVE_TASK_SQL: &str = r#"
    UPDATE moa.execution_task
    SET status = 'reserved',
        reserved_cost_microusd = $4,
        reserved_tokens = $5,
        reserved_tasks = $6,
        reserved_tool_calls = $7,
        reserved_retrieved_bytes = $8,
        reserved_at = NOW(),
        updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2 AND generation = $3 AND status = 'pending'
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, compensation_contract, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

pub(super) const MARK_TASK_RUNNING_SQL: &str = r#"
    UPDATE moa.execution_task
    SET status = 'running', started_at = COALESCE(started_at, NOW()), updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2 AND generation = $3 AND status = 'reserved'
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, compensation_contract, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

pub(super) const RESUME_TASK_SQL: &str = r#"
    UPDATE moa.execution_task
    SET status = 'running',
        attempt = $5,
        generation = $6,
        generation_history = generation_history || jsonb_build_array($7::JSONB),
        resume_input_history = CASE
            WHEN $8::JSONB IS NULL THEN resume_input_history
            ELSE resume_input_history || jsonb_build_array($8::JSONB)
        END,
        updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2 AND status = $3 AND generation = $4
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, compensation_contract, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

pub(super) const LIST_TASKS_SQL: &str = r#"
    SELECT
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, compensation_contract, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
    FROM moa.execution_task
    WHERE run_uid = $1
      AND (
          $2::TEXT IS NULL
          OR (node_id, item_key, task_id) > ($2, $3, $4)
      )
    ORDER BY node_id, item_key, task_id
    LIMIT $5
"#;

pub(super) const LIST_ALL_TASKS_SQL: &str = r#"
    SELECT
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, compensation_contract, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
    FROM moa.execution_task
    WHERE run_uid = $1
    ORDER BY node_id, item_key, task_id
"#;

pub(super) const RECONCILE_RUN_OUTCOME_SQL: &str = r#"
    UPDATE moa.execution_run
    SET status = $2,
        reserved_cost_microusd = $3,
        reserved_tokens = $4,
        reserved_tasks = $5,
        reserved_tool_calls = $6,
        reserved_retrieved_bytes = $7,
        consumed_cost_microusd = $8,
        consumed_tokens = $9,
        consumed_tasks = $10,
        consumed_tool_calls = $11,
        consumed_retrieved_bytes = $12,
        budget_overrun = $13,
        progress_completed_tasks = progress_completed_tasks + $14,
        progress_failed_tasks = progress_failed_tasks + $15,
        progress_cancelled_tasks = progress_cancelled_tasks + $16,
        wake_epoch = wake_epoch + 1,
        updated_at = NOW()
    WHERE run_uid = $1
    RETURNING *
"#;

pub(super) const RECORD_TASK_OUTCOME_SQL: &str = r#"
    UPDATE moa.execution_task
    SET status = $4,
        reserved_cost_microusd = $5,
        reserved_tokens = $6,
        reserved_tasks = $7,
        reserved_tool_calls = $8,
        reserved_retrieved_bytes = $9,
        actual_cost_microusd = $10,
        actual_tokens = $11,
        actual_tasks = $12,
        actual_tool_calls = $13,
        actual_retrieved_bytes = $14,
        current_outcome = $15,
        output = $16,
        error = $17,
        citations = $18,
        outcome_audit = outcome_audit || jsonb_build_array($19::JSONB),
        completed_at = CASE WHEN $20 THEN COALESCE(completed_at, NOW()) ELSE NULL END,
        updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2 AND generation = $3
      AND status NOT IN ('completed', 'skipped', 'failed', 'cancelled')
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, compensation_contract, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

pub(super) const RECORD_RESERVATION_REJECTION_SQL: &str = r#"
    UPDATE moa.execution_task
    SET status = 'failed',
        reserved_cost_microusd = 0,
        reserved_tokens = 0,
        reserved_tasks = 0,
        reserved_tool_calls = 0,
        reserved_retrieved_bytes = 0,
        actual_cost_microusd = $4,
        actual_tokens = $5,
        actual_tasks = 0,
        actual_tool_calls = $6,
        actual_retrieved_bytes = $7,
        current_outcome = $8,
        output = NULL,
        error = $9,
        citations = $10,
        outcome_audit = outcome_audit || jsonb_build_array($11::JSONB),
        completed_at = NOW(),
        updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2 AND generation = $3 AND status = 'running'
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, compensation_contract, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

pub(super) const APPEND_TASK_OUTCOME_AUDIT_SQL: &str = r#"
    UPDATE moa.execution_task
    SET outcome_audit = outcome_audit || jsonb_build_array($3::JSONB),
        updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, compensation_contract, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

pub(super) const SUPERSEDE_REPLAN_TASK_SQL: &str = r#"
    UPDATE moa.execution_task
    SET status = 'cancelled',
        current_outcome = $4,
        reserved_cost_microusd = 0,
        reserved_tokens = 0,
        reserved_tasks = 0,
        reserved_tool_calls = 0,
        reserved_retrieved_bytes = 0,
        actual_tasks = 1,
        error = jsonb_build_object(
            'class', 'cancelled',
            'message', 'superseded_by_plan_revision'
        ),
        outcome_audit = outcome_audit || jsonb_build_array($3::JSONB),
        completed_at = NOW(),
        updated_at = NOW()
    WHERE run_uid = $1 AND task_id = $2 AND status = 'waiting_replan'
    RETURNING
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, compensation_contract, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
"#;

pub(super) const APPEND_AMENDMENT_SQL: &str = r#"
    UPDATE moa.execution_run
    SET active_plan = $4,
        active_plan_hash = $5,
        plan_revision = $3,
        plan_history = plan_history || jsonb_build_array($6::JSONB),
        status = 'running',
        reserved_cost_microusd = $7,
        reserved_tokens = $8,
        reserved_tasks = $9,
        reserved_tool_calls = $10,
        reserved_retrieved_bytes = $11,
        consumed_tasks = $12,
        budget_overrun = $13,
        progress_cancelled_tasks = progress_cancelled_tasks + 1,
        wake_epoch = wake_epoch + 1,
        updated_at = NOW()
    WHERE run_uid = $1 AND plan_revision = $2 AND status = 'waiting_replan'
    RETURNING *
"#;

pub(super) const LOAD_NONTERMINAL_TASKS_FOR_UPDATE_SQL: &str = r#"
    SELECT
        task_id, run_uid, tenant_id, contact_id, node_id, item_key,
        requirement_ids, plan_revision, status, attempt, generation,
        input, resume_input_history, task_kind, compensation_contract, retry_policy,
        estimate_cost_microusd, estimate_tokens, estimate_tasks,
        estimate_tool_calls, estimate_retrieved_bytes,
        reserved_cost_microusd, reserved_tokens, reserved_tasks,
        reserved_tool_calls, reserved_retrieved_bytes,
        actual_cost_microusd, actual_tokens, actual_tasks,
        actual_tool_calls, actual_retrieved_bytes,
        current_outcome, output, error, citations, generation_history, outcome_audit,
        created_at, updated_at, reserved_at, started_at, completed_at
    FROM moa.execution_task
    WHERE run_uid = $1
      AND status IN ('pending', 'reserved', 'running', 'waiting_input', 'waiting_replan')
    ORDER BY task_id
    FOR UPDATE
"#;

pub(super) const LIST_COMPENSATIONS_SQL: &str = r#"
    SELECT compensation_id, run_uid, forward_task_id, registered_sequence,
           forward_generation, compensator, mapped_input, status, attempt,
           generation, outcome, error, created_at, updated_at, started_at, completed_at
    FROM moa.execution_compensation
    WHERE run_uid = $1
    ORDER BY registered_sequence DESC
"#;

pub(super) const LOAD_COMPENSATION_FOR_UPDATE_SQL: &str = r#"
    SELECT compensation_id, run_uid, forward_task_id, registered_sequence,
           forward_generation, compensator, mapped_input, status, attempt,
           generation, outcome, error, created_at, updated_at, started_at, completed_at
    FROM moa.execution_compensation
    WHERE run_uid = $1 AND compensation_id = $2
    FOR UPDATE
"#;

pub(super) const LOAD_COMPENSATION_BY_FORWARD_TASK_SQL: &str = r#"
    SELECT compensation_id, run_uid, forward_task_id, registered_sequence,
           forward_generation, compensator, mapped_input, status, attempt,
           generation, outcome, error, created_at, updated_at, started_at, completed_at
    FROM moa.execution_compensation
    WHERE run_uid = $1 AND forward_task_id = $2
"#;

pub(super) const INSERT_COMPENSATION_SQL: &str = r#"
    INSERT INTO moa.execution_compensation (
        compensation_id, run_uid, forward_task_id, tenant_id, contact_id, registered_sequence,
        forward_generation, compensator, mapped_input, status, attempt, generation,
        outcome, error, started_at, completed_at
    )
    SELECT $1, run.run_uid, $3, run.tenant_id, run.contact_id, $4, $5, $6, $7,
           $8, 1, 1, $9, $10,
           CASE WHEN $8 = 'pending' THEN NULL ELSE NOW() END,
           CASE WHEN $8 = 'pending' THEN NULL ELSE NOW() END
    FROM moa.execution_run AS run
    WHERE run.run_uid = $2
    ON CONFLICT (forward_task_id) DO NOTHING
    RETURNING compensation_id
"#;

pub(super) const FENCE_RUN_FOR_COMPENSATION_SQL: &str = r#"
    UPDATE moa.execution_run
    SET pending_terminal_status = $4,
        pending_terminal_reason = $5,
        pending_terminal_cause = $6,
        pending_terminal_output = $7,
        cancellation_reason = $8,
        waiting_reasons = '[]'::JSONB,
        wake_epoch = wake_epoch + 1,
        updated_at = NOW()
    WHERE run_uid = $1
      AND plan_revision = $2
      AND wake_epoch = $3
      AND pending_terminal_status IS NULL
      AND status NOT IN ('completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled', 'compensating')
    RETURNING *
"#;

pub(super) const BEGIN_COMPENSATION_SQL: &str = r#"
    UPDATE moa.execution_run
    SET status = 'compensating',
        waiting_reasons = '[]'::JSONB,
        wake_epoch = wake_epoch + 1,
        updated_at = NOW()
    WHERE run_uid = $1
      AND plan_revision = $2
      AND wake_epoch = $3
      AND pending_terminal_status IS NOT NULL
      AND status NOT IN ('completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled', 'compensating')
      AND NOT EXISTS (
          SELECT 1 FROM moa.execution_task
          WHERE run_uid = $1
            AND status NOT IN ('completed', 'skipped', 'failed', 'cancelled')
      )
    RETURNING *
"#;

pub(super) const CLAIM_COMPENSATION_SQL: &str = r#"
    UPDATE moa.execution_compensation
    SET status = 'running',
        started_at = COALESCE(started_at, NOW()),
        updated_at = NOW()
    WHERE run_uid = $1
      AND compensation_id = $2
      AND generation = $3
      AND status = 'pending'
      AND registered_sequence = (
          SELECT MAX(registered_sequence)
          FROM moa.execution_compensation
          WHERE run_uid = $1 AND status <> 'completed'
      )
    RETURNING compensation_id, run_uid, forward_task_id, registered_sequence,
              forward_generation, compensator, mapped_input, status, attempt,
              generation, outcome, error, created_at, updated_at, started_at, completed_at
"#;
