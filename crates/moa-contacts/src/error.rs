//! Error types for contact identity operations.

use thiserror::Error;

/// Result alias for contact operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Contact-domain error with an optional HTTP-style terminal status.
#[derive(Debug, Error)]
pub enum Error {
    /// Terminal caller-facing failure.
    #[error("{message}")]
    Terminal {
        /// Status code the transport layer should expose.
        code: u16,
        /// Caller-facing message.
        message: String,
    },

    /// SQL storage failure with operation context.
    #[error("{context}: {source}")]
    Database {
        /// Operation that failed.
        context: &'static str,
        /// SQL error source.
        #[source]
        source: sqlx::Error,
    },

    /// Session-store failure represented by the shared core error.
    #[error("session store error: {0}")]
    SessionStore(#[from] moa_core::error::MoaError),
}

impl Error {
    /// Builds a terminal caller-facing error.
    pub fn terminal(code: u16, message: impl Into<String>) -> Self {
        Self::Terminal {
            code,
            message: message.into(),
        }
    }

    /// Builds a SQL storage error.
    pub fn database(context: &'static str, source: sqlx::Error) -> Self {
        Self::Database { context, source }
    }

    /// Returns the terminal status code if this error should be exposed as one.
    #[must_use]
    pub fn terminal_code(&self) -> Option<u16> {
        match self {
            Self::Terminal { code, .. } => Some(*code),
            Self::Database { .. } | Self::SessionStore(_) => None,
        }
    }
}
