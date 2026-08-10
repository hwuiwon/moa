//! Consolidated database-backed moa-hands integration tests (one harness binary per lane).

use moa_core::types::identifiers::{SessionId, TenantId};

fn database_url() -> String {
    std::env::var("MOA_DATABASE_URL")
        .expect("MOA_DATABASE_URL must point at a fresh V58 compose Postgres")
}

async fn seed_session(pool: &sqlx::PgPool, session_id: SessionId, tenant_id: TenantId) {
    let user_id = format!("hands-db-{session_id}");
    let mut tx = pool.begin().await.expect("begin session seed transaction");
    sqlx::query(
        "INSERT INTO public.sessions \
         (id, tenant_id, storage_partition_id, user_id, model) \
         VALUES ($1, $2, $3, $4, 'test-model')",
    )
    .bind(session_id)
    .bind(tenant_id)
    .bind(tenant_id.to_string())
    .bind(&user_id)
    .execute(&mut *tx)
    .await
    .expect("seed session tenancy");
    sqlx::query(
        "INSERT INTO public.session_agent_context (\
             session_id, tenant_id, storage_partition_id, user_id,\
             agent_definition_ref, agent_revision_uid, policy_hash, display_name, policy_snapshot\
         ) VALUES (\
             $1, $2, $3, $4, 'agent://system-default',\
             '00000000-0000-4000-8000-000000000a02',\
             'hands-db-policy', 'Hands DB Agent', '{}'::jsonb\
         )",
    )
    .bind(session_id)
    .bind(tenant_id)
    .bind(tenant_id.to_string())
    .bind(&user_id)
    .execute(&mut *tx)
    .await
    .expect("seed required session agent context");
    tx.commit().await.expect("commit session seed transaction");
}

#[path = "hands_db/hand_lease_reaper_db.rs"]
mod hand_lease_reaper_db;

#[path = "hands_db/sandbox_workspace/rls_db.rs"]
mod sandbox_workspace_rls_db;

#[path = "hands_db/sandbox_workspace/lifecycle_db.rs"]
mod sandbox_workspace_lifecycle_db;

#[path = "hands_db/sandbox_workspace/capacity_db.rs"]
mod sandbox_workspace_capacity_db;

#[path = "hands_db/sandbox_workspace/storage_resources_db.rs"]
mod sandbox_workspace_storage_resources_db;

#[path = "hands_db/sandbox_workspace/dispatch_db.rs"]
mod sandbox_workspace_dispatch_db;

#[path = "hands_db/sandbox_workspace/maintenance_db.rs"]
mod sandbox_workspace_maintenance_db;

#[path = "hands_db/sandbox_workspace/retention_db.rs"]
mod sandbox_workspace_retention_db;

#[path = "hands_db/sandbox_workspace/reconciliation_db.rs"]
mod sandbox_workspace_reconciliation_db;

#[path = "hands_db/sandbox_workspace/purge_db.rs"]
mod sandbox_workspace_purge_db;
