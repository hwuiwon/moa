//! Shared workflow error conversion for Restate handlers.

use std::borrow::Cow;

use moa_authz::{AuthzCheckError, AuthzError};
use moa_core::error::{FailureProvenance, MoaError};
use restate_sdk::prelude::*;

/// Restate-specific disposition for a typed application failure.
///
/// This classifier is intentionally owned by the orchestrator and never uses
/// [`MoaError::is_fatal`], whose meaning is session lifecycle severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestateErrorClass {
    /// Restate may invoke the failed handler again.
    Retryable,
    /// Repeating the same request is unsafe or cannot make progress.
    Terminal { status: Option<u16> },
}

/// Classifies a shared MOA error at the Restate boundary.
#[must_use]
pub(crate) fn classify_moa_error(error: &MoaError) -> RestateErrorClass {
    if error.failure_provenance() == FailureProvenance::Transient {
        return RestateErrorClass::Retryable;
    }

    let status = match error {
        MoaError::ValidationError(_)
        | MoaError::SerializationError(_)
        | MoaError::SerdeJson(_)
        | MoaError::Uuid(_) => Some(400),
        MoaError::PermissionDenied(_) => Some(403),
        MoaError::SessionNotFound(_)
        | MoaError::BlobNotFound(_)
        | MoaError::SessionAttachmentNotFound(_)
        | MoaError::SessionAttachmentObjectNotFound(_) => Some(404),
        MoaError::SessionAttachmentSlotConflict(_)
        | MoaError::ExternalEffectUnknownOutcome { .. } => Some(409),
        MoaError::Unsupported(_) => Some(501),
        MoaError::HttpStatus { status, .. } if (400..500).contains(status) => Some(*status),
        MoaError::ProviderError(_)
        | MoaError::MissingEnvironmentVariable(_)
        | MoaError::ConfigError(_)
        | MoaError::StorageError(_)
        | MoaError::StorageUnavailable(_)
        | MoaError::ToolError(_)
        | MoaError::ProviderQuirk(_)
        | MoaError::HttpStatus { .. }
        | MoaError::StreamError(_)
        | MoaError::BudgetExhausted(_)
        | MoaError::Cancelled
        | MoaError::HomeDirectoryNotFound
        | MoaError::Io(_)
        | MoaError::ProviderTransport(_)
        | MoaError::ProviderTimeout(_)
        | MoaError::RateLimited { .. } => None,
    };
    RestateErrorClass::Terminal { status }
}

/// Classifies a required authorization decision at the Restate boundary.
#[must_use]
pub(crate) fn classify_authz_check_error(error: &AuthzCheckError) -> RestateErrorClass {
    match error {
        AuthzCheckError::Forbidden { .. } => RestateErrorClass::Terminal { status: Some(403) },
        AuthzCheckError::Engine(error) => classify_authz_error(error),
    }
}

/// Classifies an authorization-engine error at the Restate boundary.
#[must_use]
pub(crate) fn classify_authz_error(error: &AuthzError) -> RestateErrorClass {
    match error.failure_provenance() {
        FailureProvenance::Transient => RestateErrorClass::Retryable,
        FailureProvenance::Permanent => RestateErrorClass::Terminal { status: Some(503) },
    }
}

/// Classifies a Postgres error without parsing its display text.
#[must_use]
pub(crate) fn classify_sqlx_error(error: &sqlx::Error) -> RestateErrorClass {
    let transient = match error {
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::WorkerCrashed => true,
        sqlx::Error::Io(error) => transient_io(error.kind()),
        sqlx::Error::Database(error) => transient_sqlstate(error.code()),
        _ => false,
    };

    if transient {
        RestateErrorClass::Retryable
    } else {
        RestateErrorClass::Terminal { status: None }
    }
}

/// Converts a [`MoaError`] into a Restate handler error.
pub(crate) fn moa_error_to_handler_error(error: MoaError) -> HandlerError {
    error_to_handler_error(classify_moa_error(&error), error)
}

/// Converts a [`MoaError`] into a Restate handler error with an HTTP status when known.
pub(crate) fn moa_error_to_status_handler_error(error: MoaError) -> HandlerError {
    moa_error_to_handler_error(error)
}

/// Converts a typed authorization check error into a Restate handler error.
pub(crate) fn authz_check_error_to_handler_error(error: AuthzCheckError) -> HandlerError {
    error_to_handler_error(classify_authz_check_error(&error), error)
}

/// Converts a typed authorization-engine error into a Restate handler error.
pub(crate) fn authz_error_to_handler_error(error: AuthzError) -> HandlerError {
    error_to_handler_error(classify_authz_error(&error), error)
}

/// Converts a typed SQLx error into a Restate handler error.
pub(crate) fn sqlx_error_to_handler_error(error: sqlx::Error) -> HandlerError {
    error_to_handler_error(classify_sqlx_error(&error), error)
}

fn error_to_handler_error(
    class: RestateErrorClass,
    error: impl std::error::Error + Send + Sync + 'static,
) -> HandlerError {
    match class {
        RestateErrorClass::Retryable => HandlerError::from(error),
        RestateErrorClass::Terminal {
            status: Some(status),
        } => TerminalError::new_with_code(status, error.to_string()).into(),
        RestateErrorClass::Terminal { status: None } => {
            TerminalError::new(error.to_string()).into()
        }
    }
}

fn transient_io(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::NetworkDown
            | std::io::ErrorKind::NetworkUnreachable
            | std::io::ErrorKind::HostUnreachable
    )
}

fn transient_sqlstate(code: Option<Cow<'_, str>>) -> bool {
    let Some(code) = code else {
        return false;
    };
    code.starts_with("08")
        || code.starts_with("40")
        || matches!(
            code.as_ref(),
            "53P01" | "53P02" | "53P03" | "55P03" | "57P01" | "57P02" | "57P03"
        )
}

/// Builds a terminal `400` handler error from a message.
pub(crate) fn bad_request(message: impl Into<String>) -> HandlerError {
    TerminalError::new_with_code(400, message.into()).into()
}

/// Extracts the display message from a Restate handler error.
pub(crate) fn handler_error_message(error: &HandlerError) -> String {
    let error_ref = <HandlerError as AsRef<dyn std::error::Error + Send + Sync>>::as_ref(error);
    error_ref.to_string()
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use moa_authz_schema::{ObjectType, Relation};
    use moa_core::types::identifiers::{SessionAttachmentId, SessionId};
    use uuid::Uuid;

    use super::*;

    fn terminal(status: Option<u16>) -> RestateErrorClass {
        RestateErrorClass::Terminal { status }
    }

    #[test]
    fn every_moa_error_family_has_an_explicit_restate_class() {
        // Pins: session-fatal severity cannot accidentally become Restate retryability.
        let session_id = SessionId(Uuid::nil());
        let attachment_id = SessionAttachmentId(Uuid::nil());
        let cases = [
            (MoaError::SessionNotFound(session_id), terminal(Some(404))),
            (MoaError::ProviderError("shape".into()), terminal(None)),
            (
                MoaError::ProviderTransport("connect".into()),
                RestateErrorClass::Retryable,
            ),
            (
                MoaError::ProviderTimeout("deadline".into()),
                RestateErrorClass::Retryable,
            ),
            (
                MoaError::MissingEnvironmentVariable("KEY".into()),
                terminal(None),
            ),
            (MoaError::ConfigError("bad".into()), terminal(None)),
            (MoaError::StorageError("db".into()), terminal(None)),
            (
                MoaError::StorageUnavailable("pool timeout".into()),
                RestateErrorClass::Retryable,
            ),
            (MoaError::BlobNotFound("blob".into()), terminal(Some(404))),
            (
                MoaError::SessionAttachmentNotFound(attachment_id),
                terminal(Some(404)),
            ),
            (
                MoaError::SessionAttachmentObjectNotFound("object".into()),
                terminal(Some(404)),
            ),
            (
                MoaError::SessionAttachmentSlotConflict("slot".into()),
                terminal(Some(409)),
            ),
            (
                MoaError::ExternalEffectUnknownOutcome {
                    operation_id: "effect-1".into(),
                },
                terminal(Some(409)),
            ),
            (MoaError::ToolError("tool".into()), terminal(None)),
            (
                MoaError::ValidationError("input".into()),
                terminal(Some(400)),
            ),
            (MoaError::ProviderQuirk("protocol".into()), terminal(None)),
            (
                MoaError::SerializationError("wire".into()),
                terminal(Some(400)),
            ),
            (
                MoaError::RateLimited {
                    retries: 3,
                    message: "slow".into(),
                },
                RestateErrorClass::Retryable,
            ),
            (MoaError::StreamError("shape".into()), terminal(None)),
            (
                MoaError::PermissionDenied("deny".into()),
                terminal(Some(403)),
            ),
            (MoaError::BudgetExhausted("budget".into()), terminal(None)),
            (MoaError::Cancelled, terminal(None)),
            (MoaError::Unsupported("mode".into()), terminal(Some(501))),
            (MoaError::HomeDirectoryNotFound, terminal(None)),
            (
                MoaError::Io(io::Error::new(io::ErrorKind::InvalidInput, "bad")),
                terminal(None),
            ),
            (
                MoaError::Io(io::Error::new(io::ErrorKind::ConnectionReset, "reset")),
                RestateErrorClass::Retryable,
            ),
            (
                MoaError::SerdeJson(
                    serde_json::from_str::<serde_json::Value>("{").expect_err("invalid json"),
                ),
                terminal(Some(400)),
            ),
            (
                MoaError::Uuid(Uuid::parse_str("bad").expect_err("invalid uuid")),
                terminal(Some(400)),
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(
                classify_moa_error(&error),
                expected,
                "unexpected class for {error:?}"
            );
        }
    }

    #[test]
    fn http_status_families_are_bounded() {
        // Pins: definitive 4xx responses terminate, while timeout, rate limit, and 5xx recover.
        for status in [400, 401, 403, 404, 409, 422] {
            let error = MoaError::HttpStatus {
                status,
                retry_after: None,
                message: "client".into(),
            };
            assert_eq!(classify_moa_error(&error), terminal(Some(status)));
        }
        for status in [408, 425, 429, 500, 502, 503, 504, 599] {
            let error = MoaError::HttpStatus {
                status,
                retry_after: Some(Duration::ZERO),
                message: "transient".into(),
            };
            assert_eq!(classify_moa_error(&error), RestateErrorClass::Retryable);
        }
    }

    #[test]
    fn forbidden_is_terminal() {
        // Pins: a definitive deny can never be replayed into a later allow decision.
        let error = AuthzCheckError::Forbidden {
            subject: "operator:one".into(),
            object_type: ObjectType::Tenant,
            object_id: Uuid::nil().to_string(),
            relation: Relation::Participant,
        };
        assert_eq!(classify_authz_check_error(&error), terminal(Some(403)));
    }

    #[test]
    fn authz_engine_errors_follow_typed_provenance() {
        // Pins: OpenFGA availability failures can recover without ever making a
        // definitive forbidden decision retryable.
        let transient = AuthzCheckError::Engine(AuthzError::Database(sqlx::Error::PoolTimedOut));
        let permanent = AuthzCheckError::Engine(AuthzError::Config("missing store".into()));
        let transient_audit = AuthzCheckError::Engine(AuthzError::Audit(
            moa_ocsf::EmitError::Database(sqlx::Error::PoolTimedOut),
        ));
        let permanent_audit = AuthzCheckError::Engine(AuthzError::Audit(
            moa_ocsf::EmitError::InvalidInput("unstable actor".into()),
        ));
        assert_eq!(
            classify_authz_check_error(&transient),
            RestateErrorClass::Retryable
        );
        assert_eq!(classify_authz_check_error(&permanent), terminal(Some(503)));
        assert_eq!(
            classify_authz_check_error(&transient_audit),
            RestateErrorClass::Retryable
        );
        assert_eq!(
            classify_authz_check_error(&permanent_audit),
            terminal(Some(503))
        );
    }

    #[test]
    fn sqlx_transient_and_permanent_families_are_distinct() {
        // Pins: pool/connectivity pressure recovers, malformed rows and requests do not loop.
        assert_eq!(
            classify_sqlx_error(&sqlx::Error::PoolTimedOut),
            RestateErrorClass::Retryable
        );
        assert_eq!(
            classify_sqlx_error(&sqlx::Error::PoolClosed),
            RestateErrorClass::Retryable
        );
        assert_eq!(
            classify_sqlx_error(&sqlx::Error::RowNotFound),
            terminal(None)
        );
        assert_eq!(
            classify_sqlx_error(&sqlx::Error::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "timeout"
            ))),
            RestateErrorClass::Retryable
        );
        assert_eq!(
            classify_sqlx_error(&sqlx::Error::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad"
            ))),
            terminal(None)
        );
    }
}
