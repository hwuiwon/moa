//! Shared Postgres row decoding for contact repository modules.

use sqlx::Row as _;

use crate::{Error, Result};

pub(super) trait RowExt {
    fn col<'r, T>(&'r self, name: &'static str) -> Result<T>
    where
        T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>;
}

impl RowExt for sqlx::postgres::PgRow {
    fn col<'r, T>(&'r self, name: &'static str) -> Result<T>
    where
        T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
    {
        self.try_get(name)
            .map_err(|error| Error::database(name, error))
    }
}
