//! Shared pgvector embedding row decoding helpers.

use chrono::{DateTime, Utc};
use moa_core::types::security::SensitivityClass;
use pgvector::HalfVector;
use sqlx::Row;
use uuid::Uuid;

use crate::{Error, Result, VectorItem, VectorQuery, validate_dimension};

#[derive(Debug, Clone)]
pub(crate) struct EmbeddingRow {
    pub(crate) uid: Uuid,
    user_id: Option<String>,
    label: String,
    pii_class: SensitivityClass,
    embedding: HalfVector,
    embedding_model: String,
    embedding_model_version: i32,
    search_text: Option<String>,
    valid_to: Option<DateTime<Utc>>,
}

impl EmbeddingRow {
    pub(crate) fn from_row(row: sqlx::postgres::PgRow) -> Result<Self> {
        let pii_class: String = row.try_get("pii_class")?;
        let pii_class = pii_class
            .parse()
            .map_err(|_| Error::InvalidSensitivityClass(pii_class))?;
        Ok(Self {
            uid: row.try_get("uid")?,
            user_id: row.try_get("user_id")?,
            label: row.try_get("label")?,
            pii_class,
            embedding: row.try_get("embedding")?,
            embedding_model: row.try_get("embedding_model")?,
            embedding_model_version: row.try_get("embedding_model_version")?,
            search_text: optional_column(&row, "search_text")?,
            valid_to: row.try_get("valid_to")?,
        })
    }

    pub(crate) fn to_vector_item(&self) -> Result<VectorItem> {
        let embedding = self.embedding_f32();
        validate_dimension(&embedding)?;
        Ok(VectorItem {
            uid: self.uid,
            user_id: self.user_id.clone(),
            label: self.label.clone(),
            pii_class: self.pii_class,
            embedding,
            embedding_model: self.embedding_model.clone(),
            embedding_model_version: self.embedding_model_version,
            search_text: self.search_text.clone(),
            valid_to: self.valid_to,
        })
    }

    pub(crate) fn to_vector_query(&self, k: usize) -> Result<VectorQuery> {
        let embedding = self.embedding_f32();
        validate_dimension(&embedding)?;
        Ok(VectorQuery {
            embedding,
            k,
            label_filter: Some(vec![self.label.clone()]),
            max_pii_class: SensitivityClass::Restricted,
            include_global: false,
            as_of: None,
        })
    }

    fn embedding_f32(&self) -> Vec<f32> {
        self.embedding
            .to_vec()
            .into_iter()
            .map(|value| value.to_f32())
            .collect()
    }
}

fn optional_column<T>(row: &sqlx::postgres::PgRow, name: &'static str) -> Result<Option<T>>
where
    T: for<'a> sqlx::Decode<'a, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    match row.try_get(name) {
        Ok(value) => Ok(value),
        Err(sqlx::Error::ColumnNotFound(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
