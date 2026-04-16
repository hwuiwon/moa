//! Query helpers for mapping libSQL rows into MOA core types.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use libsql::{Row, Value};
use moa_core::{
    EventType, MoaError, Platform, Result, SessionMeta, SessionStatus, SessionSummary, WorkspaceId,
};
use uuid::Uuid;

/// Canonical column list for selecting session rows.
pub(crate) const SESSION_COLUMNS: &str = concat!(
    "id, workspace_id, user_id, title, status, platform, platform_channel, model, ",
    "created_at, updated_at, completed_at, parent_session_id, total_input_tokens, ",
    "total_input_tokens_uncached, total_input_tokens_cache_write, total_input_tokens_cache_read, ",
    "total_output_tokens, total_cost_cents, event_count, last_checkpoint_seq"
);

/// Canonical column list for selecting event rows.
pub(crate) const EVENT_COLUMNS: &str =
    "id, session_id, sequence_num, event_type, payload, timestamp, brain_id, hand_id, token_count";

/// Canonical column list for selecting session summaries.
pub(crate) const SESSION_SUMMARY_COLUMNS: &str =
    "id, workspace_id, user_id, title, status, platform, model, updated_at";

/// Expands a leading `~/` path component using the current user's home directory.
pub(crate) fn expand_local_path(path: &Path) -> Result<PathBuf> {
    if path == Path::new(":memory:") {
        return Ok(path.to_path_buf());
    }

    let raw = path.to_string_lossy();
    if let Some(stripped) = raw.strip_prefix("~/") {
        let home = std::env::var_os("HOME").ok_or(MoaError::HomeDirectoryNotFound)?;
        return Ok(PathBuf::from(home).join(stripped));
    }

    Ok(path.to_path_buf())
}

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

/// Maps a `sessions` row into a `SessionMeta`.
pub(crate) fn session_meta_from_row(row: &Row) -> Result<SessionMeta> {
    let id_text: String = row
        .get(0)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let workspace_id: String = row
        .get(1)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let user_id: String = row
        .get(2)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let status_text: String = row
        .get(4)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let platform_text: String = row
        .get(5)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let model: String = row
        .get(7)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let created_at: String = row
        .get(8)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let updated_at: String = row
        .get(9)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let total_input_tokens: u64 = row
        .get(12)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let total_input_tokens_uncached: u64 = row
        .get(13)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let total_input_tokens_cache_write: u64 = row
        .get(14)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let total_input_tokens_cache_read: u64 = row
        .get(15)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let total_output_tokens: u64 = row
        .get(16)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let total_cost_cents: u32 = row
        .get(17)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let event_count: u64 = row
        .get(18)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;

    Ok(SessionMeta {
        id: moa_core::SessionId(Uuid::parse_str(&id_text)?),
        workspace_id: WorkspaceId(workspace_id),
        user_id: moa_core::UserId(user_id),
        title: optional_text(row, 3)?,
        status: session_status_from_db(&status_text)?,
        platform: platform_from_db(&platform_text)?,
        platform_channel: optional_text(row, 6)?,
        model,
        created_at: parse_timestamp(&created_at)?,
        updated_at: parse_timestamp(&updated_at)?,
        completed_at: optional_text(row, 10)?
            .as_deref()
            .map(parse_timestamp)
            .transpose()?,
        parent_session_id: optional_text(row, 11)?
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()?
            .map(moa_core::SessionId),
        total_input_tokens: total_input_tokens as usize,
        total_input_tokens_uncached: total_input_tokens_uncached as usize,
        total_input_tokens_cache_write: total_input_tokens_cache_write as usize,
        total_input_tokens_cache_read: total_input_tokens_cache_read as usize,
        total_output_tokens: total_output_tokens as usize,
        total_cost_cents,
        event_count: event_count as usize,
        last_checkpoint_seq: optional_i64(row, 19)?.map(|value| value as u64),
    })
}

/// Maps a `sessions` row into a `SessionSummary`.
pub(crate) fn session_summary_from_row(row: &Row) -> Result<SessionSummary> {
    let id_text: String = row
        .get(0)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let workspace_id: String = row
        .get(1)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let user_id: String = row
        .get(2)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let status_text: String = row
        .get(4)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let platform_text: String = row
        .get(5)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let model: String = row
        .get(6)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;
    let updated_at: String = row
        .get(7)
        .map_err(|error| MoaError::StorageError(error.to_string()))?;

    Ok(SessionSummary {
        session_id: moa_core::SessionId(Uuid::parse_str(&id_text)?),
        workspace_id: WorkspaceId(workspace_id),
        user_id: moa_core::UserId(user_id),
        title: optional_text(row, 3)?,
        status: session_status_from_db(&status_text)?,
        platform: platform_from_db(&platform_text)?,
        model,
        updated_at: parse_timestamp(&updated_at)?,
    })
}

/// Parses an RFC 3339 timestamp string as UTC.
pub(crate) fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|error| MoaError::StorageError(error.to_string()))
}

fn optional_text(row: &Row, index: i32) -> Result<Option<String>> {
    match row
        .get_value(index)
        .map_err(|error| MoaError::StorageError(error.to_string()))?
    {
        Value::Null => Ok(None),
        Value::Text(value) => Ok(Some(value)),
        other => Err(MoaError::StorageError(format!(
            "expected text or null at column {index}, found {other:?}"
        ))),
    }
}

fn optional_i64(row: &Row, index: i32) -> Result<Option<i64>> {
    match row
        .get_value(index)
        .map_err(|error| MoaError::StorageError(error.to_string()))?
    {
        Value::Null => Ok(None),
        Value::Integer(value) => Ok(Some(value)),
        other => Err(MoaError::StorageError(format!(
            "expected integer or null at column {index}, found {other:?}"
        ))),
    }
}
