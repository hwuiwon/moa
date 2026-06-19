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
        status: from_db("session status", &status_text)?,
        platform: from_db("platform", &platform_text)?,
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
        status: from_db(
            "session status",
            &row.try_get::<String, _>("status").map_err(map_sqlx_error)?,
        )?,
        platform: from_db(
            "platform",
            &row.try_get::<String, _>("platform")
                .map_err(map_sqlx_error)?,
        )?,
        model: ModelId::new(row.try_get::<String, _>("model").map_err(map_sqlx_error)?),
        updated_at: row
            .try_get::<DateTime<Utc>, _>("updated_at")
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
        task_summary: row
            .try_get::<Option<String>, _>("task_summary")
            .map_err(map_sqlx_error)?,
        started_at: row
            .try_get::<DateTime<Utc>, _>("started_at")
            .map_err(map_sqlx_error)?,
        ended_at: row
            .try_get::<Option<DateTime<Utc>>, _>("ended_at")
            .map_err(map_sqlx_error)?,
        outcome: row
            .try_get::<Option<String>, _>("outcome")
            .map_err(map_sqlx_error)?,
        assessment: parse_segment_assessment(
            row.try_get::<Option<String>, _>("assessment")
                .map_err(map_sqlx_error)?,
        )?,
        outcome_confidence: row
            .try_get::<Option<f64>, _>("outcome_confidence")
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

/// Maps an `experience_records` row into an `ExperienceRecord`.
pub(crate) fn experience_record_from_row(row: &PgRow) -> Result<ExperienceRecord> {
    let user_id = row
        .try_get::<Option<String>, _>("user_id")
        .map_err(map_sqlx_error)?
        .ok_or_else(|| MoaError::StorageError("experience record missing user_id".to_string()))?;
    Ok(ExperienceRecord {
        id: row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?,
        segment_id: SegmentId(
            row.try_get::<Uuid, _>("segment_id")
                .map_err(map_sqlx_error)?,
        ),
        session_id: SessionId(
            row.try_get::<Uuid, _>("session_id")
                .map_err(map_sqlx_error)?,
        ),
        tenant_id: row
            .try_get::<String, _>("tenant_id")
            .map_err(map_sqlx_error)?,
        workspace_id: WorkspaceId(
            row.try_get::<String, _>("workspace_id")
                .map_err(map_sqlx_error)?,
        ),
        user_id: UserId(user_id),
        task_summary: row
            .try_get::<Option<String>, _>("task_summary")
            .map_err(map_sqlx_error)?,
        task_fingerprint: json_column(row, "task_fingerprint_payload")?,
        task_facets: json_column(row, "task_facets")?,
        actions: row
            .try_get::<Vec<String>, _>("actions")
            .map_err(map_sqlx_error)?,
        resources: json_column(row, "resources")?,
        outcome: from_db(
            "segment outcome",
            &row.try_get::<String, _>("outcome")
                .map_err(map_sqlx_error)?,
        )?,
        confidence: row
            .try_get::<f64, _>("confidence")
            .map_err(map_sqlx_error)?,
        evidence: json_column(row, "evidence")?,
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
        duration_ms: row
            .try_get::<Option<i64>, _>("duration_ms")
            .map_err(map_sqlx_error)?
            .map(|value| value as u64),
        assessment_policy_version: row
            .try_get::<String, _>("assessment_policy_version")
            .map_err(map_sqlx_error)?,
        extraction_policy_version: row
            .try_get::<String, _>("extraction_policy_version")
            .map_err(map_sqlx_error)?,
        created_at: row
            .try_get::<DateTime<Utc>, _>("created_at")
            .map_err(map_sqlx_error)?,
    })
}

/// Maps an `experience_attributions` row into an `ExperienceAttribution`.
pub(crate) fn experience_attribution_from_row(row: &PgRow) -> Result<ExperienceAttribution> {
    Ok(ExperienceAttribution {
        id: row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?,
        experience_id: row
            .try_get::<Uuid, _>("experience_id")
            .map_err(map_sqlx_error)?,
        tenant_id: row
            .try_get::<String, _>("tenant_id")
            .map_err(map_sqlx_error)?,
        workspace_id: WorkspaceId(
            row.try_get::<String, _>("workspace_id")
                .map_err(map_sqlx_error)?,
        ),
        user_id: row
            .try_get::<Option<String>, _>("user_id")
            .map_err(map_sqlx_error)?
            .map(UserId),
        subject_type: from_db(
            "attribution subject type",
            &row.try_get::<String, _>("subject_type")
                .map_err(map_sqlx_error)?,
        )?,
        subject_id: row
            .try_get::<String, _>("subject_id")
            .map_err(map_sqlx_error)?,
        effect: from_db(
            "attribution effect",
            &row.try_get::<String, _>("effect").map_err(map_sqlx_error)?,
        )?,
        confidence: row
            .try_get::<f64, _>("confidence")
            .map_err(map_sqlx_error)?,
        evidence: json_column(row, "evidence")?,
        created_at: row
            .try_get::<DateTime<Utc>, _>("created_at")
            .map_err(map_sqlx_error)?,
    })
}

/// Maps a `learning_candidates` row into a `LearningCandidate`.
pub(crate) fn learning_candidate_from_row(row: &PgRow) -> Result<LearningCandidate> {
    Ok(LearningCandidate {
        id: row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?,
        tenant_id: row
            .try_get::<String, _>("tenant_id")
            .map_err(map_sqlx_error)?,
        workspace_id: WorkspaceId(
            row.try_get::<String, _>("workspace_id")
                .map_err(map_sqlx_error)?,
        ),
        user_id: row
            .try_get::<Option<String>, _>("user_id")
            .map_err(map_sqlx_error)?
            .map(UserId),
        candidate_type: from_db(
            "learning candidate type",
            &row.try_get::<String, _>("candidate_type")
                .map_err(map_sqlx_error)?,
        )?,
        status: from_db(
            "learning candidate status",
            &row.try_get::<String, _>("status").map_err(map_sqlx_error)?,
        )?,
        target_id: row
            .try_get::<Option<String>, _>("target_id")
            .map_err(map_sqlx_error)?,
        target_label: row
            .try_get::<Option<String>, _>("target_label")
            .map_err(map_sqlx_error)?,
        task_fingerprint: row
            .try_get::<Option<serde_json::Value>, _>("task_fingerprint_payload")
            .map_err(map_sqlx_error)?
            .map(|value| {
                serde_json::from_value::<TaskFingerprint>(value).map_err(|error| {
                    MoaError::StorageError(format!("invalid task fingerprint payload: {error}"))
                })
            })
            .transpose()?,
        task_facets: row
            .try_get::<Option<serde_json::Value>, _>("task_facets")
            .map_err(map_sqlx_error)?
            .map(|value| {
                serde_json::from_value(value).map_err(|error| {
                    MoaError::StorageError(format!("invalid task facet payload: {error}"))
                })
            })
            .transpose()?,
        payload: row
            .try_get::<serde_json::Value, _>("payload")
            .map_err(map_sqlx_error)?,
        evaluation_payload: row
            .try_get::<Option<serde_json::Value>, _>("evaluation_payload")
            .map_err(map_sqlx_error)?,
        source_experience_ids: row
            .try_get::<Vec<Uuid>, _>("source_experience_ids")
            .map_err(map_sqlx_error)?,
        confidence: row
            .try_get::<Option<f64>, _>("confidence")
            .map_err(map_sqlx_error)?,
        risk_class: from_db(
            "learning risk class",
            &row.try_get::<String, _>("risk_class")
                .map_err(map_sqlx_error)?,
        )?,
        promotion_requirements: row
            .try_get::<Vec<String>, _>("promotion_requirements")
            .map_err(map_sqlx_error)?,
        status_reason: row
            .try_get::<Option<String>, _>("status_reason")
            .map_err(map_sqlx_error)?,
        batch_id: row
            .try_get::<Option<Uuid>, _>("batch_id")
            .map_err(map_sqlx_error)?,
        created_at: row
            .try_get::<DateTime<Utc>, _>("created_at")
            .map_err(map_sqlx_error)?,
        updated_at: row
            .try_get::<DateTime<Utc>, _>("updated_at")
            .map_err(map_sqlx_error)?,
    })
}

/// Maps a `task_strategy_success_rates` row into a task-conditioned aggregate.
pub(crate) fn task_strategy_success_rate_from_row(row: &PgRow) -> Result<TaskStrategySuccessRate> {
    Ok(TaskStrategySuccessRate {
        tenant_id: row
            .try_get::<String, _>("tenant_id")
            .map_err(map_sqlx_error)?,
        task_fingerprint: row
            .try_get::<String, _>("task_fingerprint")
            .map_err(map_sqlx_error)?,
        subject_type: from_db(
            "attribution subject type",
            &row.try_get::<String, _>("subject_type")
                .map_err(map_sqlx_error)?,
        )?,
        subject_id: row
            .try_get::<String, _>("subject_id")
            .map_err(map_sqlx_error)?,
        uses: row.try_get::<i64, _>("uses").map_err(map_sqlx_error)? as u64,
        success_rate: row
            .try_get::<f64, _>("success_rate")
            .map_err(map_sqlx_error)?,
        avg_confidence: row
            .try_get::<f64, _>("avg_confidence")
            .map_err(map_sqlx_error)?,
        avg_token_cost: row
            .try_get::<f64, _>("avg_token_cost")
            .map_err(map_sqlx_error)?,
        avg_turn_count: row
            .try_get::<f64, _>("avg_turn_count")
            .map_err(map_sqlx_error)?,
    })
}

fn json_column<T>(row: &PgRow, column: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let value = row
        .try_get::<serde_json::Value, _>(column)
        .map_err(map_sqlx_error)?;
    serde_json::from_value(value)
        .map_err(|error| MoaError::StorageError(format!("invalid {column} payload: {error}")))
}

fn parse_segment_assessment(value: Option<String>) -> Result<Option<SegmentAssessment>> {
    value
        .map(|value| {
            serde_json::from_str::<SegmentAssessment>(&value).map_err(|error| {
                MoaError::StorageError(format!("invalid segment assessment payload: {error}"))
            })
        })
        .transpose()
}

/// Maps an `action_policy_rules` row into an `ActionPolicyRule`.
pub(crate) fn action_policy_rule_from_row(row: &PgRow) -> Result<ActionPolicyRule> {
    Ok(ActionPolicyRule {
        id: row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?,
        workspace_id: WorkspaceId(
            row.try_get::<String, _>("workspace_id")
                .map_err(map_sqlx_error)?,
        ),
        user_id: row
            .try_get::<Option<String>, _>("user_id")
            .map_err(map_sqlx_error)?
            .map(moa_core::UserId),
        tool: row.try_get::<String, _>("tool").map_err(map_sqlx_error)?,
        pattern: row
            .try_get::<String, _>("pattern")
            .map_err(map_sqlx_error)?,
        effect: from_db(
            "action policy effect",
            &row.try_get::<String, _>("effect").map_err(map_sqlx_error)?,
        )?,
        scope: from_db(
            "action policy scope",
            &row.try_get::<String, _>("scope").map_err(map_sqlx_error)?,
        )?,
        reason: row
            .try_get::<Option<String>, _>("reason")
            .map_err(map_sqlx_error)?,
        created_by: moa_core::UserId(
            row.try_get::<String, _>("created_by")
                .map_err(map_sqlx_error)?,
        ),
        created_at: row
            .try_get::<DateTime<Utc>, _>("created_at")
            .map_err(map_sqlx_error)?,
    })
}
