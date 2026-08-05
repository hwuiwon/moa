//! Error mapping helpers for session queries.

use std::borrow::Cow;

use moa_core::error::FailureProvenance;

use super::*;

pub(crate) fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    let provenance = sqlx_failure_provenance(&error);
    let message = error.to_string();
    match provenance {
        FailureProvenance::Transient => MoaError::StorageUnavailable(message),
        FailureProvenance::Permanent => MoaError::StorageError(message),
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

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn transient_sqlx_failures_keep_retryable_provenance() {
        // Pins: real session query/store adapters must not erase pool and
        // connectivity failures into permanent string-only storage errors.
        for error in [
            sqlx::Error::PoolTimedOut,
            sqlx::Error::PoolClosed,
            sqlx::Error::Io(io::Error::new(io::ErrorKind::ConnectionReset, "reset")),
        ] {
            let mapped = map_sqlx_error(error);
            assert!(matches!(&mapped, MoaError::StorageUnavailable(_)));
            assert_eq!(mapped.failure_provenance(), FailureProvenance::Transient);
        }
    }

    #[test]
    fn permanent_sqlx_failures_remain_terminal_storage_errors() {
        // Pins: malformed queries and row-shape mistakes must not retry forever.
        for error in [
            sqlx::Error::RowNotFound,
            sqlx::Error::ColumnNotFound("missing".to_string()),
            sqlx::Error::Io(io::Error::new(io::ErrorKind::InvalidData, "bad row")),
        ] {
            let mapped = map_sqlx_error(error);
            assert!(matches!(&mapped, MoaError::StorageError(_)));
            assert_eq!(mapped.failure_provenance(), FailureProvenance::Permanent);
        }
    }

    #[test]
    fn postgres_retry_sqlstates_are_bounded() {
        // Pins: connection, transaction rollback, resource pressure, lock,
        // and server-shutdown SQLSTATEs recover; data violations do not.
        for code in [
            "08006", "40001", "40P01", "53P01", "53P02", "53P03", "55P03", "57P01", "57P02",
            "57P03",
        ] {
            assert!(transient_sqlstate(Some(Cow::Borrowed(code))), "{code}");
        }
        for code in ["22000", "23503", "23505", "42601"] {
            assert!(!transient_sqlstate(Some(Cow::Borrowed(code))), "{code}");
        }
        assert!(!transient_sqlstate(None));
    }
}
