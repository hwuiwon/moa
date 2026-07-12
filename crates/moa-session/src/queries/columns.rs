//! Canonical column-list constants for session queries.

/// Canonical column list for selecting session rows.
pub(crate) const SESSION_SELECT_COLUMNS: &str = concat!(
    "id, tenant_id, storage_partition_id, title, status, channel, active_channel_binding_id, model, ",
    "created_at, updated_at, completed_at, parent_session_id, contact_id, contact_tenant_id, ",
    "contact_state, contact_canonical_id, contact_linked_ids, contact_scopes, ",
    "created_by_actor_type, created_by_actor_id, contact_promoted_from_id, total_input_tokens, ",
    "total_input_tokens_uncached, total_input_tokens_cache_write, total_input_tokens_cache_read, ",
    "total_output_tokens, total_cost_cents, event_count, last_checkpoint_seq"
);

/// Canonical column list for inserting session rows.
pub(crate) const SESSION_INSERT_COLUMNS: &str = concat!(
    "id, tenant_id, storage_partition_id, user_id, title, status, channel, active_channel_binding_id, model, ",
    "created_at, updated_at, completed_at, parent_session_id, contact_id, contact_tenant_id, ",
    "contact_state, contact_canonical_id, contact_linked_ids, contact_scopes, ",
    "created_by_actor_type, created_by_actor_id, contact_promoted_from_id, total_input_tokens_uncached, ",
    "total_input_tokens_cache_write, total_input_tokens_cache_read, total_output_tokens, ",
    "total_cost_cents, event_count, turn_count, last_checkpoint_seq"
);

/// Canonical column list for selecting event rows.
pub(crate) const EVENT_COLUMNS: &str =
    "id, session_id, sequence_num, event_type, payload, timestamp, brain_id, hand_id, token_count";

/// Canonical column list for selecting session summaries.
pub(crate) const SESSION_SUMMARY_COLUMNS: &str = concat!(
    "id, tenant_id, storage_partition_id, title, status, channel, model, updated_at, ",
    "contact_id, contact_tenant_id, contact_state, contact_canonical_id, contact_linked_ids, ",
    "contact_scopes, created_by_actor_type, created_by_actor_id"
);

/// Canonical column list for selecting task segment rows.
pub(crate) const TASK_SEGMENT_COLUMNS: &str = concat!(
    "id, session_id, tenant_id, segment_index, task_summary, started_at, ended_at, outcome, assessment, ",
    "outcome_confidence::DOUBLE PRECISION AS outcome_confidence, ",
    "tools_used, skills_activated, skills_used, turn_count, token_cost, previous_segment_id"
);

/// Canonical column list for selecting learning-log rows.
pub(crate) const LEARNING_ENTRY_COLUMNS: &str = concat!(
    "id, tenant_id, learning_type, target_id, target_label, payload, ",
    "confidence::DOUBLE PRECISION AS confidence, source_refs, actor, valid_from, valid_to, ",
    "batch_id, version"
);

/// Canonical column list for selecting experience records.
pub(crate) const EXPERIENCE_RECORD_COLUMNS: &str = concat!(
    "id, segment_id, session_id, tenant_id, storage_partition_id, user_id, task_summary, ",
    "task_fingerprint, task_fingerprint_payload, task_facets, actions, resources, outcome, ",
    "confidence::DOUBLE PRECISION AS confidence, evidence, tools_used, skills_activated, skills_used, ",
    "turn_count, token_cost, duration_ms, assessment_policy_version, extraction_policy_version, ",
    "created_at"
);

/// Canonical column list for selecting experience attributions.
pub(crate) const EXPERIENCE_ATTRIBUTION_COLUMNS: &str = concat!(
    "id, experience_id, tenant_id, storage_partition_id, user_id, subject_type, subject_id, effect, kind, ",
    "confidence::DOUBLE PRECISION AS confidence, evidence, created_at"
);

/// Canonical column list for selecting learning candidates.
pub(crate) const LEARNING_CANDIDATE_COLUMNS: &str = concat!(
    "id, tenant_id, storage_partition_id, user_id, candidate_type, status, target_id, target_label, ",
    "task_fingerprint, task_fingerprint_payload, task_facets, payload, evaluation_payload, ",
    "source_experience_ids, confidence::DOUBLE PRECISION AS confidence, risk_class, ",
    "promotion_requirements, status_reason, batch_id, created_at, updated_at"
);

/// Canonical column list for selecting task-conditioned strategy rates.
pub(crate) const TASK_STRATEGY_SUCCESS_RATE_COLUMNS: &str = concat!(
    "tenant_id, task_fingerprint, subject_type, subject_id, uses, ",
    "success_rate::DOUBLE PRECISION AS success_rate, ",
    "avg_confidence::DOUBLE PRECISION AS avg_confidence, ",
    "avg_token_cost::DOUBLE PRECISION AS avg_token_cost, ",
    "avg_turn_count::DOUBLE PRECISION AS avg_turn_count, ",
    "effect_score::DOUBLE PRECISION AS effect_score, unused_injections"
);
