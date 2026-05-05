//! Query helpers for mapping `PostgreSQL` rows into MOA core types.

use chrono::{DateTime, Utc};
use moa_core::{
    ApprovalRule, CatalogIntent, EventType, IntentSource, IntentStatus, LearningEntry, MoaError,
    ModelId, PendingSignal, PendingSignalId, PendingSignalType, Platform, PolicyAction,
    PolicyScope, ResolutionScore, Result, SegmentId, SessionId, SessionMeta, SessionStatus,
    SessionSummary, TaskSegment, TenantIntent, WorkspaceId,
};
use sqlx::{Row, postgres::PgRow};
use uuid::Uuid;

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
    "id, session_id, tenant_id, segment_index, intent_label, ",
    "intent_confidence::DOUBLE PRECISION AS intent_confidence, task_summary, ",
    "started_at, ended_at, resolution, resolution_signal, ",
    "resolution_confidence::DOUBLE PRECISION AS resolution_confidence, ",
    "tools_used, skills_activated, turn_count, token_cost, previous_segment_id"
);

/// Canonical column list for selecting tenant-intent rows.
pub(crate) const TENANT_INTENT_COLUMNS: &str = concat!(
    "id, tenant_id, label, description, status, source, catalog_ref, example_queries, ",
    "embedding::TEXT AS embedding, segment_count, resolution_rate::DOUBLE PRECISION AS resolution_rate"
);

/// Canonical column list for selecting global catalog rows.
pub(crate) const CATALOG_INTENT_COLUMNS: &str = concat!(
    "id, label, description, category, example_queries, embedding::TEXT AS embedding, ",
    "created_at, updated_at"
);

/// Canonical column list for selecting learning-log rows.
pub(crate) const LEARNING_ENTRY_COLUMNS: &str = concat!(
    "id, tenant_id, learning_type, target_id, target_label, payload, ",
    "confidence::DOUBLE PRECISION AS confidence, source_refs, actor, valid_from, valid_to, ",
    "batch_id, version"
);

/// Converts a session status to its stored database representation.
pub(crate) fn session_status_to_db(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Created => "created",
        SessionStatus::Running => "running",
        SessionStatus::Paused => "paused",
        SessionStatus::WaitingApproval => "waiting_approval",
        SessionStatus::Completed => "completed",
        SessionStatus::Cancelled => "cancelled",
        SessionStatus::Failed => "failed",
    }
}

/// Parses a session status from its stored database representation.
pub(crate) fn session_status_from_db(value: &str) -> Result<SessionStatus> {
    match value {
        "created" => Ok(SessionStatus::Created),
        "running" => Ok(SessionStatus::Running),
        "paused" => Ok(SessionStatus::Paused),
        "waiting_approval" => Ok(SessionStatus::WaitingApproval),
        "completed" => Ok(SessionStatus::Completed),
        "cancelled" => Ok(SessionStatus::Cancelled),
        "failed" => Ok(SessionStatus::Failed),
        _ => Err(MoaError::StorageError(format!(
            "unknown session status value `{value}`"
        ))),
    }
}

/// Converts a platform enum to its stored database representation.
pub(crate) fn platform_to_db(platform: &Platform) -> &'static str {
    match platform {
        Platform::Telegram => "telegram",
        Platform::Slack => "slack",
        Platform::Discord => "discord",
        Platform::Desktop => "desktop",
        Platform::Cli => "cli",
    }
}

/// Parses a platform enum from its stored database representation.
pub(crate) fn platform_from_db(value: &str) -> Result<Platform> {
    match value {
        "telegram" => Ok(Platform::Telegram),
        "slack" => Ok(Platform::Slack),
        "discord" => Ok(Platform::Discord),
        "desktop" => Ok(Platform::Desktop),
        "cli" => Ok(Platform::Cli),
        _ => Err(MoaError::StorageError(format!(
            "unknown platform value `{value}`"
        ))),
    }
}

/// Converts an event type enum to its stored database representation.
pub(crate) fn event_type_to_db(event_type: &EventType) -> &'static str {
    match event_type {
        EventType::SessionCreated => "SessionCreated",
        EventType::SessionStatusChanged => "SessionStatusChanged",
        EventType::SessionCompleted => "SessionCompleted",
        EventType::SegmentStarted => "SegmentStarted",
        EventType::SegmentCompleted => "SegmentCompleted",
        EventType::UserMessage => "UserMessage",
        EventType::QueuedMessage => "QueuedMessage",
        EventType::BrainThinking => "BrainThinking",
        EventType::BrainResponse => "BrainResponse",
        EventType::ToolCall => "ToolCall",
        EventType::ToolResult => "ToolResult",
        EventType::ToolError => "ToolError",
        EventType::ApprovalRequested => "ApprovalRequested",
        EventType::ApprovalDecided => "ApprovalDecided",
        EventType::MemoryRead => "MemoryRead",
        EventType::MemoryWrite => "MemoryWrite",
        EventType::MemoryIngest => "MemoryIngest",
        EventType::HandProvisioned => "HandProvisioned",
        EventType::HandDestroyed => "HandDestroyed",
        EventType::HandError => "HandError",
        EventType::Checkpoint => "Checkpoint",
        EventType::CacheReport => "CacheReport",
        EventType::Error => "Error",
        EventType::Warning => "Warning",
    }
}

/// Parses an event type enum from its stored database representation.
pub(crate) fn event_type_from_db(value: &str) -> Result<EventType> {
    match value {
        "SessionCreated" => Ok(EventType::SessionCreated),
        "SessionStatusChanged" => Ok(EventType::SessionStatusChanged),
        "SessionCompleted" => Ok(EventType::SessionCompleted),
        "SegmentStarted" => Ok(EventType::SegmentStarted),
        "SegmentCompleted" => Ok(EventType::SegmentCompleted),
        "UserMessage" => Ok(EventType::UserMessage),
        "QueuedMessage" => Ok(EventType::QueuedMessage),
        "BrainThinking" => Ok(EventType::BrainThinking),
        "BrainResponse" => Ok(EventType::BrainResponse),
        "ToolCall" => Ok(EventType::ToolCall),
        "ToolResult" => Ok(EventType::ToolResult),
        "ToolError" => Ok(EventType::ToolError),
        "ApprovalRequested" => Ok(EventType::ApprovalRequested),
        "ApprovalDecided" => Ok(EventType::ApprovalDecided),
        "MemoryRead" => Ok(EventType::MemoryRead),
        "MemoryWrite" => Ok(EventType::MemoryWrite),
        "MemoryIngest" => Ok(EventType::MemoryIngest),
        "HandProvisioned" => Ok(EventType::HandProvisioned),
        "HandDestroyed" => Ok(EventType::HandDestroyed),
        "HandError" => Ok(EventType::HandError),
        "Checkpoint" => Ok(EventType::Checkpoint),
        "CacheReport" => Ok(EventType::CacheReport),
        "Error" => Ok(EventType::Error),
        "Warning" => Ok(EventType::Warning),
        _ => Err(MoaError::StorageError(format!(
            "unknown event type value `{value}`"
        ))),
    }
}

/// Converts a pending signal type to its stored representation.
pub(crate) fn pending_signal_type_to_db(signal_type: PendingSignalType) -> &'static str {
    match signal_type {
        PendingSignalType::QueueMessage => "queue_message",
    }
}

/// Parses a pending signal type from its stored representation.
pub(crate) fn pending_signal_type_from_db(value: &str) -> Result<PendingSignalType> {
    match value {
        "queue_message" => Ok(PendingSignalType::QueueMessage),
        other => Err(MoaError::StorageError(format!(
            "unknown pending signal type `{other}`"
        ))),
    }
}

/// Converts an intent status to its stored representation.
pub(crate) fn intent_status_to_db(status: IntentStatus) -> &'static str {
    match status {
        IntentStatus::Proposed => "proposed",
        IntentStatus::Active => "active",
        IntentStatus::Deprecated => "deprecated",
    }
}

/// Parses an intent status from its stored representation.
pub(crate) fn intent_status_from_db(value: &str) -> Result<IntentStatus> {
    match value {
        "proposed" => Ok(IntentStatus::Proposed),
        "active" => Ok(IntentStatus::Active),
        "deprecated" => Ok(IntentStatus::Deprecated),
        other => Err(MoaError::StorageError(format!(
            "unknown intent status `{other}`"
        ))),
    }
}

/// Converts an intent source to its stored representation.
pub(crate) fn intent_source_to_db(source: IntentSource) -> &'static str {
    match source {
        IntentSource::Discovered => "discovered",
        IntentSource::Manual => "manual",
        IntentSource::Catalog => "catalog",
    }
}

/// Parses an intent source from its stored representation.
pub(crate) fn intent_source_from_db(value: &str) -> Result<IntentSource> {
    match value {
        "discovered" => Ok(IntentSource::Discovered),
        "manual" => Ok(IntentSource::Manual),
        "catalog" => Ok(IntentSource::Catalog),
        other => Err(MoaError::StorageError(format!(
            "unknown intent source `{other}`"
        ))),
    }
}

/// Converts a policy action to its stored representation.
pub(crate) fn policy_action_to_db(action: &PolicyAction) -> &'static str {
    match action {
        PolicyAction::Allow => "allow",
        PolicyAction::Deny => "deny",
        PolicyAction::RequireApproval => "require_approval",
    }
}

/// Parses a policy action from its stored representation.
pub(crate) fn policy_action_from_db(value: &str) -> Result<PolicyAction> {
    match value {
        "allow" => Ok(PolicyAction::Allow),
        "deny" => Ok(PolicyAction::Deny),
        "require_approval" => Ok(PolicyAction::RequireApproval),
        other => Err(MoaError::StorageError(format!(
            "unknown approval rule action `{other}`"
        ))),
    }
}

/// Converts a policy scope to its stored representation.
pub(crate) fn policy_scope_to_db(scope: &PolicyScope) -> &'static str {
    match scope {
        PolicyScope::Workspace => "workspace",
        PolicyScope::Global => "global",
    }
}

/// Parses a policy scope from its stored representation.
pub(crate) fn policy_scope_from_db(value: &str) -> Result<PolicyScope> {
    match value {
        "workspace" => Ok(PolicyScope::Workspace),
        "global" => Ok(PolicyScope::Global),
        other => Err(MoaError::StorageError(format!(
            "unknown approval rule scope `{other}`"
        ))),
    }
}

/// Maps a `sessions` row into a `SessionMeta`.
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

/// Converts a `sqlx` error into the session crate storage error variant.
pub(crate) fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}
