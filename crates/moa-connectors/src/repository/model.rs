//! Shared connector persistence rows and database invariants.

use super::*;

#[derive(FromRow)]
pub(super) struct ConnectionRow {
    connection_uid: Uuid,
    tenant_id: Uuid,
    display_name: String,
    artifact_uid: Option<Uuid>,
    revision_uid: Option<Uuid>,
    built_in_key: Option<String>,
    built_in_version: Option<i64>,
    origin: Option<String>,
    non_secret_config: Value,
    config_generation: i64,
    lifecycle_status: String,
    health_status: String,
    health_reason: Option<String>,
    created_by_identity_id: Option<Uuid>,
    owner_identity_id: Option<Uuid>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(FromRow)]
pub(super) struct BindingRow {
    binding_uid: Uuid,
    tenant_id: Uuid,
    connection_uid: Uuid,
    action_id: String,
    connection_generation: i64,
    compiled_contract: Value,
    contract_hash: String,
    governed_contract_revision: String,
    minimum_effect: String,
    enabled: bool,
}

#[derive(FromRow)]
pub(super) struct PinnedActionRow {
    connection_uid: Uuid,
    connection_tenant_id: Uuid,
    display_name: String,
    artifact_uid: Option<Uuid>,
    revision_uid: Option<Uuid>,
    built_in_key: Option<String>,
    built_in_version: Option<i64>,
    origin: Option<String>,
    non_secret_config: Value,
    config_generation: i64,
    lifecycle_status: String,
    health_status: String,
    health_reason: Option<String>,
    created_by_identity_id: Option<Uuid>,
    owner_identity_id: Option<Uuid>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    binding_uid: Uuid,
    binding_tenant_id: Uuid,
    binding_connection_uid: Uuid,
    action_id: String,
    connection_generation: i64,
    compiled_contract: Value,
    contract_hash: String,
    governed_contract_revision: String,
    minimum_effect: String,
    enabled: bool,
}

pub(super) const CONNECTION_COLUMNS: &str = "connection_uid, tenant_id, display_name, artifact_uid, revision_uid, built_in_key, \
     built_in_version, origin, non_secret_config, config_generation, lifecycle_status, health_status, \
     health_reason, created_by_identity_id, owner_identity_id, created_at, updated_at";

pub(super) struct DefinitionColumns {
    pub(super) artifact_uid: Option<Uuid>,
    pub(super) revision_uid: Option<Uuid>,
    pub(super) built_in_key: Option<String>,
    pub(super) built_in_version: Option<i64>,
}

pub(super) fn definition_columns(
    definition: &ConnectionDefinitionRef,
) -> Result<DefinitionColumns> {
    match definition {
        ConnectionDefinitionRef::Artifact {
            artifact_uid,
            revision_uid,
        } => Ok(DefinitionColumns {
            artifact_uid: Some(*artifact_uid),
            revision_uid: Some(*revision_uid),
            built_in_key: None,
            built_in_version: None,
        }),
        ConnectionDefinitionRef::BuiltIn { key, version } => Ok(DefinitionColumns {
            artifact_uid: None,
            revision_uid: None,
            built_in_key: Some(key.clone()),
            built_in_version: Some(i64::try_from(version.get()).map_err(|_| {
                Error::InvalidContract {
                    message: "built-in connector version exceeds Postgres BIGINT".to_string(),
                }
            })?),
        }),
    }
}

pub(super) fn generation_i64(generation: ConnectionGeneration) -> Result<i64> {
    i64::try_from(generation.get()).map_err(|_| Error::InvalidContract {
        message: "connector generation exceeds Postgres BIGINT".to_string(),
    })
}

pub(super) fn generation_from_i64(value: i64) -> Result<ConnectionGeneration> {
    u64::try_from(value)
        .map_err(|_| Error::InvalidGeneration { value: 0 })
        .and_then(ConnectionGeneration::new)
}

pub(super) fn connection_from_row(row: ConnectionRow) -> Result<ConnectorConnection> {
    let definition = match (
        row.artifact_uid,
        row.revision_uid,
        row.built_in_key,
        row.built_in_version,
    ) {
        (Some(artifact_uid), Some(revision_uid), None, None) => ConnectionDefinitionRef::Artifact {
            artifact_uid,
            revision_uid,
        },
        (None, None, Some(key), Some(version)) => ConnectionDefinitionRef::BuiltIn {
            key,
            version: NonZeroU64::new(u64::try_from(version).map_err(|_| {
                Error::InvalidContract {
                    message: "persisted built-in connector version is invalid".to_string(),
                }
            })?)
            .ok_or_else(|| Error::InvalidContract {
                message: "persisted built-in connector version is invalid".to_string(),
            })?,
        },
        _ => {
            return Err(Error::InvalidContract {
                message: "persisted connector definition reference is inconsistent".to_string(),
            });
        }
    };
    let health = parse_connection_health(&row.health_status)?;
    Ok(ConnectorConnection {
        connection_id: ConnectorConnectionId(row.connection_uid),
        tenant_id: TenantId::from(row.tenant_id),
        display_name: row.display_name,
        definition,
        origin: row.origin.map(|origin| origin.parse()).transpose()?,
        non_secret_config: row.non_secret_config,
        generation: generation_from_i64(row.config_generation)?,
        status: parse_connection_status(&row.lifecycle_status)?,
        health,
        health_reason: row.health_reason,
        created_by_identity_id: row.created_by_identity_id,
        owner_identity_id: row.owner_identity_id,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn parse_connection_status(value: &str) -> Result<ConnectionStatus> {
    match value {
        "pending_auth" => Ok(ConnectionStatus::PendingAuth),
        "active" => Ok(ConnectionStatus::Active),
        "suspended" => Ok(ConnectionStatus::Suspended),
        "disconnecting" => Ok(ConnectionStatus::Disconnecting),
        "deleted" => Ok(ConnectionStatus::Deleted),
        _ => Err(Error::InvalidContract {
            message: "persisted connector lifecycle state is unknown".to_string(),
        }),
    }
}

fn parse_connection_health(value: &str) -> Result<ConnectionHealth> {
    match value {
        "pending" => Ok(ConnectionHealth::Pending),
        "ready" => Ok(ConnectionHealth::Ready),
        "degraded" => Ok(ConnectionHealth::Degraded),
        "unavailable" => Ok(ConnectionHealth::Unavailable),
        "quarantined" => Ok(ConnectionHealth::Quarantined),
        _ => Err(Error::InvalidContract {
            message: "persisted connector health state is unknown".to_string(),
        }),
    }
}

pub(super) async fn lock_connection(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) -> Result<ConnectorConnection> {
    let query = format!(
        "SELECT {CONNECTION_COLUMNS} FROM moa.connector_connections \
         WHERE tenant_id = $1 AND connection_uid = $2 FOR UPDATE"
    );
    let row = sqlx::query_as::<_, ConnectionRow>(&query)
        .bind(tenant_id.0)
        .bind(connection_id.0)
        .fetch_optional(conn.as_mut())
        .await?
        .ok_or(Error::ConnectionNotFound { connection_id })?;
    connection_from_row(row)
}

pub(super) async fn lock_connection_optional(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) -> Result<Option<ConnectorConnection>> {
    let query = format!(
        "SELECT {CONNECTION_COLUMNS} FROM moa.connector_connections \
         WHERE tenant_id = $1 AND connection_uid = $2 FOR UPDATE"
    );
    sqlx::query_as::<_, ConnectionRow>(&query)
        .bind(tenant_id.0)
        .bind(connection_id.0)
        .fetch_optional(conn.as_mut())
        .await?
        .map(connection_from_row)
        .transpose()
}

pub(super) async fn assume_owner_role(conn: &mut ScopedConn<'_>) -> Result<()> {
    sqlx::query("RESET ROLE")
        .execute(conn.as_mut())
        .await
        .map(|_| ())
        .map_err(Error::from)
}

pub(super) fn check_generation(
    actual: ConnectionGeneration,
    expected: ConnectionGeneration,
) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(Error::GenerationConflict { expected, actual })
    }
}

pub(super) async fn enqueue_connection_authz_delete(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    owner_identity_id: Option<Uuid>,
    use_subjects: &[ConnectorUseSubject],
) -> Result<()> {
    enqueue_raw(
        conn.as_mut(),
        TupleOp::Delete,
        &format!("tenant:{tenant_id}"),
        "tenant",
        &format!("connector_connection:{connection_id}"),
        Some(tenant_id.0),
    )
    .await?;
    if let Some(owner_identity_id) = owner_identity_id {
        let owner = TupleKey::new(
            UserType::Operator,
            owner_identity_id,
            Relation::Owner,
            ObjectType::ConnectorConnection,
            connection_id.0,
        );
        enqueue(conn.as_mut(), TupleOp::Delete, &owner, Some(tenant_id.0)).await?;
    }
    for subject in use_subjects {
        enqueue(
            conn.as_mut(),
            TupleOp::Delete,
            &subject.tuple(connection_id),
            Some(tenant_id.0),
        )
        .await?;
    }
    Ok(())
}

pub(super) async fn enqueue_connection_authz_create(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    owner_identity_id: Uuid,
) -> Result<()> {
    enqueue_raw(
        conn.as_mut(),
        TupleOp::Write,
        &format!("tenant:{tenant_id}"),
        "tenant",
        &format!("connector_connection:{connection_id}"),
        Some(tenant_id.0),
    )
    .await?;
    let owner = TupleKey::new(
        UserType::Operator,
        owner_identity_id,
        Relation::Owner,
        ObjectType::ConnectorConnection,
        connection_id.0,
    );
    enqueue(conn.as_mut(), TupleOp::Write, &owner, Some(tenant_id.0)).await?;
    Ok(())
}

pub(super) fn binding_from_row(row: BindingRow) -> Result<InstalledActionBinding> {
    let binding = InstalledActionBinding {
        binding_id: InstalledActionBindingId(row.binding_uid),
        tenant_id: TenantId::from(row.tenant_id),
        connection_id: ConnectorConnectionId(row.connection_uid),
        connection_generation: generation_from_i64(row.connection_generation)?,
        action_id: row.action_id,
        compiled_contract: serde_json::from_value::<CompiledOperationContract>(
            row.compiled_contract,
        )?,
        contract_hash: OperationContractHash::from_str(&row.contract_hash)?,
        governed_contract_revision: row.governed_contract_revision,
        minimum_effect: ActionPolicyEffect::from_str(&row.minimum_effect).map_err(|_| {
            Error::InvalidContract {
                message: "persisted connector minimum effect is unknown".to_string(),
            }
        })?,
        enabled: row.enabled,
    };
    binding.validate()?;
    Ok(binding)
}

pub(super) fn pinned_action_from_row(row: PinnedActionRow) -> Result<PinnedConnectorAction> {
    let connection = connection_from_row(ConnectionRow {
        connection_uid: row.connection_uid,
        tenant_id: row.connection_tenant_id,
        display_name: row.display_name,
        artifact_uid: row.artifact_uid,
        revision_uid: row.revision_uid,
        built_in_key: row.built_in_key,
        built_in_version: row.built_in_version,
        origin: row.origin,
        non_secret_config: row.non_secret_config,
        config_generation: row.config_generation,
        lifecycle_status: row.lifecycle_status,
        health_status: row.health_status,
        health_reason: row.health_reason,
        created_by_identity_id: row.created_by_identity_id,
        owner_identity_id: row.owner_identity_id,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })?;
    let binding = binding_from_row(BindingRow {
        binding_uid: row.binding_uid,
        tenant_id: row.binding_tenant_id,
        connection_uid: row.binding_connection_uid,
        action_id: row.action_id,
        connection_generation: row.connection_generation,
        compiled_contract: row.compiled_contract,
        contract_hash: row.contract_hash,
        governed_contract_revision: row.governed_contract_revision,
        minimum_effect: row.minimum_effect,
        enabled: row.enabled,
    })?;
    Ok(PinnedConnectorAction {
        connection,
        binding,
    })
}
