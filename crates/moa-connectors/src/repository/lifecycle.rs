//! Connection lifecycle and immutable action-binding persistence.

use super::model::{
    CONNECTION_COLUMNS, ConnectionRow, PinnedActionRow, assume_owner_role, check_generation,
    connection_from_row, definition_columns, enqueue_connection_authz_delete, generation_i64,
    lock_connection, pinned_action_from_row,
};
use super::use_grants::take_connection_use_grants;
use super::*;

#[async_trait]
impl ConnectionLifecycleRepository for PostgresConnectionRepository {
    async fn create(&self, request: NewConnectorConnection) -> Result<ConnectorConnection> {
        validate_new_connection(&request)?;
        let mut conn = self.begin(request.tenant_id).await?;
        let definition = definition_columns(&request.definition_ref)?;
        let query = format!(
            "INSERT INTO moa.connector_connections (connection_uid, tenant_id, display_name, \
             artifact_uid, revision_uid, built_in_key, built_in_version, origin, non_secret_config, \
             created_by_identity_id, owner_identity_id) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING {CONNECTION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(request.connection_id.0)
            .bind(request.tenant_id.0)
            .bind(&request.display_name)
            .bind(definition.artifact_uid)
            .bind(definition.revision_uid)
            .bind(definition.built_in_key)
            .bind(definition.built_in_version)
            .bind(request.origin.as_ref().map(ConnectionOrigin::as_str))
            .bind(&request.non_secret_config)
            .bind(request.created_by_identity_id)
            .bind(request.owner_identity_id)
            .fetch_one(conn.as_mut())
            .await?;

        // The connector row is now fully written under forced tenant RLS. The
        // shared authz outbox is intentionally owner-only, so restore the pool's
        // owning role without leaving this transaction; no connector table is
        // accessed after this boundary.
        assume_owner_role(&mut conn).await?;

        enqueue_raw(
            conn.as_mut(),
            TupleOp::Write,
            &format!("tenant:{}", request.tenant_id),
            "tenant",
            &format!("connector_connection:{}", request.connection_id),
            Some(request.tenant_id.0),
        )
        .await?;
        let owner = TupleKey::new(
            UserType::Operator,
            request.owner_identity_id,
            Relation::Owner,
            ObjectType::ConnectorConnection,
            request.connection_id.0,
        );
        enqueue(
            conn.as_mut(),
            TupleOp::Write,
            &owner,
            Some(request.tenant_id.0),
        )
        .await?;

        let result = connection_from_row(row)?;
        conn.commit().await?;
        Ok(result)
    }

    async fn load(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
    ) -> Result<Option<ConnectorConnection>> {
        let mut conn = self.begin(tenant_id).await?;
        let query = format!(
            "SELECT {CONNECTION_COLUMNS} FROM moa.connector_connections \
             WHERE tenant_id = $1 AND connection_uid = $2"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(tenant_id.0)
            .bind(connection_id.0)
            .fetch_optional(conn.as_mut())
            .await?;
        conn.commit().await?;
        row.map(connection_from_row).transpose()
    }

    async fn list(
        &self,
        tenant_id: TenantId,
        request: ConnectionListRequest,
    ) -> Result<ConnectionListPage> {
        if request.limit == 0 || request.limit > MAX_CONNECTION_LIST_LIMIT {
            return Err(Error::InvalidContract {
                message: "connector list limit must be in 1..=100".to_string(),
            });
        }
        let mut conn = self.begin(tenant_id).await?;
        let query = format!(
            "SELECT {CONNECTION_COLUMNS} FROM moa.connector_connections \
             WHERE tenant_id = $1 AND lifecycle_status <> 'deleted' \
               AND ($2::UUID IS NULL OR connection_uid > $2) \
             ORDER BY connection_uid LIMIT $3"
        );
        let rows = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(tenant_id.0)
            .bind(request.after.map(|cursor| cursor.0))
            .bind(i64::from(request.limit) + 1)
            .fetch_all(conn.as_mut())
            .await?;
        let mut connections = rows
            .into_iter()
            .map(connection_from_row)
            .collect::<Result<Vec<_>>>()?;
        let has_more = connections.len() > usize::from(request.limit);
        if has_more {
            connections.truncate(usize::from(request.limit));
        }
        let next_cursor = has_more
            .then(|| {
                connections
                    .last()
                    .map(|connection| connection.connection_id)
            })
            .flatten();
        conn.commit().await?;
        Ok(ConnectionListPage {
            connections,
            next_cursor,
        })
    }

    async fn load_pinned_action(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        binding_id: InstalledActionBindingId,
    ) -> Result<Option<PinnedConnectorAction>> {
        let mut conn = self.begin(tenant_id).await?;
        let row = sqlx::query_as::<_, PinnedActionRow>(
            "SELECT \
                 c.connection_uid, c.tenant_id AS connection_tenant_id, c.display_name, \
                 c.artifact_uid, c.revision_uid, c.built_in_key, c.built_in_version, c.origin, \
                 c.non_secret_config, c.config_generation, c.lifecycle_status, c.health_status, \
                 c.health_reason, c.created_by_identity_id, c.owner_identity_id, c.created_at, \
                 c.updated_at, b.binding_uid, b.tenant_id AS binding_tenant_id, \
                 b.connection_uid AS binding_connection_uid, b.action_id, \
                 b.connection_generation, b.compiled_contract, b.contract_hash, \
                 b.governed_contract_revision, b.minimum_effect, b.enabled \
             FROM moa.connector_connections AS c \
             INNER JOIN moa.connector_action_bindings AS b \
               ON b.tenant_id = c.tenant_id AND b.connection_uid = c.connection_uid \
             WHERE c.tenant_id = $1 AND c.connection_uid = $2 AND b.binding_uid = $3",
        )
        .bind(tenant_id.0)
        .bind(connection_id.0)
        .bind(binding_id.0)
        .fetch_optional(conn.as_mut())
        .await?;
        conn.commit().await?;
        row.map(pinned_action_from_row).transpose()
    }

    async fn transition(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
        target: ConnectionStatus,
    ) -> Result<ConnectorConnection> {
        let mut conn = self.begin(tenant_id).await?;
        let current = lock_connection(&mut conn, tenant_id, connection_id).await?;
        check_generation(current.generation, expected_generation)?;
        if target == ConnectionStatus::Active && current.status == ConnectionStatus::PendingAuth {
            return Err(Error::InvalidContract {
                message: "initial connection activation must compile bindings and verify credential slots"
                    .to_string(),
            });
        }
        current.status.transition(target)?;
        if target == ConnectionStatus::Active
            && !has_enabled_current_binding(&mut conn, tenant_id, connection_id, current.generation)
                .await?
        {
            return Err(Error::InvalidContract {
                message: "connection has no enabled binding for its current credential generation"
                    .to_string(),
            });
        }
        let query = format!(
            "UPDATE moa.connector_connections SET lifecycle_status = $3, updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2 AND config_generation = $4 \
             AND lifecycle_status = $5 RETURNING {CONNECTION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(tenant_id.0)
            .bind(connection_id.0)
            .bind(target.as_str())
            .bind(generation_i64(expected_generation)?)
            .bind(current.status.as_str())
            .fetch_optional(conn.as_mut())
            .await?
            .ok_or(Error::GenerationConflict {
                expected: expected_generation,
                actual: current.generation,
            })?;

        if matches!(
            target,
            ConnectionStatus::Disconnecting | ConnectionStatus::Deleted
        ) {
            sqlx::query(
                "UPDATE moa.connector_action_bindings SET enabled = FALSE, updated_at = NOW() \
                 WHERE tenant_id = $1 AND connection_uid = $2 AND enabled",
            )
            .bind(tenant_id.0)
            .bind(connection_id.0)
            .execute(conn.as_mut())
            .await?;
        }
        if target == ConnectionStatus::Deleted {
            let grants = take_connection_use_grants(&mut conn, tenant_id, connection_id).await?;
            // All connector reads and writes completed under `moa_app`. The
            // exact inverse intents must share this transaction, while the
            // outbox deliberately remains unavailable to the application role.
            assume_owner_role(&mut conn).await?;
            enqueue_connection_authz_delete(
                &mut conn,
                tenant_id,
                connection_id,
                current.owner_identity_id,
                &grants,
            )
            .await?;
        }
        let result = connection_from_row(row)?;
        conn.commit().await?;
        Ok(result)
    }

    async fn update_health(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
        health: ConnectionHealth,
        reason: Option<String>,
    ) -> Result<ConnectorConnection> {
        validate_health_reason(reason.as_deref())?;
        let mut conn = self.begin(tenant_id).await?;
        let current = lock_connection(&mut conn, tenant_id, connection_id).await?;
        check_generation(current.generation, expected_generation)?;
        let query = format!(
            "UPDATE moa.connector_connections SET health_status = $3, health_reason = $4, \
             updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2 AND config_generation = $5 \
             RETURNING {CONNECTION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(tenant_id.0)
            .bind(connection_id.0)
            .bind(health.as_str())
            .bind(reason)
            .bind(generation_i64(expected_generation)?)
            .fetch_optional(conn.as_mut())
            .await?;
        let row = match row {
            Some(row) => row,
            None => {
                return Err(Error::GenerationConflict {
                    expected: expected_generation,
                    actual: current.generation,
                });
            }
        };
        let result = connection_from_row(row)?;
        conn.commit().await?;
        Ok(result)
    }

    async fn advance_credential_generation(
        &self,
        tenant_id: TenantId,
        connection_id: ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> Result<ConnectorConnection> {
        let next_generation = expected_generation.next()?;
        let mut conn = self.begin(tenant_id).await?;
        let current = lock_connection(&mut conn, tenant_id, connection_id).await?;
        check_generation(current.generation, expected_generation)?;
        let target = match current.status {
            ConnectionStatus::Active => ConnectionStatus::Suspended,
            ConnectionStatus::PendingAuth | ConnectionStatus::Suspended => current.status,
            ConnectionStatus::Disconnecting | ConnectionStatus::Deleted => {
                return Err(Error::InvalidContract {
                    message:
                        "credential generation cannot advance while connection teardown is active"
                            .to_string(),
                });
            }
        };

        sqlx::query(
            "UPDATE moa.connector_action_bindings SET enabled = FALSE, updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2 AND enabled",
        )
        .bind(tenant_id.0)
        .bind(connection_id.0)
        .execute(conn.as_mut())
        .await?;
        let query = format!(
            "UPDATE moa.connector_connections SET lifecycle_status = $3, config_generation = $4, \
             updated_at = NOW() WHERE tenant_id = $1 AND connection_uid = $2 \
             AND config_generation = $5 AND lifecycle_status = $6 \
             RETURNING {CONNECTION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(tenant_id.0)
            .bind(connection_id.0)
            .bind(target.as_str())
            .bind(generation_i64(next_generation)?)
            .bind(generation_i64(expected_generation)?)
            .bind(current.status.as_str())
            .fetch_optional(conn.as_mut())
            .await?
            .ok_or(Error::GenerationConflict {
                expected: expected_generation,
                actual: current.generation,
            })?;
        let result = connection_from_row(row)?;
        conn.commit().await?;
        Ok(result)
    }

    async fn activate(&self, request: ConnectionActivation) -> Result<ConnectorConnection> {
        let next = request.expected_generation.next()?;
        validate_activation_bindings(&request, next)?;
        let mut conn = self.begin(request.tenant_id).await?;
        let current = lock_connection(&mut conn, request.tenant_id, request.connection_id).await?;
        check_generation(current.generation, request.expected_generation)?;
        if !matches!(
            current.status,
            ConnectionStatus::PendingAuth | ConnectionStatus::Suspended
        ) {
            return Err(Error::InvalidContract {
                message: "compiled connector activation requires pending_auth or suspended"
                    .to_string(),
            });
        }
        current.status.transition(ConnectionStatus::Active)?;

        sqlx::query(
            "UPDATE moa.connector_action_bindings SET enabled = FALSE, updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2 AND enabled",
        )
        .bind(request.tenant_id.0)
        .bind(request.connection_id.0)
        .execute(conn.as_mut())
        .await?;
        for binding in &request.bindings {
            insert_binding(&mut conn, binding).await?;
        }
        let query = format!(
            "UPDATE moa.connector_connections SET lifecycle_status = 'active', \
             config_generation = $3, updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2 AND config_generation = $4 \
             RETURNING {CONNECTION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(request.tenant_id.0)
            .bind(request.connection_id.0)
            .bind(generation_i64(next)?)
            .bind(generation_i64(request.expected_generation)?)
            .fetch_optional(conn.as_mut())
            .await?
            .ok_or(Error::GenerationConflict {
                expected: request.expected_generation,
                actual: current.generation,
            })?;
        let result = connection_from_row(row)?;
        conn.commit().await?;
        Ok(result)
    }
}

fn validate_new_connection(request: &NewConnectorConnection) -> Result<()> {
    if request.display_name.trim().is_empty() || request.display_name.trim() != request.display_name
    {
        return Err(Error::InvalidContract {
            message: "connector display name must be non-empty and trimmed".to_string(),
        });
    }
    if !request.non_secret_config.is_object() {
        return Err(Error::InvalidContract {
            message: "connector non-secret config must be a JSON object".to_string(),
        });
    }
    if request.non_secret_config.get("origin").is_some() {
        return Err(Error::InvalidContract {
            message: "connector origin must not be duplicated in non-secret configuration"
                .to_string(),
        });
    }
    match (&request.definition_ref, &request.origin) {
        (ConnectionDefinitionRef::Artifact { .. }, Some(_))
        | (ConnectionDefinitionRef::BuiltIn { .. }, None) => {}
        (ConnectionDefinitionRef::Artifact { .. }, None) => {
            return Err(Error::InvalidConnectionOrigin {
                reason: "artifact connector requires a typed origin",
            });
        }
        (ConnectionDefinitionRef::BuiltIn { .. }, Some(_)) => {
            return Err(Error::InvalidConnectionOrigin {
                reason: "managed connector parent cannot carry an HTTP origin",
            });
        }
    }
    Ok(())
}

fn validate_health_reason(reason: Option<&str>) -> Result<()> {
    if reason.is_some_and(|value| {
        value.trim().is_empty() || value.trim() != value || value.len() > 2_048
    }) {
        return Err(Error::InvalidContract {
            message: "connector health reason must be non-empty, trimmed, and at most 2048 bytes"
                .to_string(),
        });
    }
    Ok(())
}

async fn has_enabled_current_binding(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    generation: ConnectionGeneration,
) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.connector_action_bindings \
         WHERE tenant_id = $1 AND connection_uid = $2 \
         AND connection_generation = $3 AND enabled)",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .bind(generation_i64(generation)?)
    .fetch_one(conn.as_mut())
    .await
    .map_err(Error::from)
}

fn validate_activation_bindings(
    request: &ConnectionActivation,
    expected_generation: ConnectionGeneration,
) -> Result<()> {
    if request.bindings.is_empty() {
        return Err(Error::InvalidContract {
            message: "connector activation requires at least one action binding".to_string(),
        });
    }
    if request.bindings.len() > crate::service::MAX_CONNECTOR_ACTION_BINDINGS {
        return Err(Error::InvalidContract {
            message: "connector activation accepts at most 64 action bindings".to_string(),
        });
    }
    let mut action_ids = std::collections::BTreeSet::new();
    for binding in &request.bindings {
        binding.validate()?;
        if binding.minimum_effect != ActionPolicyEffect::AdminReview {
            return Err(Error::InvalidContract {
                message: "HTTP connector binding minimum effect must require admin review"
                    .to_string(),
            });
        }
        if binding.tenant_id != request.tenant_id
            || binding.connection_id != request.connection_id
            || binding.connection_generation != expected_generation
            || !binding.enabled
        {
            return Err(Error::InvalidContract {
                message: "connector activation binding identity or generation is inconsistent"
                    .to_string(),
            });
        }
        if !action_ids.insert(binding.action_id.as_str()) {
            return Err(Error::InvalidContract {
                message: "connector activation contains a duplicate action id".to_string(),
            });
        }
    }
    Ok(())
}

async fn insert_binding(conn: &mut ScopedConn<'_>, binding: &InstalledActionBinding) -> Result<()> {
    sqlx::query(
        "INSERT INTO moa.connector_action_bindings (binding_uid, tenant_id, connection_uid, \
         action_id, connection_generation, compiled_contract, contract_hash, \
         governed_contract_revision, minimum_effect, enabled) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(binding.binding_id.0)
    .bind(binding.tenant_id.0)
    .bind(binding.connection_id.0)
    .bind(&binding.action_id)
    .bind(generation_i64(binding.connection_generation)?)
    .bind(serde_json::to_value(&binding.compiled_contract)?)
    .bind(binding.contract_hash.to_string())
    .bind(&binding.governed_contract_revision)
    .bind(binding.minimum_effect.as_str())
    .bind(binding.enabled)
    .execute(conn.as_mut())
    .await?;
    Ok(())
}
