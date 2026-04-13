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
