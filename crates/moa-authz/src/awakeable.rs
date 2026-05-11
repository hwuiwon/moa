//! Shared awakeable resolver trait for approval providers.

use async_trait::async_trait;
use thiserror::Error;

/// Error returned when an external resolver cannot resolve a Restate awakeable.
#[derive(Debug, Error)]
pub enum AwakeableResolveError {
    /// Transport or remote-service failure.
    #[error("{0}")]
    Message(String),
}

impl AwakeableResolveError {
    /// Build a resolver error from a displayable message.
    #[must_use]
    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// Resolve a Restate awakeable from outside the waiting handler context.
#[async_trait]
pub trait AwakeableResolver: Send + Sync {
    /// Resolve `awakeable_id` with `payload`.
    async fn resolve(
        &self,
        awakeable_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), AwakeableResolveError>;
}
