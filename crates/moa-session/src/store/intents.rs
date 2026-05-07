//! Intent taxonomy, catalog, and learning-log operations.

use super::*;

impl PostgresSessionStore {
    /// Creates or updates one tenant intent by id.
    pub async fn create_intent(&self, intent: &TenantIntent) -> Result<()> {
        let tenant_intents = self.table_name("tenant_intents");
        let embedding = intent.embedding.as_ref().map(|value| vector_literal(value));
        sqlx::query(&format!(
            "INSERT INTO {tenant_intents} \
             (id, tenant_id, workspace_id, label, description, status, source, catalog_ref, example_queries, embedding, segment_count, resolution_rate) \
             VALUES ($1, $2, $2, $3, $4, $5, $6, $7, $8, $9::vector, $10, $11) \
             ON CONFLICT (id) DO UPDATE SET \
                 tenant_id = EXCLUDED.tenant_id, \
                 workspace_id = EXCLUDED.workspace_id, \
                 label = EXCLUDED.label, \
                 description = EXCLUDED.description, \
                 status = EXCLUDED.status, \
                 source = EXCLUDED.source, \
                 catalog_ref = EXCLUDED.catalog_ref, \
                 example_queries = EXCLUDED.example_queries, \
                 embedding = EXCLUDED.embedding, \
                 segment_count = EXCLUDED.segment_count, \
                 resolution_rate = EXCLUDED.resolution_rate, \
                 updated_at = NOW(), \
                 deprecated_at = CASE WHEN EXCLUDED.status = 'deprecated' THEN NOW() ELSE NULL END"
        ))
        .bind(intent.id)
        .bind(&intent.tenant_id)
        .bind(&intent.label)
        .bind(intent.description.as_deref())
        .bind(intent_status_to_db(intent.status))
        .bind(intent_source_to_db(intent.source))
        .bind(intent.catalog_ref)
        .bind(&intent.example_queries)
        .bind(embedding.as_deref())
        .bind(intent.segment_count as i32)
        .bind(intent.resolution_rate)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
    }

    /// Loads a tenant intent by id.
    pub async fn get_intent(&self, id: Uuid) -> Result<TenantIntent> {
        let tenant_intents = self.table_name("tenant_intents");
        let row = sqlx::query(&format!(
            "SELECT {TENANT_INTENT_COLUMNS} FROM {tenant_intents} WHERE id = $1 LIMIT 1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .ok_or_else(|| MoaError::StorageError(format!("tenant intent `{id}` was not found")))?;
        tenant_intent_from_row(&row)
    }

    /// Updates a tenant intent lifecycle status.
    pub async fn update_intent_status(&self, id: Uuid, status: IntentStatus) -> Result<()> {
        let tenant_intents = self.table_name("tenant_intents");
        let affected = sqlx::query(&format!(
            "UPDATE {tenant_intents} SET \
                 status = $1, \
                 updated_at = NOW(), \
                 deprecated_at = CASE WHEN $1 = 'deprecated' THEN NOW() ELSE NULL END \
             WHERE id = $2"
        ))
        .bind(intent_status_to_db(status))
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        if affected == 0 {
            return Err(MoaError::StorageError(format!(
                "tenant intent `{id}` was not found"
            )));
        }
        Ok(())
    }

    /// Renames a tenant intent and updates its description when provided.
    pub async fn rename_intent(
        &self,
        id: Uuid,
        new_label: &str,
        description: Option<&str>,
    ) -> Result<TenantIntent> {
        let tenant_intents = self.table_name("tenant_intents");
        let row = sqlx::query(&format!(
            "UPDATE {tenant_intents} SET \
                 label = $1, \
                 description = COALESCE($2, description), \
                 updated_at = NOW() \
             WHERE id = $3 \
             RETURNING {TENANT_INTENT_COLUMNS}"
        ))
        .bind(new_label)
        .bind(description)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .ok_or_else(|| MoaError::StorageError(format!("tenant intent `{id}` was not found")))?;
        tenant_intent_from_row(&row)
    }

    /// Deletes a proposed tenant intent after admin rejection.
    pub async fn reject_intent(&self, id: Uuid) -> Result<u64> {
        let tenant_intents = self.table_name("tenant_intents");
        let affected = sqlx::query(&format!(
            "DELETE FROM {tenant_intents} WHERE id = $1 AND status = 'proposed'"
        ))
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        Ok(affected)
    }

    /// Lists tenant intents, optionally filtered by lifecycle status.
    pub async fn list_intents(
        &self,
        tenant_id: &str,
        status: Option<IntentStatus>,
    ) -> Result<Vec<TenantIntent>> {
        let tenant_intents = self.table_name("tenant_intents");
        let mut query = QueryBuilder::<Postgres>::new(format!(
            "SELECT {TENANT_INTENT_COLUMNS} FROM {tenant_intents} WHERE tenant_id = "
        ));
        query.push_bind(tenant_id);
        if let Some(status) = status {
            query.push(" AND status = ");
            query.push_bind(intent_status_to_db(status));
        }
        query.push(" ORDER BY status ASC, updated_at DESC, label ASC");

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        rows.iter().map(tenant_intent_from_row).collect()
    }

    /// Assigns a tenant intent to a task segment.
    pub async fn classify_segment(
        &self,
        segment_id: Uuid,
        intent_id: Uuid,
        confidence: f64,
    ) -> Result<()> {
        let tenant_intents = self.table_name("tenant_intents");
        let task_segments = self.table_name("task_segments");
        let intent = self.get_intent(intent_id).await?;
        let affected = sqlx::query(&format!(
            "UPDATE {task_segments} SET intent_label = $1, intent_confidence = $2 WHERE id = $3"
        ))
        .bind(&intent.label)
        .bind(confidence)
        .bind(segment_id)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();

        if affected == 0 {
            return Err(MoaError::StorageError(format!(
                "task segment `{segment_id}` was not found"
            )));
        }

        sqlx::query(&format!(
            "UPDATE {tenant_intents} i SET \
                 segment_count = counts.segment_count, \
                 resolution_rate = counts.resolution_rate, \
                 updated_at = NOW() \
             FROM ( \
                 SELECT \
                     COUNT(*)::INT AS segment_count, \
                     AVG(CASE WHEN resolution = 'resolved' THEN 1.0 \
                              WHEN resolution = 'partial' THEN 0.5 \
                              WHEN resolution IS NULL THEN NULL \
                              ELSE 0.0 END)::NUMERIC(4,3) AS resolution_rate \
                 FROM {task_segments} \
                 WHERE tenant_id = $1 AND intent_label = $2 \
             ) counts \
             WHERE i.id = $3"
        ))
        .bind(&intent.tenant_id)
        .bind(&intent.label)
        .bind(intent_id)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
    }

    /// Returns nearest active tenant intents for the provided embedding.
    pub async fn get_intent_by_embedding(
        &self,
        tenant_id: &str,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(TenantIntent, f64)>> {
        if embedding.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let tenant_intents = self.table_name("tenant_intents");
        let rows = sqlx::query(&format!(
            "SELECT {TENANT_INTENT_COLUMNS}, (embedding <=> $1::vector)::DOUBLE PRECISION AS distance \
             FROM {tenant_intents} \
             WHERE tenant_id = $2 AND status = 'active' AND embedding IS NOT NULL \
             ORDER BY distance ASC, updated_at DESC \
             LIMIT $3"
        ))
        .bind(vector_literal(embedding))
        .bind(tenant_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        rows.iter()
            .map(|row| {
                Ok((
                    tenant_intent_from_row(row)?,
                    row.try_get::<f64, _>("distance").map_err(map_sqlx_error)?,
                ))
            })
            .collect()
    }

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
        .bind(&entry.tenant_id)
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

    /// Creates or updates a global catalog intent.
    pub async fn upsert_catalog_intent(&self, intent: &CatalogIntent) -> Result<()> {
        let global_intent_catalog = self.table_name("global_intent_catalog");
        let embedding = intent.embedding.as_ref().map(|value| vector_literal(value));
        sqlx::query(&format!(
            "INSERT INTO {global_intent_catalog} \
             (id, label, description, category, example_queries, embedding, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6::vector, $7, $8) \
             ON CONFLICT (label) DO UPDATE SET \
                 description = EXCLUDED.description, \
                 category = EXCLUDED.category, \
                 example_queries = EXCLUDED.example_queries, \
                 embedding = EXCLUDED.embedding, \
                 updated_at = EXCLUDED.updated_at"
        ))
        .bind(intent.id)
        .bind(&intent.label)
        .bind(&intent.description)
        .bind(intent.category.as_deref())
        .bind(&intent.example_queries)
        .bind(embedding.as_deref())
        .bind(intent.created_at)
        .bind(intent.updated_at)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
    }

    /// Lists global catalog intents, optionally filtered by category.
    pub async fn list_catalog_intents(&self, category: Option<&str>) -> Result<Vec<CatalogIntent>> {
        let global_intent_catalog = self.table_name("global_intent_catalog");
        let mut query = QueryBuilder::<Postgres>::new(format!(
            "SELECT {CATALOG_INTENT_COLUMNS} FROM {global_intent_catalog} WHERE TRUE"
        ));
        if let Some(category) = category {
            query.push(" AND category = ");
            query.push_bind(category);
        }
        query.push(" ORDER BY category ASC NULLS LAST, label ASC");

        let rows = query
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
        rows.iter().map(catalog_intent_from_row).collect()
    }

    /// Adopts one global catalog intent into a tenant taxonomy.
    pub async fn adopt_catalog_intent(
        &self,
        tenant_id: &str,
        catalog_id: Uuid,
    ) -> Result<TenantIntent> {
        let global_intent_catalog = self.table_name("global_intent_catalog");
        let tenant_intents = self.table_name("tenant_intents");
        let catalog_row = sqlx::query(&format!(
            "SELECT {CATALOG_INTENT_COLUMNS} FROM {global_intent_catalog} WHERE id = $1 LIMIT 1"
        ))
        .bind(catalog_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .ok_or_else(|| {
            MoaError::StorageError(format!(
                "global catalog intent `{catalog_id}` was not found"
            ))
        })?;
        let catalog = catalog_intent_from_row(&catalog_row)?;
        let embedding = catalog
            .embedding
            .as_ref()
            .map(|value| vector_literal(value));

        let row = sqlx::query(&format!(
            "INSERT INTO {tenant_intents} \
             (id, tenant_id, workspace_id, label, description, status, source, catalog_ref, example_queries, embedding) \
             VALUES ($1, $2, $2, $3, $4, 'active', 'catalog', $5, $6, $7::vector) \
             ON CONFLICT (tenant_id, label) DO UPDATE SET \
                 workspace_id = EXCLUDED.workspace_id, \
                 description = EXCLUDED.description, \
                 status = 'active', \
                 source = 'catalog', \
                 catalog_ref = EXCLUDED.catalog_ref, \
                 example_queries = EXCLUDED.example_queries, \
                 embedding = EXCLUDED.embedding, \
                 updated_at = NOW(), \
                 deprecated_at = NULL \
             RETURNING {TENANT_INTENT_COLUMNS}"
        ))
        .bind(Uuid::now_v7())
        .bind(tenant_id)
        .bind(&catalog.label)
        .bind(Some(catalog.description.as_str()))
        .bind(catalog.id)
        .bind(&catalog.example_queries)
        .bind(embedding.as_deref())
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        tenant_intent_from_row(&row)
    }

    /// Deprecates a tenant's adoption of a global catalog intent.
    pub async fn remove_catalog_adoption(&self, tenant_id: &str, catalog_id: Uuid) -> Result<u64> {
        let tenant_intents = self.table_name("tenant_intents");
        let affected = sqlx::query(&format!(
            "UPDATE {tenant_intents} SET \
                 status = 'deprecated', \
                 deprecated_at = NOW(), \
                 updated_at = NOW() \
             WHERE tenant_id = $1 AND catalog_ref = $2 AND source = 'catalog'"
        ))
        .bind(tenant_id)
        .bind(catalog_id)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?
        .rows_affected();
        Ok(affected)
    }

    /// Lists undefined task segments for one tenant in a recent discovery window.
    pub async fn list_undefined_segments(
        &self,
        tenant_id: &str,
        window_days: u64,
        limit: usize,
    ) -> Result<Vec<TaskSegment>> {
        let task_segments = self.table_name("task_segments");
        let rows = sqlx::query(&format!(
            "SELECT {TASK_SEGMENT_COLUMNS} FROM {task_segments} \
             WHERE tenant_id = $1 \
               AND intent_label IS NULL \
               AND started_at >= NOW() - ($2::TEXT || ' days')::INTERVAL \
             ORDER BY started_at DESC \
             LIMIT $3"
        ))
        .bind(tenant_id)
        .bind(window_days.to_string())
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        rows.iter().map(task_segment_from_row).collect()
    }

    /// Reclassifies recent undefined segments after an intent becomes active.
    pub async fn retroactively_classify_intent(
        &self,
        intent_id: Uuid,
        confidence: f64,
        window_days: u64,
        batch_id: Uuid,
    ) -> Result<u64> {
        let intent = self.get_intent(intent_id).await?;
        let task_segments = self.table_name("task_segments");
        let learning_log = self.table_name("learning_log");
        let mut tx = self.pool.begin().await.map_err(map_sqlx_error)?;
        let rows = sqlx::query(&format!(
            "UPDATE {task_segments} SET intent_label = $1, intent_confidence = $2 \
             WHERE tenant_id = $3 \
               AND intent_label IS NULL \
               AND started_at >= NOW() - ($4::TEXT || ' days')::INTERVAL \
             RETURNING id"
        ))
        .bind(&intent.label)
        .bind(confidence)
        .bind(&intent.tenant_id)
        .bind(window_days.to_string())
        .fetch_all(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        for row in &rows {
            let segment_id = row.try_get::<Uuid, _>("id").map_err(map_sqlx_error)?;
            sqlx::query(&format!(
                "INSERT INTO {learning_log} \
                 (id, tenant_id, workspace_id, learning_type, target_id, target_label, payload, confidence, source_refs, actor, batch_id, version) \
                 VALUES ($1, $2, $2, 'intent_classified', $3, $4, $5, $6, $7, 'system', $8, 1)"
            ))
            .bind(Uuid::now_v7())
            .bind(&intent.tenant_id)
            .bind(segment_id.to_string())
            .bind(Some(intent.label.as_str()))
            .bind(Json(serde_json::json!({
                "intent_id": intent.id,
                "retroactive": true
            })))
            .bind(confidence)
            .bind(vec![segment_id])
            .bind(batch_id)
            .execute(&mut *tx)
            .await
            .map_err(map_sqlx_error)?;
        }

        tx.commit().await.map_err(map_sqlx_error)?;
        Ok(rows.len() as u64)
    }
}
