//! Error types for authorization client and outbox operations.

use moa_core::error::FailureProvenance;
use thiserror::Error;

/// Failure phase for an OpenFGA HTTP transport error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthzTransportKind {
    /// The request exceeded its configured deadline.
    Timeout,
    /// The connection or response body failed in transit.
    Network,
    /// The request could not be constructed or decoded safely.
    Protocol,
}

/// Errors returned by the OpenFGA client and transactional outbox.
#[derive(Debug, Error)]
pub enum AuthzError {
    /// OpenFGA returned a non-success HTTP status.
    #[error("FGA HTTP error: {status}: {body}")]
    HttpError {
        /// HTTP status code.
        status: u16,
        /// Response body returned by OpenFGA.
        body: String,
    },

    /// The HTTP client failed before receiving a valid OpenFGA response.
    #[error("FGA transport error ({kind:?}): {source}")]
    Transport {
        /// Typed transport phase used by retry owners.
        kind: AuthzTransportKind,
        /// Original HTTP client failure.
        #[source]
        source: reqwest::Error,
    },

    /// Postgres returned an error while reading or updating the outbox.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// JSON serialization or deserialization failed.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// Required runtime configuration was missing or invalid.
    #[error("config error: {0}")]
    Config(String),

    /// OpenFGA returned a response that could not be interpreted safely.
    #[error("FGA returned ambiguous response: {0}")]
    Ambiguous(String),

    /// Security audit emission failed.
    #[error("audit error: {0}")]
    Audit(#[source] moa_ocsf::EmitError),
}

impl AuthzError {
    /// Returns the typed provenance of an authorization-engine failure.
    #[must_use]
    pub fn failure_provenance(&self) -> FailureProvenance {
        match self {
            Self::HttpError { status, .. } if matches!(*status, 408 | 425 | 429 | 500..=599) => {
                FailureProvenance::Transient
            }
            Self::Transport {
                kind: AuthzTransportKind::Timeout | AuthzTransportKind::Network,
                ..
            }
            | Self::Database(
                sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::WorkerCrashed,
            ) => FailureProvenance::Transient,
            Self::Database(sqlx::Error::Io(error)) if transient_io(error.kind()) => {
                FailureProvenance::Transient
            }
            Self::Database(sqlx::Error::Database(error)) if transient_sqlstate(error.code()) => {
                FailureProvenance::Transient
            }
            Self::Audit(moa_ocsf::EmitError::Database(error))
                if sqlx_failure_provenance(error) == FailureProvenance::Transient =>
            {
                FailureProvenance::Transient
            }
            Self::Audit(moa_ocsf::EmitError::Signing(
                moa_ocsf::signing::SigningError::Database(error),
            )) if sqlx_failure_provenance(error) == FailureProvenance::Transient => {
                FailureProvenance::Transient
            }
            Self::HttpError { .. }
            | Self::Transport { .. }
            | Self::Database(_)
            | Self::Serde(_)
            | Self::Config(_)
            | Self::Ambiguous(_)
            | Self::Audit(_) => FailureProvenance::Permanent,
        }
    }
}

fn sqlx_failure_provenance(error: &sqlx::Error) -> FailureProvenance {
    match error {
        sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::WorkerCrashed => {
            FailureProvenance::Transient
        }
        sqlx::Error::Io(error) if transient_io(error.kind()) => FailureProvenance::Transient,
        sqlx::Error::Database(error) if transient_sqlstate(error.code()) => {
            FailureProvenance::Transient
        }
        _ => FailureProvenance::Permanent,
    }
}

impl From<reqwest::Error> for AuthzError {
    fn from(source: reqwest::Error) -> Self {
        let kind = if source.is_timeout() {
            AuthzTransportKind::Timeout
        } else if source.is_connect()
            || source.is_body()
            || (source.is_request() && !source.is_builder())
        {
            AuthzTransportKind::Network
        } else {
            AuthzTransportKind::Protocol
        };
        Self::Transport { kind, source }
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

fn transient_sqlstate(code: Option<std::borrow::Cow<'_, str>>) -> bool {
    let Some(code) = code else {
        return false;
    };
    code.starts_with("08")
        || code.starts_with("40")
        || code == "53P01"
        || code == "53P02"
        || code == "53P03"
        || code == "55P03"
        || code == "57P01"
        || code == "57P02"
        || code == "57P03"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protocol_error() -> reqwest::Error {
        reqwest::Client::new()
            .get("://invalid")
            .build()
            .expect_err("invalid URL must fail request construction")
    }

    #[test]
    fn every_authz_error_family_has_typed_provenance() {
        // Pins: definitive denies/configuration/protocol failures never share the
        // retry path with OpenFGA/network/database availability failures.
        let transient = [
            AuthzError::HttpError {
                status: 503,
                body: "unavailable".into(),
            },
            AuthzError::HttpError {
                status: 429,
                body: "limited".into(),
            },
            AuthzError::Transport {
                kind: AuthzTransportKind::Timeout,
                source: protocol_error(),
            },
            AuthzError::Transport {
                kind: AuthzTransportKind::Network,
                source: protocol_error(),
            },
            AuthzError::Database(sqlx::Error::PoolTimedOut),
            AuthzError::Audit(moa_ocsf::EmitError::Database(sqlx::Error::PoolTimedOut)),
        ];
        for error in transient {
            assert_eq!(
                error.failure_provenance(),
                FailureProvenance::Transient,
                "expected transient provenance for {error:?}"
            );
        }

        let permanent = [
            AuthzError::HttpError {
                status: 403,
                body: "forbidden".into(),
            },
            AuthzError::Transport {
                kind: AuthzTransportKind::Protocol,
                source: protocol_error(),
            },
            AuthzError::Database(sqlx::Error::RowNotFound),
            AuthzError::Serde(
                serde_json::from_str::<serde_json::Value>("{").expect_err("invalid JSON must fail"),
            ),
            AuthzError::Config("missing store".into()),
            AuthzError::Ambiguous("missing allowed".into()),
            AuthzError::Audit(moa_ocsf::EmitError::InvalidInput("bad actor".into())),
        ];
        for error in permanent {
            assert_eq!(
                error.failure_provenance(),
                FailureProvenance::Permanent,
                "expected permanent provenance for {error:?}"
            );
        }
    }
}
