//! Canonical column-list constants for session queries.

/// Canonical column list for selecting session rows.
pub(crate) const SESSION_SELECT_COLUMNS: &str = concat!(
    "id, workspace_id, user_id, title, status, platform, platform_channel, model, ",
    "created_at, updated_at, completed_at, parent_session_id, total_input_tokens, ",
    "total_input_tokens_uncached, total_input_tokens_cache_write, total_input_tokens_cache_read, ",
    "total_output_tokens, total_cost_cents, event_count, last_checkpoint_seq"
);

/// Canonical column list for inserting session rows.
pub(crate) const SESSION_INSERT_COLUMNS: &str = concat!(
    "id, workspace_id, user_id, title, status, platform, platform_channel, model, ",
    "created_at, updated_at, completed_at, parent_session_id, total_input_tokens_uncached, ",
    "total_input_tokens_cache_write, total_input_tokens_cache_read, total_output_tokens, ",
    "total_cost_cents, event_count, turn_count, last_checkpoint_seq"
);

/// Canonical column list for selecting event rows.
pub(crate) const EVENT_COLUMNS: &str =
    "id, session_id, sequence_num, event_type, payload, timestamp, brain_id, hand_id, token_count";

/// Canonical column list for selecting session summaries.
pub(crate) const SESSION_SUMMARY_COLUMNS: &str =
    "id, workspace_id, user_id, title, status, platform, model, updated_at";

/// Canonical column list for selecting task segment rows.
pub(crate) const TASK_SEGMENT_COLUMNS: &str = concat!(
    "id, session_id, tenant_id, segment_index, task_summary, started_at, ended_at, outcome, assessment, ",
    "outcome_confidence::DOUBLE PRECISION AS outcome_confidence, ",
    "tools_used, skills_activated, turn_count, token_cost, previous_segment_id"
);

/// Canonical column list for selecting learning-log rows.
pub(crate) const LEARNING_ENTRY_COLUMNS: &str = concat!(
    "id, tenant_id, learning_type, target_id, target_label, payload, ",
    "confidence::DOUBLE PRECISION AS confidence, source_refs, actor, valid_from, valid_to, ",
    "batch_id, version"
);
