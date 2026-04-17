//! Identifier newtypes shared across MOA crates.

uuid_id!(
    /// Identifier for a MOA session.
    pub struct SessionId
);
uuid_id!(
    /// Identifier for a persisted pending session signal.
    pub struct PendingSignalId
);
string_id!(
    /// Identifier for a MOA user.
    pub struct UserId
);
string_id!(
    /// Identifier for a workspace.
    pub struct WorkspaceId
);
uuid_id!(
    /// Identifier for a brain execution instance.
    pub struct BrainId
);
string_id!(
    /// Stable identifier for an LLM model (e.g., "gpt-5.4", "claude-sonnet-4-6").
    pub struct ModelId
);

impl Default for ModelId {
    fn default() -> Self {
        Self::new("")
    }
}

uuid_id!(
    /// Stable identifier for a single tool call within a session.
    pub struct ToolCallId
);

impl From<uuid::Uuid> for ToolCallId {
    fn from(value: uuid::Uuid) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use super::SessionId;

    #[test]
    fn session_id_roundtrip() {
        let id = SessionId::new();
        let json = serde_json::to_string(&id).expect("serialize session id");
        let parsed: SessionId = serde_json::from_str(&json).expect("deserialize session id");
        assert_eq!(id, parsed);
    }
}
