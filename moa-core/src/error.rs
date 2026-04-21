//! Shared error types and failure classification for MOA crates.

use std::io;
use std::time::Duration;

use thiserror::Error;

use crate::types::{SessionId, ToolOutput, WorkspaceId};

/// Convenience result type for MOA libraries.
pub type Result<T> = std::result::Result<T, MoaError>;

/// Common error variants shared across MOA crates.
#[derive(Debug, Error)]
pub enum MoaError {
    /// The requested session does not exist.
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),

    /// The requested workspace does not exist.
    #[error("workspace not found: {0}")]
    WorkspaceNotFound(WorkspaceId),

    /// A provider returned an error.
    #[error("provider error: {0}")]
    ProviderError(String),

    /// A required environment variable is not set.
    #[error("missing environment variable: {0}")]
    MissingEnvironmentVariable(String),

    /// Configuration loading or validation failed.
    #[error("configuration error: {0}")]
    ConfigError(String),

    /// Storage access failed.
    #[error("storage error: {0}")]
    StorageError(String),

    /// A referenced blob payload could not be found.
    #[error("blob not found: {0}")]
    BlobNotFound(String),

    /// Tool execution failed.
    #[error("tool error: {0}")]
    ToolError(String),

    /// Validation failed.
    #[error("validation error: {0}")]
    ValidationError(String),

    /// Recoverable provider shape mismatch — orchestrator pauses, doesn't kill.
    #[error("provider quirk: {0}")]
    ProviderQuirk(String),

    /// Serialization or deserialization failed.
    #[error("serialization error: {0}")]
    SerializationError(String),

    /// An HTTP request returned a non-success status.
    #[error("http status {status}: {message}")]
    HttpStatus {
        /// The HTTP status code.
        status: u16,
        /// Parsed `Retry-After` hint when the upstream provided one.
        retry_after: Option<Duration>,
        /// The error message or response body.
        message: String,
    },

    /// The provider rate limited the request after retries.
    #[error("rate limited after {retries} retries: {message}")]
    RateLimited {
        /// Number of retry attempts performed.
        retries: usize,
        /// Provider-supplied error message when available.
        message: String,
    },

    /// An error occurred while parsing or consuming a stream.
    #[error("stream error: {0}")]
    StreamError(String),

    /// Permission to perform an action was denied.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// The workspace has exhausted its configured daily spend budget.
    #[error("daily workspace budget exhausted: {0}")]
    BudgetExhausted(String),

    /// Operation was cancelled by the user.
    #[error("operation cancelled by user")]
    Cancelled,

    /// The requested functionality is unsupported in the current mode.
    #[error("unsupported operation: {0}")]
    Unsupported(String),

    /// The requested functionality has not been implemented yet.
    #[error("not implemented: {0}")]
    NotImplemented(String),

    /// The user's home directory could not be resolved.
    #[error("home directory not found")]
    HomeDirectoryNotFound,

    /// An I/O error occurred.
    #[error(transparent)]
    Io(#[from] io::Error),

    /// A config crate error occurred.
    #[error(transparent)]
    Config(#[from] config::ConfigError),

    /// A JSON serialization error occurred.
    #[error(transparent)]
    SerdeJson(#[from] serde_json::Error),

    /// A UUID parsing error occurred.
    #[error(transparent)]
    Uuid(#[from] uuid::Error),
}

/// Classification of tool execution failures for retry and recovery decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolFailureClass {
    /// Transient infrastructure error — retry in place with backoff.
    Retryable {
        /// Human-readable explanation safe to surface to the brain.
        reason: String,
        /// Suggested delay before the next retry attempt.
        backoff_hint: Duration,
    },
    /// Sandbox is dead or unreachable — destroy and re-provision before retrying.
    ReProvision {
        /// Human-readable explanation safe to surface to the brain.
        reason: String,
    },
    /// Permanent failure — do not retry automatically.
    Fatal {
        /// Human-readable explanation safe to surface to the brain.
        reason: String,
    },
}

impl ToolFailureClass {
    /// Returns the stable telemetry label for this failure class.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Retryable { .. } => "retryable",
            Self::ReProvision { .. } => "reprovision",
            Self::Fatal { .. } => "fatal",
        }
    }

    /// Returns the human-readable reason carried by this classification.
    pub fn reason(&self) -> &str {
        match self {
            Self::Retryable { reason, .. }
            | Self::ReProvision { reason }
            | Self::Fatal { reason } => reason,
        }
    }
}

impl MoaError {
    /// Whether this error should terminate the session (`true`) or is
    /// recoverable (the orchestrator pauses the session so the user can
    /// resume, retry, or intervene). Fatal classes are typically
    /// configuration, auth/permission, storage, and anything that
    /// indicates the process can't meaningfully continue.
    ///
    /// Recoverable classes are provider quirks, single-payload
    /// deserialization failures, exhausted rate-limit retries, and
    /// transient stream hiccups.
    pub fn is_fatal(&self) -> bool {
        match self {
            // Configuration, identity, and infrastructure — can't be
            // "retried" without user action.
            Self::ConfigError(_)
            | Self::StorageError(_)
            | Self::MissingEnvironmentVariable(_)
            | Self::HomeDirectoryNotFound
            | Self::PermissionDenied(_)
            | Self::BudgetExhausted(_)
            | Self::Unsupported(_)
            | Self::NotImplemented(_)
            | Self::Io(_)
            | Self::Config(_) => true,
            // Single-payload/event-shaped problems — resumable.
            Self::ProviderQuirk(_)
            | Self::ValidationError(_)
            | Self::SerializationError(_)
            | Self::RateLimited { .. }
            | Self::StreamError(_)
            | Self::HttpStatus { .. }
            | Self::ProviderError(_)
            | Self::ToolError(_)
            | Self::SerdeJson(_)
            | Self::Uuid(_) => false,
            // Session/blob lookups and cancellation are neither fatal
            // in the "kill the app" sense nor recoverable within the
            // same session — treat them as fatal so the supervisor
            // doesn't leave a broken session in `Paused`.
            Self::SessionNotFound(_) | Self::WorkspaceNotFound(_) | Self::BlobNotFound(_) => true,
            Self::Cancelled => true,
        }
    }
}

/// Classifies one tool execution error into retry, re-provision, or fatal handling.
pub fn classify_tool_error(error: &MoaError, consecutive_timeouts: u32) -> ToolFailureClass {
    match error {
        MoaError::RateLimited { message, .. } => ToolFailureClass::Retryable {
            reason: format!("tool provider rate limited the request: {message}"),
            backoff_hint: Duration::from_secs(2),
        },
        MoaError::HttpStatus {
            status,
            retry_after,
            message,
        } => match status {
            400 => ToolFailureClass::Fatal {
                reason: format!("tool request was rejected as invalid: {message}"),
            },
            401 | 403 => ToolFailureClass::Fatal {
                reason: format!("tool provider authentication or authorization failed: {message}"),
            },
            429 => ToolFailureClass::Retryable {
                reason: format!("tool provider rate limited the request: {message}"),
                backoff_hint: retry_after.unwrap_or(Duration::from_secs(2)),
            },
            502..=504 => ToolFailureClass::Retryable {
                reason: format!(
                    "tool provider gateway is temporarily unavailable ({status}): {message}"
                ),
                backoff_hint: Duration::from_secs(1),
            },
            code if *code >= 500 => ToolFailureClass::Retryable {
                reason: format!(
                    "tool provider returned a transient server error ({code}): {message}"
                ),
                backoff_hint: Duration::from_secs(1),
            },
            _ => ToolFailureClass::Fatal {
                reason: format!("tool request failed with HTTP {status}: {message}"),
            },
        },
        MoaError::ToolError(message) => classify_message_error(message, consecutive_timeouts),
        MoaError::ProviderError(message) => classify_message_error(message, consecutive_timeouts),
        MoaError::StreamError(message) => classify_message_error(message, consecutive_timeouts),
        MoaError::ValidationError(message) => ToolFailureClass::Fatal {
            reason: format!("tool input failed validation: {message}"),
        },
        MoaError::PermissionDenied(message) => ToolFailureClass::Fatal {
            reason: message.clone(),
        },
        MoaError::ConfigError(message) => ToolFailureClass::Fatal {
            reason: format!("tool execution is misconfigured: {message}"),
        },
        MoaError::MissingEnvironmentVariable(name) => ToolFailureClass::Fatal {
            reason: format!("required environment variable is missing: {name}"),
        },
        MoaError::StorageError(message) => ToolFailureClass::Fatal {
            reason: format!("tool execution could not access storage: {message}"),
        },
        MoaError::BlobNotFound(message) => ToolFailureClass::Fatal {
            reason: format!("tool artifact was not found: {message}"),
        },
        MoaError::SessionNotFound(session_id) => ToolFailureClass::Fatal {
            reason: format!("session not found: {session_id}"),
        },
        MoaError::WorkspaceNotFound(workspace_id) => ToolFailureClass::Fatal {
            reason: format!("workspace not found: {workspace_id}"),
        },
        MoaError::ProviderQuirk(message) => ToolFailureClass::Retryable {
            reason: format!("tool provider returned a transient shape mismatch: {message}"),
            backoff_hint: Duration::from_secs(1),
        },
        MoaError::SerializationError(message) => ToolFailureClass::Fatal {
            reason: format!("tool payload could not be serialized: {message}"),
        },
        MoaError::BudgetExhausted(message) => ToolFailureClass::Fatal {
            reason: format!("tool execution budget is exhausted: {message}"),
        },
        MoaError::Cancelled => ToolFailureClass::Fatal {
            reason: "tool execution was cancelled".to_string(),
        },
        MoaError::Unsupported(message) => ToolFailureClass::Fatal {
            reason: message.clone(),
        },
        MoaError::NotImplemented(message) => ToolFailureClass::Fatal {
            reason: format!("tool behavior is not implemented: {message}"),
        },
        MoaError::HomeDirectoryNotFound => ToolFailureClass::Fatal {
            reason: "home directory could not be resolved".to_string(),
        },
        MoaError::Io(error) => ToolFailureClass::Fatal {
            reason: format!("tool execution failed with an I/O error: {error}"),
        },
        MoaError::Config(error) => ToolFailureClass::Fatal {
            reason: format!("tool configuration is invalid: {error}"),
        },
        MoaError::SerdeJson(error) => ToolFailureClass::Fatal {
            reason: format!("tool payload could not be decoded: {error}"),
        },
        MoaError::Uuid(error) => ToolFailureClass::Fatal {
            reason: format!("tool identifier is invalid: {error}"),
        },
    }
}

impl From<ToolFailureClass> for ToolOutput {
    fn from(class: ToolFailureClass) -> Self {
        let class_label = class.label().to_string();
        let (message, structured) = match class {
            ToolFailureClass::Retryable {
                backoff_hint,
                reason,
            } => (
                format!(
                    "tool execution hit a transient infrastructure failure and automatic retries were exhausted: {reason}"
                ),
                serde_json::json!({
                    "failure_class": class_label,
                    "reason": reason,
                    "backoff_ms": backoff_hint.as_millis() as u64,
                }),
            ),
            ToolFailureClass::ReProvision { reason } => (
                format!(
                    "tool sandbox became unavailable and automatic re-provisioning did not recover it: {reason}"
                ),
                serde_json::json!({
                    "failure_class": class_label,
                    "reason": reason,
                }),
            ),
            ToolFailureClass::Fatal { reason } => (
                format!("tool execution failed: {reason}"),
                serde_json::json!({
                    "failure_class": class_label,
                    "reason": reason,
                }),
            ),
        };

        ToolOutput {
            content: vec![crate::types::ToolContent::Text { text: message }],
            is_error: true,
            structured: Some(structured),
            duration: Duration::ZERO,
            truncated: false,
            original_output_tokens: None,
            artifact: None,
        }
    }
}

fn classify_message_error(message: &str, consecutive_timeouts: u32) -> ToolFailureClass {
    if let Some(class) = classify_timeout_like_message(message, consecutive_timeouts) {
        return class;
    }

    let message_lower = message.to_ascii_lowercase();
    if message_lower.contains("connection refused")
        || message_lower.contains("connection reset")
        || message_lower.contains("broken pipe")
        || message_lower.contains("socket")
        || message_lower.contains("temporarily unavailable")
    {
        return ToolFailureClass::Retryable {
            reason: message.to_string(),
            backoff_hint: Duration::from_secs(1),
        };
    }

    ToolFailureClass::Fatal {
        reason: message.to_string(),
    }
}

fn classify_timeout_like_message(
    message: &str,
    consecutive_timeouts: u32,
) -> Option<ToolFailureClass> {
    let message_lower = message.to_ascii_lowercase();
    if message_lower.contains("deadline_exceeded") {
        return Some(ToolFailureClass::Retryable {
            reason: message.to_string(),
            backoff_hint: Duration::ZERO,
        });
    }

    if !message_lower.contains("timed out") && !message_lower.contains("timeout") {
        return None;
    }

    if consecutive_timeouts >= 1 {
        return Some(ToolFailureClass::ReProvision {
            reason: format!(
                "the sandbox became unresponsive after repeated execution timeouts: {message}"
            ),
        });
    }

    let backoff_hint = if message_lower.contains("command timed out")
        || message_lower.contains("deadline exceeded")
        || message_lower.contains("deadline_exceeded")
    {
        Duration::ZERO
    } else {
        Duration::from_secs(1)
    };

    Some(ToolFailureClass::Retryable {
        reason: message.to_string(),
        backoff_hint,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{MoaError, ToolFailureClass, classify_tool_error};

    #[test]
    fn provider_quirk_is_non_fatal() {
        assert!(!MoaError::ProviderQuirk("new field".into()).is_fatal());
    }

    #[test]
    fn validation_error_is_non_fatal() {
        assert!(!MoaError::ValidationError("compatibility: ...".into()).is_fatal());
    }

    #[test]
    fn config_error_is_fatal() {
        assert!(MoaError::ConfigError("bad toml".into()).is_fatal());
    }

    #[test]
    fn classifies_rate_limit_as_retryable() {
        let class = classify_tool_error(
            &MoaError::HttpStatus {
                status: 429,
                retry_after: Some(Duration::from_secs(7)),
                message: "try again later".to_string(),
            },
            0,
        );

        assert_eq!(
            class,
            ToolFailureClass::Retryable {
                reason: "tool provider rate limited the request: try again later".to_string(),
                backoff_hint: Duration::from_secs(7),
            }
        );
    }

    #[test]
    fn classifies_unknown_tool_as_fatal() {
        let class = classify_tool_error(&MoaError::ToolError("unknown tool: nope".into()), 0);

        assert_eq!(
            class,
            ToolFailureClass::Fatal {
                reason: "unknown tool: nope".to_string(),
            }
        );
    }

    #[test]
    fn classifies_repeated_timeout_as_reprovision() {
        let class = classify_tool_error(
            &MoaError::ToolError("docker exec command timed out after 1s".into()),
            1,
        );

        assert_eq!(
            class,
            ToolFailureClass::ReProvision {
                reason: "the sandbox became unresponsive after repeated execution timeouts: docker exec command timed out after 1s".to_string(),
            }
        );
    }
}
