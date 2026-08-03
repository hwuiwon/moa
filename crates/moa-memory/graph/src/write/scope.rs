//! Scope validation and runtime identity derivation for graph writes.

use moa_core::types::memory::InformationBarrierId;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::{Error, PostgresGraphStore, Result, edge::EdgeWriteIntent, node::NodeWriteIntent};

use super::node::StoredNode;

/// Extends the transaction-local read clearances with barriers authored by this write.
///
/// PostgreSQL applies the restrictive `node_index` SELECT policy while resolving
/// `INSERT .. ON CONFLICT`, so a writer must be able to see the barrier-tagged
/// row it is inserting or de-duplicating. The value remains transaction-local
/// and is derived only from validated write intents; it never grants a caller a
/// durable or request-wide read clearance.
pub(super) async fn install_write_barriers(
    conn: &mut PgConnection,
    barriers: impl IntoIterator<Item = InformationBarrierId>,
) -> Result<()> {
    let mut barriers = barriers
        .into_iter()
        .map(|barrier| barrier.as_str().to_owned())
        .collect::<Vec<_>>();
    barriers.sort_unstable();
    barriers.dedup();
    if barriers.is_empty() {
        return Ok(());
    }

    sqlx::query(
        r#"
        SELECT pg_catalog.set_config(
            'moa.cleared_barriers',
            array_to_string(
                ARRAY(
                    SELECT DISTINCT barrier
                    FROM unnest(moa.current_cleared_barriers() || $1::TEXT[])
                        AS authored(barrier)
                    ORDER BY barrier
                ),
                ','
            ),
            true
        )
        "#,
    )
    .bind(&barriers)
    .execute(conn)
    .await?;
    Ok(())
}

/// Validates one node intent's scope tuple and object-shaped properties.
pub(super) fn validate_node_scope(intent: &NodeWriteIntent) -> Result<()> {
    validate_scope_shape(
        intent.storage_partition_id.as_deref(),
        intent.contact_id.as_deref(),
        &intent.scope,
    )?;
    if !intent.properties.is_object() {
        return Err(Error::Conflict(
            "node properties must be a JSON object".to_string(),
        ));
    }
    Ok(())
}

/// Validates one edge intent's scope tuple and object-shaped properties.
pub(super) fn validate_edge_scope(intent: &EdgeWriteIntent) -> Result<()> {
    validate_scope_shape(
        intent.storage_partition_id.as_deref(),
        intent.contact_id.as_deref(),
        &intent.scope,
    )?;
    if !intent.properties.is_object() {
        return Err(Error::Conflict(
            "edge properties must be a JSON object".to_string(),
        ));
    }
    Ok(())
}

/// Returns the expected scope tier for a storage-partition/contact pair.
///
/// Returns `None` for the invalid combination of a contact without a storage
/// partition; callers map that to their own error.
pub(crate) fn expected_scope_tier(
    storage_partition_id: Option<&str>,
    contact_id: Option<&str>,
) -> Option<&'static str> {
    match (storage_partition_id, contact_id) {
        (None, None) => Some("global"),
        (Some(_), None) => Some("tenant"),
        (Some(_), Some(_)) => Some("contact"),
        (None, Some(_)) => None,
    }
}

/// Checks that an explicit scope tier matches its ownership identifiers.
pub(super) fn validate_scope_shape(
    storage_partition_id: Option<&str>,
    contact_id: Option<&str>,
    scope: &str,
) -> Result<()> {
    let expected = expected_scope_tier(storage_partition_id, contact_id)
        .ok_or_else(|| Error::Conflict("contact scope requires storage partition".to_string()))?;
    if scope == expected {
        Ok(())
    } else {
        Err(Error::Conflict(format!(
            "scope `{scope}` does not match computed scope `{expected}`"
        )))
    }
}

/// Provides the `(storage_partition_id, contact_id, scope)` tuple used for
/// scope-equality checks across nodes and edges.
pub(super) trait ScopeTriple {
    /// Returns the storage-partition, contact, and tier values compared as one unit.
    fn scope_triple(&self) -> (Option<&str>, Option<&str>, &str);
}

impl ScopeTriple for StoredNode {
    fn scope_triple(&self) -> (Option<&str>, Option<&str>, &str) {
        (
            self.storage_partition_id.as_deref(),
            self.contact_id.as_deref(),
            self.scope.as_str(),
        )
    }
}

impl ScopeTriple for NodeWriteIntent {
    fn scope_triple(&self) -> (Option<&str>, Option<&str>, &str) {
        (
            self.storage_partition_id.as_deref(),
            self.contact_id.as_deref(),
            self.scope.as_str(),
        )
    }
}

impl ScopeTriple for EdgeWriteIntent {
    fn scope_triple(&self) -> (Option<&str>, Option<&str>, &str) {
        (
            self.storage_partition_id.as_deref(),
            self.contact_id.as_deref(),
            self.scope.as_str(),
        )
    }
}

/// Returns an [`Error::Conflict`] with `message` when two scope triples differ.
pub(super) fn ensure_same_scope(
    a: &impl ScopeTriple,
    b: &impl ScopeTriple,
    message: &str,
) -> Result<()> {
    if a.scope_triple() == b.scope_triple() {
        Ok(())
    } else {
        Err(Error::Conflict(message.to_string()))
    }
}

/// Derives a node's runtime tenant/contact IDs and validates its data subject.
pub(super) fn runtime_ids_for_node(
    store: &PostgresGraphStore,
    intent: &NodeWriteIntent,
) -> Result<(Uuid, Option<Uuid>)> {
    let (tenant_id, contact_id) = runtime_ids_from_parts(
        store,
        intent.storage_partition_id.as_deref(),
        intent.contact_id.as_deref(),
        "nodes",
    )?;
    let expected_subject_id = contact_id.unwrap_or(tenant_id);
    if intent.data_subject_id != expected_subject_id {
        return Err(Error::DataSubjectMismatch {
            actual: intent.data_subject_id,
            expected: expected_subject_id,
        });
    }
    Ok((tenant_id, contact_id))
}

/// Derives concrete runtime tenant/contact IDs from store scope or persisted strings.
pub(super) fn runtime_ids_from_parts(
    store: &PostgresGraphStore,
    storage_partition_id: Option<&str>,
    contact_id: Option<&str>,
    target: &str,
) -> Result<(Uuid, Option<Uuid>)> {
    if let Some(scope) = store.scope() {
        return Ok((scope.tenant_id().0, scope.contact_id().map(|id| id.0)));
    }

    let Some(storage_partition_id) = storage_partition_id else {
        return Err(Error::Conflict(format!(
            "tenant-owned graph {target} require tenant scope"
        )));
    };
    let tenant_id = parse_uuid(storage_partition_id, "storage partition", "tenant_id")?;
    let contact_id = contact_id
        .map(|value| parse_uuid(value, "contact", "contact_id"))
        .transpose()?;
    Ok((tenant_id, contact_id))
}

fn parse_uuid(value: &str, value_kind: &str, column: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|error| {
        Error::Conflict(format!(
            "{value_kind} `{value}` cannot be used as {column}: {error}"
        ))
    })
}
