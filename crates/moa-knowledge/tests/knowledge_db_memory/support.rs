//! Shared fixtures for knowledge database-memory tests.

use moa_core::types::{identifiers::TenantId, memory::RlsContext};
use moa_knowledge::domain::LinkedProviderKind;
use sqlx::PgPool;
use uuid::Uuid;

/// Inserts the active managed connector parent required by a knowledge connection fixture.
pub(crate) async fn insert_managed_connector_parent(
    pool: &PgPool,
    tenant_id: TenantId,
    connection_uid: Uuid,
    provider: LinkedProviderKind,
) {
    let mut conn = moa_db::ScopedConn::begin_as_app(pool, &RlsContext::tenant(tenant_id), true)
        .await
        .expect("begin tenant-scoped managed connector parent fixture transaction");
    sqlx::query(
        r#"
        INSERT INTO moa.connector_connections (
            connection_uid, tenant_id, display_name, built_in_key, built_in_version,
            non_secret_config, lifecycle_status, health_status
        )
        VALUES ($1, $2, $3, $4, 1, '{}'::JSONB, 'active', 'ready')
        "#,
    )
    .bind(connection_uid)
    .bind(tenant_id.0)
    .bind(format!("knowledge fixture {connection_uid}"))
    .bind(format!("knowledge:{}", provider.as_str()))
    .execute(conn.as_mut())
    .await
    .expect("insert managed connector parent fixture");
    conn.commit()
        .await
        .expect("commit managed connector parent fixture transaction");
}
