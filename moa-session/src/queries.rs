//! Query helpers for mapping `PostgreSQL` rows into MOA core types.

use chrono::{DateTime, Utc};
use moa_core::{
    ApprovalRule, EventType, MoaError, ModelId, PendingSignal, PendingSignalId, PendingSignalType,
    Platform, PolicyAction, PolicyScope, Result, SessionMeta, SessionStatus, SessionSummary,
    WorkspaceId,
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
