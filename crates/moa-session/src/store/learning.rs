//! Learning-log operations for the Postgres session store.

use super::*;

impl PostgresSessionStore {
    /// Appends one learning-log entry.
    pub async fn append_learning(&self, entry: &LearningEntry) -> Result<()> {
        let learning_log = self.table_name("learning_log");
        sqlx::query(&format!(
            "INSERT INTO {learning_log} \
             (id, tenant_id, workspace_id, learning_type, target_id, target_label, payload, confidence, \
              source_refs, actor, valid_from, valid_to, batch_id, version) \
             VALUES ($1, $2, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)"
        ))
        .bind(entry.id)
        .bind(entry.tenant_id.to_string())
        .bind(&entry.learning_type)
        .bind(&entry.target_id)
        .bind(entry.target_label.as_deref())
        .bind(Json(entry.payload.clone()))
        .bind(entry.confidence)
        .bind(&entry.source_refs)
        .bind(&entry.actor)
        .bind(entry.valid_from)
        .bind(entry.valid_to)
        .bind(entry.batch_id)
        .bind(entry.version)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
    }

    /// Appends one learning-log entry using the caller's open transaction.
    pub async fn append_learning_in_tx(
        &self,
        conn: &mut sqlx::PgConnection,
        entry: &LearningEntry,
    ) -> Result<()> {
        let learning_log = self.table_name("learning_log");
        sqlx::query(&format!(
            "INSERT INTO {learning_log} \
             (id, tenant_id, workspace_id, learning_type, target_id, target_label, payload, confidence, \
              source_refs, actor, valid_from, valid_to, batch_id, version) \
             VALUES ($1, $2, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)"
        ))
        .bind(entry.id)
        .bind(entry.tenant_id.to_string())
        .bind(&entry.learning_type)
        .bind(&entry.target_id)
        .bind(entry.target_label.as_deref())
        .bind(Json(entry.payload.clone()))
        .bind(entry.confidence)
        .bind(&entry.source_refs)
        .bind(&entry.actor)
        .bind(entry.valid_from)
        .bind(entry.valid_to)
        .bind(entry.batch_id)
        .bind(entry.version)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
    }

    /// Lists current learning-log entries for a tenant.
    pub async fn list_learnings(
        &self,
        tenant_id: &str,
        learning_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<LearningEntry>> {
        let learning_log = self.table_name("learning_log");
        let mut query = QueryBuilder::<Postgres>::new(format!(
            "SELECT {LEARNING_ENTRY_COLUMNS} FROM {learning_log} \
             WHERE tenant_id = "
        ));
        query.push_bind(tenant_id);
        query.push(" AND valid_to IS NULL");
        if let Some(learning_type) = learning_type {
            query.push(" AND learning_type = ");
            query.push_bind(learning_type);
        }
        query.push(" ORDER BY recorded_at DESC, valid_from DESC");
        query.push(" LIMIT ");
        query.push_bind(limit as i64);

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        rows.iter().map(learning_entry_from_row).collect()
    }

    /// Invalidates every current learning-log entry in a batch.
    pub async fn rollback_batch(&self, batch_id: Uuid) -> Result<u64> {
        let learning_log = self.table_name("learning_log");
        let affected = sqlx::query(&format!(
            "UPDATE {learning_log} SET valid_to = NOW() \
             WHERE batch_id = $1 AND valid_to IS NULL"
        ))
        .bind(batch_id)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        Ok(affected)
    }
}
