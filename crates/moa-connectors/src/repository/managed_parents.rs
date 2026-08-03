//! Closed managed knowledge-parent claim persistence.

use super::model::{
    CONNECTION_COLUMNS, ConnectionRow, assume_owner_role, check_generation, connection_from_row,
    enqueue_connection_authz_create, enqueue_connection_authz_delete, generation_i64,
    lock_connection, lock_connection_optional,
};
use super::*;

#[derive(FromRow)]
struct ManagedParentClaimRow {
    request_hash: String,
    connection_uid: Uuid,
    parent_created_by_claim: bool,
}

#[async_trait]
impl ManagedParentRepository for PostgresConnectionRepository {
    async fn claim_managed_parent(
        &self,
        request: ManagedParentClaimRequest,
    ) -> Result<ManagedParentClaim> {
        validate_managed_parent_claim_request(&request)?;
        let mut conn = self.begin(request.tenant_id).await?;
        if let Some(claim) =
            load_managed_parent_claim(&mut conn, request.tenant_id, &request.operation_id).await?
        {
            validate_managed_parent_claim_identity(&request, &claim)?;
            let parent =
                lock_connection(&mut conn, request.tenant_id, request.connection_id).await?;
            validate_managed_parent_compatibility(&request, &parent, true)?;
            conn.commit().await?;
            return Ok(ManagedParentClaim {
                connection: parent,
                parent_created_by_claim: claim.parent_created_by_claim,
            });
        }

        let existing =
            lock_connection_optional(&mut conn, request.tenant_id, request.connection_id).await?;
        let (parent, parent_created_by_claim) = match existing {
            Some(parent) => {
                validate_managed_parent_compatibility(&request, &parent, false)?;
                (parent, false)
            }
            None => {
                let owner_identity_id =
                    request
                        .owner_identity_id
                        .ok_or(Error::ManagedParentOwnerRequired {
                            connection_id: request.connection_id,
                        })?;
                insert_managed_parent(&mut conn, &request, owner_identity_id).await?
            }
        };

        let inserted_claim: Option<ManagedParentClaimRow> = sqlx::query_as(
            "INSERT INTO moa.connector_managed_parent_claims (tenant_id, operation_id, \
             request_hash, connection_uid, parent_created_by_claim) VALUES ($1,$2,$3,$4,$5) \
             ON CONFLICT (tenant_id, operation_id) DO NOTHING \
             RETURNING request_hash, connection_uid, parent_created_by_claim",
        )
        .bind(request.tenant_id.0)
        .bind(&request.operation_id)
        .bind(&request.request_hash)
        .bind(request.connection_id.0)
        .bind(parent_created_by_claim)
        .fetch_optional(conn.as_mut())
        .await?;

        let durable_claim = match inserted_claim {
            Some(claim) => claim,
            None => load_managed_parent_claim(&mut conn, request.tenant_id, &request.operation_id)
                .await?
                .ok_or(Error::ManagedParentClaimConflict {
                    connection_id: request.connection_id,
                })?,
        };
        validate_managed_parent_claim_identity(&request, &durable_claim)?;

        if parent_created_by_claim {
            let owner_identity_id =
                request
                    .owner_identity_id
                    .ok_or(Error::ManagedParentOwnerRequired {
                        connection_id: request.connection_id,
                    })?;
            assume_owner_role(&mut conn).await?;
            enqueue_connection_authz_create(
                &mut conn,
                request.tenant_id,
                request.connection_id,
                owner_identity_id,
            )
            .await?;
        }
        conn.commit().await?;
        Ok(ManagedParentClaim {
            connection: parent,
            parent_created_by_claim: durable_claim.parent_created_by_claim,
        })
    }

    async fn activate_managed_knowledge_parent(
        &self,
        request: ManagedParentActivationRequest,
    ) -> Result<ConnectorConnection> {
        let mut conn = self.begin(request.tenant_id).await?;
        let current = lock_connection(&mut conn, request.tenant_id, request.connection_id).await?;
        check_generation(current.generation, request.expected_generation)?;
        validate_managed_parent_definition(
            request.connection_id,
            &current.definition,
            request.definition,
        )?;
        if has_any_action_binding(&mut conn, request.tenant_id, request.connection_id).await? {
            return Err(Error::ManagedParentActionDependents {
                connection_id: request.connection_id,
            });
        }
        if current.status == ConnectionStatus::Active {
            conn.commit().await?;
            return Ok(current);
        }
        if !matches!(
            current.status,
            ConnectionStatus::PendingAuth | ConnectionStatus::Suspended
        ) {
            return Err(Error::InvalidTransition {
                from: current.status,
                to: ConnectionStatus::Active,
            });
        }
        current.status.transition(ConnectionStatus::Active)?;
        let query = format!(
            "UPDATE moa.connector_connections SET lifecycle_status = 'active', updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2 AND config_generation = $3 \
             AND lifecycle_status = $4 RETURNING {CONNECTION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(request.tenant_id.0)
            .bind(request.connection_id.0)
            .bind(generation_i64(request.expected_generation)?)
            .bind(current.status.as_str())
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

    async fn delete_managed_parent_if_unused(
        &self,
        request: ManagedParentDeleteRequest,
    ) -> Result<ManagedParentDeleteOutcome> {
        validate_managed_parent_claim_identity_fields(
            request.connection_id,
            &request.operation_id,
            &request.request_hash,
        )?;
        let mut conn = self.begin(request.tenant_id).await?;
        let claim = load_managed_parent_claim(&mut conn, request.tenant_id, &request.operation_id)
            .await?
            .ok_or(Error::ManagedParentClaimConflict {
                connection_id: request.connection_id,
            })?;
        if claim.connection_uid != request.connection_id.0
            || claim.request_hash != request.request_hash
        {
            return Err(Error::ManagedParentClaimConflict {
                connection_id: request.connection_id,
            });
        }
        let current = lock_connection(&mut conn, request.tenant_id, request.connection_id).await?;
        validate_any_closed_managed_parent(request.connection_id, &current.definition)?;
        if !claim.parent_created_by_claim {
            conn.commit().await?;
            return Ok(ManagedParentDeleteOutcome::Preserved {
                connection: current,
                reason: ManagedParentPreservationReason::PreExisting,
            });
        }
        if current.status == ConnectionStatus::Deleted {
            conn.commit().await?;
            return Ok(ManagedParentDeleteOutcome::AlreadyDeleted(current));
        }
        if has_managed_parent_dependents(
            &mut conn,
            request.tenant_id,
            request.connection_id,
            &request.operation_id,
        )
        .await?
        {
            conn.commit().await?;
            return Ok(ManagedParentDeleteOutcome::Preserved {
                connection: current,
                reason: ManagedParentPreservationReason::DependentCapability,
            });
        }

        let query = format!(
            "UPDATE moa.connector_connections SET lifecycle_status = 'deleted', updated_at = NOW() \
             WHERE tenant_id = $1 AND connection_uid = $2 AND lifecycle_status <> 'deleted' \
             RETURNING {CONNECTION_COLUMNS}"
        );
        let row = sqlx::query_as::<_, ConnectionRow>(&query)
            .bind(request.tenant_id.0)
            .bind(request.connection_id.0)
            .fetch_one(conn.as_mut())
            .await?;
        let deleted = connection_from_row(row)?;
        assume_owner_role(&mut conn).await?;
        enqueue_connection_authz_delete(
            &mut conn,
            request.tenant_id,
            request.connection_id,
            current.owner_identity_id,
            &[],
        )
        .await?;
        conn.commit().await?;
        Ok(ManagedParentDeleteOutcome::Deleted(deleted))
    }
}

fn validate_managed_parent_claim_request(request: &ManagedParentClaimRequest) -> Result<()> {
    validate_managed_parent_claim_identity_fields(
        request.connection_id,
        &request.operation_id,
        &request.request_hash,
    )?;
    if request.display_name.trim().is_empty()
        || request.display_name.trim() != request.display_name
        || request.display_name.len() > 512
    {
        return Err(Error::InvalidContract {
            message:
                "managed parent display name must be non-empty, trimmed, and at most 512 bytes"
                    .to_string(),
        });
    }
    Ok(())
}

fn validate_managed_parent_claim_identity_fields(
    _connection_id: ConnectorConnectionId,
    operation_id: &str,
    request_hash: &str,
) -> Result<()> {
    if operation_id.trim().is_empty()
        || operation_id.trim() != operation_id
        || operation_id.len() > 512
    {
        return Err(Error::InvalidContract {
            message:
                "managed parent operation id must be non-empty, trimmed, and at most 512 bytes"
                    .to_string(),
        });
    }
    if request_hash.len() != 64
        || !request_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::InvalidContract {
            message: "managed parent request hash must be 64 lowercase hexadecimal characters"
                .to_string(),
        });
    }
    Ok(())
}

fn validate_managed_parent_claim_identity(
    request: &ManagedParentClaimRequest,
    claim: &ManagedParentClaimRow,
) -> Result<()> {
    if claim.connection_uid != request.connection_id.0 || claim.request_hash != request.request_hash
    {
        return Err(Error::ManagedParentClaimConflict {
            connection_id: request.connection_id,
        });
    }
    Ok(())
}

fn validate_managed_parent_definition(
    connection_id: ConnectorConnectionId,
    actual: &ConnectionDefinitionRef,
    expected: ManagedParentDefinition,
) -> Result<()> {
    if *actual == expected.definition_ref() {
        Ok(())
    } else {
        Err(Error::ManagedParentMismatch {
            connection_id,
            field: "definition",
        })
    }
}

fn validate_any_closed_managed_parent(
    connection_id: ConnectorConnectionId,
    actual: &ConnectionDefinitionRef,
) -> Result<()> {
    if [
        ManagedParentDefinition::KnowledgeNangoV1,
        ManagedParentDefinition::KnowledgeMergeV1,
    ]
    .into_iter()
    .any(|definition| actual == &definition.definition_ref())
    {
        Ok(())
    } else {
        Err(Error::ManagedParentMismatch {
            connection_id,
            field: "definition",
        })
    }
}

fn validate_managed_parent_compatibility(
    request: &ManagedParentClaimRequest,
    connection: &ConnectorConnection,
    exact_replay: bool,
) -> Result<()> {
    validate_managed_parent_definition(
        request.connection_id,
        &connection.definition,
        request.definition,
    )?;
    let lifecycle_compatible = if exact_replay {
        matches!(
            connection.status,
            ConnectionStatus::PendingAuth | ConnectionStatus::Active | ConnectionStatus::Suspended
        )
    } else {
        connection.status == ConnectionStatus::Active
    };
    if !lifecycle_compatible {
        return Err(Error::ManagedParentMismatch {
            connection_id: request.connection_id,
            field: "lifecycle_status",
        });
    }
    let config = connection
        .non_secret_config
        .as_object()
        .ok_or(Error::ManagedParentMismatch {
            connection_id: request.connection_id,
            field: "non_secret_config",
        })?;
    if !config.is_empty() {
        return Err(Error::ManagedParentMismatch {
            connection_id: request.connection_id,
            field: "non_secret_config",
        });
    }
    Ok(())
}

async fn load_managed_parent_claim(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    operation_id: &str,
) -> Result<Option<ManagedParentClaimRow>> {
    sqlx::query_as(
        "SELECT request_hash, connection_uid, parent_created_by_claim \
         FROM moa.connector_managed_parent_claims \
         WHERE tenant_id = $1 AND operation_id = $2 FOR UPDATE",
    )
    .bind(tenant_id.0)
    .bind(operation_id)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(Error::from)
}

async fn insert_managed_parent(
    conn: &mut ScopedConn<'_>,
    request: &ManagedParentClaimRequest,
    owner_identity_id: Uuid,
) -> Result<(ConnectorConnection, bool)> {
    let config = serde_json::json!({});
    let query = format!(
        "INSERT INTO moa.connector_connections (connection_uid, tenant_id, display_name, \
         built_in_key, built_in_version, non_secret_config, created_by_identity_id, \
         owner_identity_id) VALUES ($1,$2,$3,$4,1,$5,$6,$6) \
         ON CONFLICT (connection_uid) DO NOTHING RETURNING {CONNECTION_COLUMNS}"
    );
    let inserted = sqlx::query_as::<_, ConnectionRow>(&query)
        .bind(request.connection_id.0)
        .bind(request.tenant_id.0)
        .bind(&request.display_name)
        .bind(request.definition.key())
        .bind(config)
        .bind(owner_identity_id)
        .fetch_optional(conn.as_mut())
        .await?;
    match inserted {
        Some(row) => Ok((connection_from_row(row)?, true)),
        None => {
            let parent = lock_connection(conn, request.tenant_id, request.connection_id).await?;
            validate_managed_parent_compatibility(request, &parent, false)?;
            Ok((parent, false))
        }
    }
}

async fn has_any_action_binding(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.connector_action_bindings \
         WHERE tenant_id = $1 AND connection_uid = $2)",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .fetch_one(conn.as_mut())
    .await
    .map_err(Error::from)
}

async fn has_managed_parent_dependents(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    operation_id: &str,
) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT \
           EXISTS (SELECT 1 FROM moa.connector_action_bindings \
                   WHERE tenant_id = $1 AND connection_uid = $2) \
           OR EXISTS (SELECT 1 FROM moa.connector_connection_use_grants \
                      WHERE tenant_id = $1 AND connection_uid = $2) \
           OR EXISTS (SELECT 1 FROM moa.knowledge_connections \
                      WHERE tenant_id = $1 AND connection_uid = $2) \
           OR EXISTS (SELECT 1 FROM moa.knowledge_link_claims \
                      WHERE tenant_id = $1 AND connection_uid = $2 AND operation_id <> $3 \
                        AND state NOT IN ('compensated', 'finalized'))",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .bind(operation_id)
    .fetch_one(conn.as_mut())
    .await
    .map_err(Error::from)
}
