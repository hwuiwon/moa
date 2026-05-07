//! Row mapping helpers for session query results.

use super::*;

pub(crate) fn session_meta_from_row(row: &PgRow) -> Result<SessionMeta> {
    let id = row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?;
    let workspace_id = row
        .try_get::<String, _>("workspace_id")
        .map_err(map_sqlx_error)?;
    let user_id = row
        .try_get::<String, _>("user_id")
        .map_err(map_sqlx_error)?;
    let status_text = row.try_get::<String, _>("status").map_err(map_sqlx_error)?;
    let platform_text = row
        .try_get::<String, _>("platform")
        .map_err(map_sqlx_error)?;
    let model = row.try_get::<String, _>("model").map_err(map_sqlx_error)?;

    Ok(SessionMeta {
        id: moa_core::SessionId(id),
        workspace_id: WorkspaceId(workspace_id),
        user_id: moa_core::UserId(user_id),
        title: row
            .try_get::<Option<String>, _>("title")
            .map_err(map_sqlx_error)?,
        status: session_status_from_db(&status_text)?,
        platform: platform_from_db(&platform_text)?,
        platform_channel: row
            .try_get::<Option<String>, _>("platform_channel")
            .map_err(map_sqlx_error)?,
        model: ModelId::new(model),
        created_at: row
            .try_get::<DateTime<Utc>, _>("created_at")
            .map_err(map_sqlx_error)?,
        updated_at: row
            .try_get::<DateTime<Utc>, _>("updated_at")
            .map_err(map_sqlx_error)?,
        completed_at: row
            .try_get::<Option<DateTime<Utc>>, _>("completed_at")
            .map_err(map_sqlx_error)?,
        parent_session_id: row
            .try_get::<Option<Uuid>, _>("parent_session_id")
            .map_err(map_sqlx_error)?
            .map(moa_core::SessionId),
        total_input_tokens: row
            .try_get::<i64, _>("total_input_tokens")
            .map_err(map_sqlx_error)? as usize,
        total_input_tokens_uncached: row
            .try_get::<i64, _>("total_input_tokens_uncached")
            .map_err(map_sqlx_error)? as usize,
        total_input_tokens_cache_write: row
            .try_get::<i64, _>("total_input_tokens_cache_write")
            .map_err(map_sqlx_error)? as usize,
        total_input_tokens_cache_read: row
            .try_get::<i64, _>("total_input_tokens_cache_read")
            .map_err(map_sqlx_error)? as usize,
        total_output_tokens: row
            .try_get::<i64, _>("total_output_tokens")
            .map_err(map_sqlx_error)? as usize,
        total_cost_cents: row
            .try_get::<i64, _>("total_cost_cents")
            .map_err(map_sqlx_error)? as u32,
        event_count: row
            .try_get::<i64, _>("event_count")
            .map_err(map_sqlx_error)? as usize,
        last_checkpoint_seq: row
            .try_get::<Option<i64>, _>("last_checkpoint_seq")
            .map_err(map_sqlx_error)?
            .map(|value| value as u64),
    })
}

/// Maps a `sessions` row into a `SessionSummary`.
pub(crate) fn session_summary_from_row(row: &PgRow) -> Result<SessionSummary> {
    Ok(SessionSummary {
        session_id: moa_core::SessionId(row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?),
        workspace_id: WorkspaceId(
            row.try_get::<String, _>("workspace_id")
                .map_err(map_sqlx_error)?,
        ),
        user_id: moa_core::UserId(
            row.try_get::<String, _>("user_id")
                .map_err(map_sqlx_error)?,
        ),
        title: row
            .try_get::<Option<String>, _>("title")
            .map_err(map_sqlx_error)?,
        status: session_status_from_db(
            &row.try_get::<String, _>("status").map_err(map_sqlx_error)?,
        )?,
        platform: platform_from_db(
            &row.try_get::<String, _>("platform")
                .map_err(map_sqlx_error)?,
        )?,
        model: ModelId::new(row.try_get::<String, _>("model").map_err(map_sqlx_error)?),
        updated_at: row
            .try_get::<DateTime<Utc>, _>("updated_at")
            .map_err(map_sqlx_error)?,
    })
}

/// Maps a `pending_signals` row into a `PendingSignal`.
pub(crate) fn pending_signal_from_row(row: &PgRow) -> Result<PendingSignal> {
    Ok(PendingSignal {
        id: PendingSignalId(row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?),
        session_id: moa_core::SessionId(
            row.try_get::<Uuid, _>("session_id")
                .map_err(map_sqlx_error)?,
        ),
        signal_type: pending_signal_type_from_db(
            &row.try_get::<String, _>("signal_type")
                .map_err(map_sqlx_error)?,
        )?,
        payload: row
            .try_get::<serde_json::Value, _>("payload")
            .map_err(map_sqlx_error)?,
        created_at: row
            .try_get::<DateTime<Utc>, _>("created_at")
            .map_err(map_sqlx_error)?,
    })
}

/// Maps a `task_segments` row into a `TaskSegment`.
pub(crate) fn task_segment_from_row(row: &PgRow) -> Result<TaskSegment> {
    Ok(TaskSegment {
        id: SegmentId(row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?),
        session_id: SessionId(
            row.try_get::<Uuid, _>("session_id")
                .map_err(map_sqlx_error)?,
        ),
        tenant_id: row
            .try_get::<String, _>("tenant_id")
            .map_err(map_sqlx_error)?,
        segment_index: row
            .try_get::<i32, _>("segment_index")
            .map_err(map_sqlx_error)? as u32,
        intent_label: row
            .try_get::<Option<String>, _>("intent_label")
            .map_err(map_sqlx_error)?,
        intent_confidence: row
            .try_get::<Option<f64>, _>("intent_confidence")
            .map_err(map_sqlx_error)?,
        task_summary: row
            .try_get::<Option<String>, _>("task_summary")
            .map_err(map_sqlx_error)?,
        started_at: row
            .try_get::<DateTime<Utc>, _>("started_at")
            .map_err(map_sqlx_error)?,
        ended_at: row
            .try_get::<Option<DateTime<Utc>>, _>("ended_at")
            .map_err(map_sqlx_error)?,
        resolution: row
            .try_get::<Option<String>, _>("resolution")
            .map_err(map_sqlx_error)?,
        resolution_signal: parse_resolution_signal(
            row.try_get::<Option<String>, _>("resolution_signal")
                .map_err(map_sqlx_error)?,
        )?,
        resolution_confidence: row
            .try_get::<Option<f64>, _>("resolution_confidence")
            .map_err(map_sqlx_error)?,
        tools_used: row
            .try_get::<Vec<String>, _>("tools_used")
            .map_err(map_sqlx_error)?,
        skills_activated: row
            .try_get::<Vec<String>, _>("skills_activated")
            .map_err(map_sqlx_error)?,
        turn_count: row
            .try_get::<i32, _>("turn_count")
            .map_err(map_sqlx_error)? as u32,
        token_cost: row
            .try_get::<i64, _>("token_cost")
            .map_err(map_sqlx_error)? as u64,
        previous_segment_id: row
            .try_get::<Option<Uuid>, _>("previous_segment_id")
            .map_err(map_sqlx_error)?
            .map(SegmentId),
    })
}

/// Maps a `tenant_intents` row into a `TenantIntent`.
pub(crate) fn tenant_intent_from_row(row: &PgRow) -> Result<TenantIntent> {
    Ok(TenantIntent {
        id: row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?,
        tenant_id: row
            .try_get::<String, _>("tenant_id")
            .map_err(map_sqlx_error)?,
        label: row.try_get::<String, _>("label").map_err(map_sqlx_error)?,
        description: row
            .try_get::<Option<String>, _>("description")
            .map_err(map_sqlx_error)?,
        status: intent_status_from_db(
            &row.try_get::<String, _>("status").map_err(map_sqlx_error)?,
        )?,
        source: intent_source_from_db(
            &row.try_get::<String, _>("source").map_err(map_sqlx_error)?,
        )?,
        catalog_ref: row
            .try_get::<Option<Uuid>, _>("catalog_ref")
            .map_err(map_sqlx_error)?,
        example_queries: row
            .try_get::<Vec<String>, _>("example_queries")
            .map_err(map_sqlx_error)?,
        embedding: parse_vector_text(
            row.try_get::<Option<String>, _>("embedding")
                .map_err(map_sqlx_error)?,
        )?,
        segment_count: row
            .try_get::<i32, _>("segment_count")
            .map_err(map_sqlx_error)? as u32,
        resolution_rate: row
            .try_get::<Option<f64>, _>("resolution_rate")
            .map_err(map_sqlx_error)?,
    })
}

/// Maps a `global_intent_catalog` row into a `CatalogIntent`.
pub(crate) fn catalog_intent_from_row(row: &PgRow) -> Result<CatalogIntent> {
    Ok(CatalogIntent {
        id: row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?,
        label: row.try_get::<String, _>("label").map_err(map_sqlx_error)?,
        description: row
            .try_get::<String, _>("description")
            .map_err(map_sqlx_error)?,
        category: row
            .try_get::<Option<String>, _>("category")
            .map_err(map_sqlx_error)?,
        example_queries: row
            .try_get::<Vec<String>, _>("example_queries")
            .map_err(map_sqlx_error)?,
        embedding: parse_vector_text(
            row.try_get::<Option<String>, _>("embedding")
                .map_err(map_sqlx_error)?,
        )?,
        created_at: row
            .try_get::<DateTime<Utc>, _>("created_at")
            .map_err(map_sqlx_error)?,
        updated_at: row
            .try_get::<DateTime<Utc>, _>("updated_at")
            .map_err(map_sqlx_error)?,
    })
}

/// Maps a `learning_log` row into a `LearningEntry`.
pub(crate) fn learning_entry_from_row(row: &PgRow) -> Result<LearningEntry> {
    Ok(LearningEntry {
        id: row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?,
        tenant_id: row
            .try_get::<String, _>("tenant_id")
            .map_err(map_sqlx_error)?,
        learning_type: row
            .try_get::<String, _>("learning_type")
            .map_err(map_sqlx_error)?,
        target_id: row
            .try_get::<String, _>("target_id")
            .map_err(map_sqlx_error)?,
        target_label: row
            .try_get::<Option<String>, _>("target_label")
            .map_err(map_sqlx_error)?,
        payload: row
            .try_get::<serde_json::Value, _>("payload")
            .map_err(map_sqlx_error)?,
        confidence: row
            .try_get::<Option<f64>, _>("confidence")
            .map_err(map_sqlx_error)?,
        source_refs: row
            .try_get::<Vec<Uuid>, _>("source_refs")
            .map_err(map_sqlx_error)?,
        actor: row.try_get::<String, _>("actor").map_err(map_sqlx_error)?,
        valid_from: row
            .try_get::<DateTime<Utc>, _>("valid_from")
            .map_err(map_sqlx_error)?,
        valid_to: row
            .try_get::<Option<DateTime<Utc>>, _>("valid_to")
            .map_err(map_sqlx_error)?,
        batch_id: row
            .try_get::<Option<Uuid>, _>("batch_id")
            .map_err(map_sqlx_error)?,
        version: row.try_get::<i32, _>("version").map_err(map_sqlx_error)?,
    })
}

fn parse_resolution_signal(value: Option<String>) -> Result<Option<ResolutionScore>> {
    value
        .map(|value| {
            serde_json::from_str::<ResolutionScore>(&value).map_err(|error| {
                MoaError::StorageError(format!("invalid resolution signal payload: {error}"))
            })
        })
        .transpose()
}

fn parse_vector_text(value: Option<String>) -> Result<Option<Vec<f32>>> {
    value
        .map(|value| {
            let trimmed = value.trim().trim_start_matches('[').trim_end_matches(']');
            if trimmed.is_empty() {
                return Ok(Vec::new());
            }
            trimmed
                .split(',')
                .map(|part| {
                    part.trim().parse::<f32>().map_err(|error| {
                        MoaError::StorageError(format!(
                            "invalid vector component `{}`: {error}",
                            part.trim()
                        ))
                    })
                })
                .collect()
        })
        .transpose()
}

/// Maps an `approval_rules` row into an `ApprovalRule`.
pub(crate) fn approval_rule_from_row(row: &PgRow) -> Result<ApprovalRule> {
    Ok(ApprovalRule {
        id: row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?,
        workspace_id: WorkspaceId(
            row.try_get::<String, _>("workspace_id")
                .map_err(map_sqlx_error)?,
        ),
        tool: row.try_get::<String, _>("tool").map_err(map_sqlx_error)?,
        pattern: row
            .try_get::<String, _>("pattern")
            .map_err(map_sqlx_error)?,
        action: policy_action_from_db(
            &row.try_get::<String, _>("action").map_err(map_sqlx_error)?,
        )?,
        scope: policy_scope_from_db(&row.try_get::<String, _>("scope").map_err(map_sqlx_error)?)?,
        created_by: moa_core::UserId(
            row.try_get::<String, _>("created_by")
                .map_err(map_sqlx_error)?,
        ),
        created_at: row
            .try_get::<DateTime<Utc>, _>("created_at")
            .map_err(map_sqlx_error)?,
    })
}
