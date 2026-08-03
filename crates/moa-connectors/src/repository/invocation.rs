//! One-way connector invocation-ledger persistence.

use super::model::{generation_from_i64, generation_i64};
use super::*;

#[derive(FromRow)]
struct InvocationRow {
    invocation_uid: Uuid,
    tenant_id: Uuid,
    connection_uid: Uuid,
    binding_uid: Uuid,
    connection_generation: i64,
    tool_call_id: String,
    request_hash: String,
    upstream_idempotency_key: Option<String>,
    state: String,
    error_metadata: Option<Value>,
    output_metadata: Option<Value>,
    started_at: chrono::DateTime<chrono::Utc>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

const INVOCATION_COLUMNS: &str = "invocation_uid, tenant_id, connection_uid, binding_uid, connection_generation, \
     tool_call_id, request_hash, upstream_idempotency_key, state, error_metadata, \
     output_metadata, started_at, completed_at, updated_at";

#[async_trait]
impl ConnectorInvocationRepository for PostgresConnectionRepository {
    async fn reserve_invocation(
        &self,
        request: InvocationReservationRequest,
    ) -> Result<InvocationReservation> {
        validate_invocation_request(&request)?;
        let mut conn = self.begin(request.tenant_id).await?;
        let query = format!(
            "INSERT INTO moa.connector_action_invocations (invocation_uid, tenant_id, \
             connection_uid, binding_uid, connection_generation, tool_call_id, request_hash, \
             upstream_idempotency_key) \
             SELECT $1,$2,$3,$4,$5,$6,$7,$8 \
             FROM moa.connector_action_bindings AS binding \
             JOIN moa.connector_connections AS connection \
               ON connection.connection_uid = binding.connection_uid \
              AND connection.tenant_id = binding.tenant_id \
              AND connection.config_generation = binding.connection_generation \
             WHERE binding.tenant_id = $2 AND binding.connection_uid = $3 \
               AND binding.binding_uid = $4 AND binding.connection_generation = $5 \
               AND binding.enabled AND connection.lifecycle_status = 'active' \
             ON CONFLICT (tenant_id, tool_call_id) DO NOTHING RETURNING {INVOCATION_COLUMNS}"
        );
        let inserted = sqlx::query_as::<_, InvocationRow>(&query)
            .bind(request.invocation_id.0)
            .bind(request.tenant_id.0)
            .bind(request.connection_id.0)
            .bind(request.binding_id.0)
            .bind(generation_i64(request.connection_generation)?)
            .bind(&request.tool_call_id)
            .bind(request.request_hash.to_string())
            .bind(&request.upstream_idempotency_key)
            .fetch_optional(conn.as_mut())
            .await?;
        let reservation = if let Some(row) = inserted {
            InvocationReservation::Reserved(invocation_from_row(row)?)
        } else {
            let select = format!(
                "SELECT {INVOCATION_COLUMNS} FROM moa.connector_action_invocations \
                 WHERE tenant_id = $1 AND tool_call_id = $2"
            );
            let existing = sqlx::query_as::<_, InvocationRow>(&select)
                .bind(request.tenant_id.0)
                .bind(&request.tool_call_id)
                .fetch_optional(conn.as_mut())
                .await?
                .ok_or_else(|| Error::CatalogInvariant {
                    message:
                        "connector invocation binding is not active, enabled, and current-generation"
                            .to_string(),
                })
                .and_then(invocation_from_row)?;
            if existing.request_hash != request.request_hash
                || existing.connection_id != request.connection_id
                || existing.binding_id != request.binding_id
                || existing.connection_generation != request.connection_generation
                || existing.upstream_idempotency_key != request.upstream_idempotency_key
            {
                return Err(Error::InvocationConflict {
                    tool_call_id: request.tool_call_id,
                });
            }
            if existing.state.is_terminal() {
                InvocationReservation::Replay(existing)
            } else {
                InvocationReservation::InFlight(existing)
            }
        };
        conn.commit().await?;
        Ok(reservation)
    }

    async fn load_invocation(
        &self,
        tenant_id: TenantId,
        invocation_id: ConnectorInvocationId,
    ) -> Result<Option<ConnectorInvocationRecord>> {
        let mut conn = self.begin(tenant_id).await?;
        let query = format!(
            "SELECT {INVOCATION_COLUMNS} FROM moa.connector_action_invocations \
             WHERE tenant_id = $1 AND invocation_uid = $2"
        );
        let row = sqlx::query_as::<_, InvocationRow>(&query)
            .bind(tenant_id.0)
            .bind(invocation_id.0)
            .fetch_optional(conn.as_mut())
            .await?;
        conn.commit().await?;
        row.map(invocation_from_row).transpose()
    }

    async fn mark_transmitting(
        &self,
        tenant_id: TenantId,
        invocation_id: ConnectorInvocationId,
    ) -> Result<ConnectorInvocationRecord> {
        let mut conn = self.begin(tenant_id).await?;
        let query = format!(
            "UPDATE moa.connector_action_invocations SET state = 'transmitting', updated_at = NOW() \
             WHERE tenant_id = $1 AND invocation_uid = $2 AND state = 'reserved' \
             AND EXISTS ( \
                 SELECT 1 FROM moa.connector_action_bindings AS binding \
                 JOIN moa.connector_connections AS connection \
                   ON connection.connection_uid = binding.connection_uid \
                  AND connection.tenant_id = binding.tenant_id \
                  AND connection.config_generation = binding.connection_generation \
                 WHERE binding.binding_uid = moa.connector_action_invocations.binding_uid \
                   AND binding.tenant_id = moa.connector_action_invocations.tenant_id \
                   AND binding.connection_uid = moa.connector_action_invocations.connection_uid \
                   AND binding.connection_generation = moa.connector_action_invocations.connection_generation \
                   AND binding.enabled AND connection.lifecycle_status = 'active' \
             ) \
             RETURNING {INVOCATION_COLUMNS}"
        );
        let updated = sqlx::query_as::<_, InvocationRow>(&query)
            .bind(tenant_id.0)
            .bind(invocation_id.0)
            .fetch_optional(conn.as_mut())
            .await?;
        let result = if let Some(row) = updated {
            invocation_from_row(row)?
        } else {
            let select = format!(
                "SELECT {INVOCATION_COLUMNS} FROM moa.connector_action_invocations \
                 WHERE tenant_id = $1 AND invocation_uid = $2"
            );
            let existing = sqlx::query_as::<_, InvocationRow>(&select)
                .bind(tenant_id.0)
                .bind(invocation_id.0)
                .fetch_optional(conn.as_mut())
                .await?
                .map(invocation_from_row)
                .transpose()?
                .ok_or(Error::InvocationStateConflict {
                    invocation_id,
                    from: ConnectorInvocationState::Reserved,
                    to: ConnectorInvocationState::Transmitting,
                })?;
            return Err(Error::InvocationStateConflict {
                invocation_id,
                from: existing.state,
                to: ConnectorInvocationState::Transmitting,
            });
        };
        conn.commit().await?;
        Ok(result)
    }

    async fn finish_invocation(
        &self,
        tenant_id: TenantId,
        invocation_id: ConnectorInvocationId,
        terminal: ConnectorInvocationTerminal,
    ) -> Result<ConnectorInvocationRecord> {
        validate_terminal_metadata(&terminal)?;
        let target = terminal.state();
        let source = if target == ConnectorInvocationState::FailedBeforeSend {
            ConnectorInvocationState::Reserved
        } else {
            ConnectorInvocationState::Transmitting
        };
        source.transition(invocation_id, target)?;
        let (error_metadata, output_metadata) = terminal_metadata(&terminal);
        let mut conn = self.begin(tenant_id).await?;
        let query = format!(
            "UPDATE moa.connector_action_invocations SET state = $3, error_metadata = $4, \
             output_metadata = $5, completed_at = NOW(), updated_at = NOW() \
             WHERE tenant_id = $1 AND invocation_uid = $2 AND state = $6 \
             RETURNING {INVOCATION_COLUMNS}"
        );
        let updated = sqlx::query_as::<_, InvocationRow>(&query)
            .bind(tenant_id.0)
            .bind(invocation_id.0)
            .bind(target.as_str())
            .bind(error_metadata)
            .bind(output_metadata)
            .bind(source.as_str())
            .fetch_optional(conn.as_mut())
            .await?;
        let result = if let Some(row) = updated {
            invocation_from_row(row)?
        } else {
            let select = format!(
                "SELECT {INVOCATION_COLUMNS} FROM moa.connector_action_invocations \
                 WHERE tenant_id = $1 AND invocation_uid = $2"
            );
            let row = sqlx::query_as::<_, InvocationRow>(&select)
                .bind(tenant_id.0)
                .bind(invocation_id.0)
                .fetch_optional(conn.as_mut())
                .await?
                .ok_or(Error::InvocationStateConflict {
                    invocation_id,
                    from: source,
                    to: target,
                })?;
            let existing = invocation_from_row(row)?;
            if terminal_matches(&existing, &terminal) {
                existing
            } else {
                return Err(Error::InvocationStateConflict {
                    invocation_id,
                    from: existing.state,
                    to: target,
                });
            }
        };
        conn.commit().await?;
        Ok(result)
    }
}

fn validate_invocation_request(request: &InvocationReservationRequest) -> Result<()> {
    if request.tool_call_id.trim().is_empty()
        || request.tool_call_id.trim() != request.tool_call_id
        || request.tool_call_id.len() > 512
    {
        return Err(Error::InvalidContract {
            message: "connector tool-call id must be non-empty, trimmed, and at most 512 bytes"
                .to_string(),
        });
    }
    if request
        .upstream_idempotency_key
        .as_deref()
        .is_some_and(|key| key.trim().is_empty() || key.trim() != key || key.len() > 512)
    {
        return Err(Error::InvalidContract {
            message: "upstream idempotency key must be non-empty, trimmed, and at most 512 bytes"
                .to_string(),
        });
    }
    Ok(())
}

fn invocation_from_row(row: InvocationRow) -> Result<ConnectorInvocationRecord> {
    Ok(ConnectorInvocationRecord {
        invocation_id: ConnectorInvocationId(row.invocation_uid),
        tenant_id: TenantId::from(row.tenant_id),
        connection_id: ConnectorConnectionId(row.connection_uid),
        binding_id: InstalledActionBindingId(row.binding_uid),
        connection_generation: generation_from_i64(row.connection_generation)?,
        tool_call_id: row.tool_call_id,
        request_hash: OperationContractHash::from_str(&row.request_hash)?,
        upstream_idempotency_key: row.upstream_idempotency_key,
        state: parse_invocation_state(&row.state)?,
        error_metadata: row.error_metadata,
        output_metadata: row.output_metadata,
        started_at: row.started_at,
        completed_at: row.completed_at,
        updated_at: row.updated_at,
    })
}

fn parse_invocation_state(value: &str) -> Result<ConnectorInvocationState> {
    match value {
        "reserved" => Ok(ConnectorInvocationState::Reserved),
        "transmitting" => Ok(ConnectorInvocationState::Transmitting),
        "succeeded" => Ok(ConnectorInvocationState::Succeeded),
        "failed_before_send" => Ok(ConnectorInvocationState::FailedBeforeSend),
        "failed" => Ok(ConnectorInvocationState::Failed),
        "unknown_outcome" => Ok(ConnectorInvocationState::UnknownOutcome),
        _ => Err(Error::InvalidContract {
            message: "persisted connector invocation state is unknown".to_string(),
        }),
    }
}

fn terminal_metadata(terminal: &ConnectorInvocationTerminal) -> (Option<&Value>, Option<&Value>) {
    match terminal {
        ConnectorInvocationTerminal::Succeeded { output_metadata } => (None, Some(output_metadata)),
        ConnectorInvocationTerminal::FailedBeforeSend { error_metadata }
        | ConnectorInvocationTerminal::Failed { error_metadata }
        | ConnectorInvocationTerminal::UnknownOutcome { error_metadata } => {
            (Some(error_metadata), None)
        }
    }
}

fn validate_terminal_metadata(terminal: &ConnectorInvocationTerminal) -> Result<()> {
    let (error_metadata, output_metadata) = terminal_metadata(terminal);
    if error_metadata.is_some_and(|value| !value.is_object())
        || output_metadata.is_some_and(|value| !value.is_object())
    {
        return Err(Error::InvalidContract {
            message: "connector invocation terminal metadata must be a JSON object".to_string(),
        });
    }
    Ok(())
}

fn terminal_matches(
    record: &ConnectorInvocationRecord,
    terminal: &ConnectorInvocationTerminal,
) -> bool {
    if record.state != terminal.state() {
        return false;
    }
    let (error_metadata, output_metadata) = terminal_metadata(terminal);
    record.error_metadata.as_ref() == error_metadata
        && record.output_metadata.as_ref() == output_metadata
}
