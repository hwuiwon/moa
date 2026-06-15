//! Database enum conversion helpers for session queries.

use super::*;

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
        Platform::Api => "api",
    }
}

/// Parses a platform enum from its stored database representation.
pub(crate) fn platform_from_db(value: &str) -> Result<Platform> {
    match value {
        "telegram" => Ok(Platform::Telegram),
        "slack" => Ok(Platform::Slack),
        "discord" => Ok(Platform::Discord),
        "api" => Ok(Platform::Api),
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
        EventType::SubAgentSpawned => "SubAgentSpawned",
        EventType::SubAgentMessageSent => "SubAgentMessageSent",
        EventType::SubAgentStatusChanged => "SubAgentStatusChanged",
        EventType::SubAgentNotificationDelivered => "SubAgentNotificationDelivered",
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
        "SubAgentSpawned" => Ok(EventType::SubAgentSpawned),
        "SubAgentMessageSent" => Ok(EventType::SubAgentMessageSent),
        "SubAgentStatusChanged" => Ok(EventType::SubAgentStatusChanged),
        "SubAgentNotificationDelivered" => Ok(EventType::SubAgentNotificationDelivered),
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
