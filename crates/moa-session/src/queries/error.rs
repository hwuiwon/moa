//! Error mapping helpers for session queries.

use super::*;

pub(crate) fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}
