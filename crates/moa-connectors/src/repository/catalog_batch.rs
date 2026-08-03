//! Batched active-connection and action-binding catalog loading.

use super::model::{
    BindingRow, CONNECTION_COLUMNS, ConnectionRow, binding_from_row, connection_from_row,
};
use super::*;

#[async_trait]
impl InstalledConnectorCatalogSource for PostgresConnectionRepository {
    async fn candidates(
        &self,
        tenant_id: TenantId,
        connection_ids: &[ConnectorConnectionId],
    ) -> Result<Vec<(ConnectorConnection, InstalledActionBinding)>> {
        if connection_ids.is_empty() {
            return Ok(Vec::new());
        }
        let connection_ids = connection_ids
            .iter()
            .map(|connection_id| connection_id.0)
            .collect::<Vec<_>>();
        let mut conn = self.begin(tenant_id).await?;
        let connection_query = format!(
            "SELECT {CONNECTION_COLUMNS} FROM moa.connector_connections \
             WHERE tenant_id = $1 AND connection_uid = ANY($2) AND lifecycle_status = 'active' \
             AND health_status <> 'quarantined'"
        );
        let connections = sqlx::query_as::<_, ConnectionRow>(&connection_query)
            .bind(tenant_id.0)
            .bind(&connection_ids)
            .fetch_all(conn.as_mut())
            .await?
            .into_iter()
            .map(connection_from_row)
            .collect::<Result<Vec<_>>>()?;
        let connection_map = connections
            .into_iter()
            .map(|connection| (connection.connection_id, connection))
            .collect::<std::collections::HashMap<_, _>>();

        let bindings = sqlx::query_as::<_, BindingRow>(
            "SELECT binding.binding_uid, binding.tenant_id, binding.connection_uid, \
             binding.action_id, binding.connection_generation, binding.compiled_contract, \
             binding.contract_hash, binding.governed_contract_revision, binding.minimum_effect, \
             binding.enabled FROM moa.connector_action_bindings AS binding \
             JOIN moa.connector_connections AS connection \
               ON connection.connection_uid = binding.connection_uid \
              AND connection.tenant_id = binding.tenant_id \
              AND connection.config_generation = binding.connection_generation \
             WHERE binding.tenant_id = $1 AND binding.connection_uid = ANY($2) \
               AND binding.enabled AND connection.lifecycle_status = 'active' \
               AND connection.health_status <> 'quarantined'",
        )
        .bind(tenant_id.0)
        .bind(&connection_ids)
        .fetch_all(conn.as_mut())
        .await?;

        let mut candidates = Vec::with_capacity(bindings.len());
        for row in bindings {
            let binding = binding_from_row(row)?;
            let connection = connection_map
                .get(&binding.connection_id)
                .cloned()
                .ok_or_else(|| Error::CatalogInvariant {
                    message: "active binding has no active connection projection".to_string(),
                })?;
            candidates.push((connection, binding));
        }
        conn.commit().await?;
        Ok(candidates)
    }
}
