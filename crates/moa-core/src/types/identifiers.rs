//! Identifier newtypes shared across MOA crates.

uuid_id!(
    /// Identifier for a MOA session.
    pub struct SessionId
);
uuid_id!(
    /// Identifier for one task segment within a session.
    pub struct SegmentId
);
string_id!(
    /// Identifier for a MOA user.
    pub struct UserId
);
string_id!(
    /// Identifier for a persisted storage partition.
    pub struct StoragePartitionId
);
uuid_id!(
    /// Identifier for a tenant runtime boundary.
    pub struct TenantId
);
impl From<uuid::Uuid> for TenantId {
    fn from(value: uuid::Uuid) -> Self {
        Self(value)
    }
}
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
