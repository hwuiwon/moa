//! Concrete PostgreSQL transaction for durable tenant purge.

use moa_authz::{FgaClient, FgaTuple, enqueue_raw};
use moa_authz_schema::TupleOp;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum RelationalPurgeOutcome {
    Committed,
    AlreadyCommitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
struct ContactSessionTupleTarget {
    session_id: Uuid,
    contact_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::FromRow)]
struct AgentTupleTarget {
    agent_id: Uuid,
    operator_user_id: Option<Uuid>,
}

pub(super) async fn purge_relational(
    pool: &sqlx::PgPool,
    fga: &FgaClient,
    tenant_id: Uuid,
    operation_id: &str,
) -> Result<RelationalPurgeOutcome, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("db begin: {error}"))?;
    sqlx::query(
        r#"
        INSERT INTO moa.tenant_purge_operations (tenant_id, operation_id)
        VALUES ($1, $2)
        ON CONFLICT (tenant_id) DO NOTHING
        "#,
    )
    .bind(tenant_id)
    .bind(operation_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("create tenant purge fence: {error}"))?;
    let (stored_operation_id, status): (String, String) = sqlx::query_as(
        r#"
        SELECT operation_id, status
        FROM moa.tenant_purge_operations
        WHERE tenant_id = $1
        FOR UPDATE
        "#,
    )
    .bind(tenant_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| format!("lock tenant purge fence: {error}"))?;
    if stored_operation_id != operation_id {
        return Err("tenant purge operation id does not match its durable fence".to_string());
    }
    if status == "relationally_committed" {
        tx.rollback()
            .await
            .map_err(|error| format!("release tenant purge fence: {error}"))?;
        return Ok(RelationalPurgeOutcome::AlreadyCommitted);
    }

    let user_ids = load_ids(
        &mut tx,
        "SELECT id FROM users WHERE tenant_id = $1",
        tenant_id,
    )
    .await
    .map_err(|error| format!("load tenant users: {error}"))?;
    let api_key_ids = load_ids(
        &mut tx,
        "SELECT id FROM api_keys WHERE tenant_id = $1",
        tenant_id,
    )
    .await
    .map_err(|error| format!("load tenant api keys: {error}"))?;
    let session_ids = load_ids(
        &mut tx,
        "SELECT id FROM sessions WHERE tenant_id = $1",
        tenant_id,
    )
    .await
    .map_err(|error| format!("load tenant sessions: {error}"))?;
    let contact_targets: Vec<ContactSessionTupleTarget> = sqlx::query_as(
        "SELECT id AS session_id, contact_id FROM sessions WHERE tenant_id = $1 AND contact_id IS NOT NULL",
    )
    .bind(tenant_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("load tenant contact session tuples: {error}"))?;
    let agent_targets: Vec<AgentTupleTarget> =
        sqlx::query_as("SELECT id AS agent_id, operator_user_id FROM agents WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|error| format!("load tenant agent tuples: {error}"))?;
    let agent_can_act_as_tuples = load_agent_can_act_as_tuples(fga, &agent_targets).await?;

    enqueue_workspace_tuple(&mut tx, tenant_id).await?;
    for user_id in &user_ids {
        for relation in ["admin", "operator"] {
            enqueue_raw(
                &mut *tx,
                TupleOp::Delete,
                &format!("operator:{user_id}"),
                relation,
                &format!("tenant:{tenant_id}"),
                Some(tenant_id),
            )
            .await
            .map_err(|error| format!("tenant user tuple delete: {error}"))?;
        }
    }
    for key_id in &api_key_ids {
        enqueue_api_key_tuples(&mut tx, tenant_id, *key_id).await?;
    }
    for session_id in &session_ids {
        enqueue_raw(
            &mut *tx,
            TupleOp::Delete,
            &format!("tenant:{tenant_id}"),
            "tenant",
            &format!("session:{session_id}"),
            Some(tenant_id),
        )
        .await
        .map_err(|error| format!("session tenant tuple delete: {error}"))?;
        for user_id in &user_ids {
            for relation in ["owner", "participant"] {
                enqueue_raw(
                    &mut *tx,
                    TupleOp::Delete,
                    &format!("operator:{user_id}"),
                    relation,
                    &format!("session:{session_id}"),
                    Some(tenant_id),
                )
                .await
                .map_err(|error| format!("session user tuple delete: {error}"))?;
            }
        }
    }
    for target in &contact_targets {
        for relation in ["owner", "contact"] {
            enqueue_raw(
                &mut *tx,
                TupleOp::Delete,
                &format!("contact:{}", target.contact_id),
                relation,
                &format!("session:{}", target.session_id),
                Some(tenant_id),
            )
            .await
            .map_err(|error| format!("session contact tuple delete: {error}"))?;
        }
    }
    for target in &agent_targets {
        enqueue_agent_tuple_deletes(&mut tx, tenant_id, target, &agent_can_act_as_tuples).await?;
    }

    let storage_partition_id =
        moa_core::types::identifiers::StoragePartitionId::for_tenant(tenant_id.into()).to_string();
    delete_tenant_rows(&mut tx, tenant_id, &storage_partition_id).await?;
    delete_tenant_record(&mut tx, tenant_id).await?;
    sqlx::query(
        r#"
        UPDATE moa.tenant_purge_operations
        SET status = 'relationally_committed', relationally_committed_at = now()
        WHERE tenant_id = $1 AND operation_id = $2
        "#,
    )
    .bind(tenant_id)
    .bind(operation_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("commit tenant purge fence: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| format!("db commit: {error}"))?;
    Ok(RelationalPurgeOutcome::Committed)
}

async fn load_ids(
    tx: &mut Transaction<'_, Postgres>,
    statement: &str,
    tenant_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(statement)
        .bind(tenant_id)
        .fetch_all(&mut **tx)
        .await
}

async fn load_agent_can_act_as_tuples(
    fga: &FgaClient,
    targets: &[AgentTupleTarget],
) -> Result<Vec<FgaTuple>, String> {
    let mut tuples = Vec::new();
    for target in targets {
        let object = format!("agent:{}", target.agent_id);
        let current = fga
            .read(None, Some("can_act_as"), Some(&object))
            .await
            .map_err(|error| format!("load agent can_act_as tuples: {error}"))?;
        tuples.extend(
            current
                .into_iter()
                .filter(|tuple| tuple.relation == "can_act_as" && tuple.object == object),
        );
    }
    Ok(tuples)
}

async fn enqueue_workspace_tuple(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
) -> Result<(), String> {
    enqueue_raw(
        &mut **tx,
        TupleOp::Delete,
        &format!("workspace:{}", moa_core::WORKSPACE_ID),
        "workspace",
        &format!("tenant:{tenant_id}"),
        Some(tenant_id),
    )
    .await
    .map_err(|error| format!("tenant workspace delete tuple: {error}"))
}

async fn enqueue_api_key_tuples(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    key_id: Uuid,
) -> Result<(), String> {
    for (user, relation, object) in [
        (
            format!("tenant:{tenant_id}"),
            "tenant".to_string(),
            format!("api_key:{key_id}"),
        ),
        (
            format!("api_key:{key_id}"),
            "admin".to_string(),
            format!("tenant:{tenant_id}"),
        ),
        (
            format!("api_key:{key_id}"),
            "operator".to_string(),
            format!("tenant:{tenant_id}"),
        ),
    ] {
        enqueue_raw(
            &mut **tx,
            TupleOp::Delete,
            &user,
            &relation,
            &object,
            Some(tenant_id),
        )
        .await
        .map_err(|error| format!("api key tuple delete: {error}"))?;
    }
    Ok(())
}

async fn enqueue_agent_tuple_deletes(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    target: &AgentTupleTarget,
    can_act_as_tuples: &[FgaTuple],
) -> Result<(), String> {
    let agent_object = format!("agent:{}", target.agent_id);
    for tuple in can_act_as_tuples
        .iter()
        .filter(|tuple| tuple.relation == "can_act_as" && tuple.object == agent_object)
    {
        enqueue_raw(
            &mut **tx,
            TupleOp::Delete,
            &tuple.user,
            &tuple.relation,
            &tuple.object,
            Some(tenant_id),
        )
        .await
        .map_err(|error| format!("agent delegation tuple delete: {error}"))?;
    }
    enqueue_raw(
        &mut **tx,
        TupleOp::Delete,
        &format!("tenant:{tenant_id}"),
        "tenant",
        &agent_object,
        Some(tenant_id),
    )
    .await
    .map_err(|error| format!("agent tenant tuple delete: {error}"))?;
    if let Some(operator_user_id) = target.operator_user_id {
        enqueue_raw(
            &mut **tx,
            TupleOp::Delete,
            &format!("operator:{operator_user_id}"),
            "operator",
            &agent_object,
            Some(tenant_id),
        )
        .await
        .map_err(|error| format!("agent operator tuple delete: {error}"))?;
    }
    Ok(())
}

async fn delete_tenant_rows(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    storage_partition_id: &str,
) -> Result<(), String> {
    let tenant_deletes = [
        "DELETE FROM moa.knowledge_object_ingestion_claims WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_contact_group_memberships WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_contact_groups WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_chunks WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_blocks WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_document_versions WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_provider_events WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_ingestion_steps WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_objects WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_sync_runs WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_connections WHERE tenant_id = $1",
        "DELETE FROM security_events WHERE tenant_id = $1",
        "DELETE FROM tenant_audit_destinations WHERE tenant_id = $1",
        "DELETE FROM tenant_signing_keys WHERE tenant_id = $1",
        "DELETE FROM tenant_action_reviews WHERE tenant_id = $1",
        "DELETE FROM action_policy_rules WHERE tenant_id = $1",
        "DELETE FROM builtin_pending_approvals WHERE tenant_id = $1",
        "DELETE FROM auth0_ciba_approvals WHERE session_id IN (SELECT id FROM sessions WHERE tenant_id = $1) OR deciding_user_id IN (SELECT id FROM users WHERE tenant_id = $1)",
        "DELETE FROM moa.hand_leases WHERE tenant_id = $1",
        "DELETE FROM session_agent_context WHERE tenant_id = $1",
        "DELETE FROM session_attachments WHERE tenant_id = $1",
        "DELETE FROM session_blobs WHERE tenant_id = $1",
        "DELETE FROM session_channel_bindings WHERE tenant_id = $1",
        "DELETE FROM contact_verification_challenges WHERE tenant_id = $1",
        "DELETE FROM contact_token_grants WHERE tenant_id = $1",
        "DELETE FROM contact_channel_accounts WHERE tenant_id = $1",
        "DELETE FROM contact_points WHERE tenant_id = $1",
        "DELETE FROM contacts WHERE tenant_id = $1",
        "DELETE FROM tenant_user_invitations WHERE tenant_id = $1",
        "DELETE FROM password_reset_tokens WHERE tenant_id = $1",
        "DELETE FROM user_session_tokens WHERE tenant_id = $1",
        "DELETE FROM local_user_credentials WHERE tenant_id = $1",
        "DELETE FROM auth0_user_map WHERE tenant_id = $1",
        "DELETE FROM linked_connections WHERE user_id IN (SELECT id FROM users WHERE tenant_id = $1)",
        "DELETE FROM scim_group_members WHERE user_id IN (SELECT id FROM users WHERE tenant_id = $1)",
        "DELETE FROM scim_groups WHERE tenant_id = $1",
        "DELETE FROM agents WHERE tenant_id = $1",
        "DELETE FROM api_key_revocations WHERE api_key_id IN (SELECT id FROM api_keys WHERE tenant_id = $1)",
        "DELETE FROM api_keys WHERE tenant_id = $1",
        "DELETE FROM users WHERE tenant_id = $1",
    ];
    for statement in tenant_deletes {
        sqlx::query(statement)
            .bind(tenant_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("{statement}: {error}"))?;
    }

    let storage_deletes = [
        "DELETE FROM moa.agent_deployment WHERE storage_partition_id = $1",
        "DELETE FROM moa.agent_installation WHERE storage_partition_id = $1",
        "DELETE FROM moa.experiment_trial WHERE storage_partition_id = $1",
        "DELETE FROM moa.experiment_run_artifact_revision WHERE storage_partition_id = $1",
        "DELETE FROM moa.experiment_run WHERE storage_partition_id = $1",
        "DELETE FROM analytics.score_run WHERE storage_partition_id = $1",
        "DELETE FROM moa.execution_template_admission WHERE tenant_id = $1::UUID",
        "DELETE FROM moa.execution_action_review_outbox WHERE tenant_id = $1::UUID",
        "DELETE FROM moa.execution_task WHERE tenant_id = $1::UUID",
        "DELETE FROM moa.execution_run WHERE tenant_id = $1::UUID",
        "DELETE FROM moa.artifact_file WHERE storage_partition_id = $1",
        "UPDATE moa.artifact SET latest_revision_uid = NULL WHERE storage_partition_id = $1",
        "DELETE FROM moa.artifact_revision WHERE storage_partition_id = $1",
        "DELETE FROM moa.artifact WHERE storage_partition_id = $1",
        "DELETE FROM learning_candidates WHERE storage_partition_id = $1",
        "DELETE FROM experience_attributions WHERE storage_partition_id = $1",
        "DELETE FROM experience_records WHERE storage_partition_id = $1",
        "DELETE FROM learning_log WHERE storage_partition_id = $1",
        "DELETE FROM task_segments WHERE storage_partition_id = $1",
        "DELETE FROM analytics.turn_lineage WHERE storage_partition_id = $1",
        "DELETE FROM analytics.scores WHERE storage_partition_id = $1",
        "DELETE FROM analytics.audit_roots WHERE storage_partition_id = $1",
        "DELETE FROM analytics.compliance_storage_partition_state WHERE storage_partition_id = $1",
        "DELETE FROM analytics.compliance_tenants WHERE storage_partition_id = $1",
        "DELETE FROM pii_vault.plaintext_side WHERE storage_partition_id = $1",
        "DELETE FROM pii_vault.subject_keys WHERE storage_partition_id = $1",
        "DELETE FROM moa.retrieval_lineage WHERE storage_partition_id = $1",
        "DELETE FROM moa.memory_digests WHERE storage_partition_id = $1",
        "DELETE FROM moa.ingest_dlq WHERE storage_partition_id = $1",
        "DELETE FROM moa.ingest_dedup WHERE storage_partition_id = $1",
        "DELETE FROM moa.embeddings WHERE storage_partition_id = $1",
        "DELETE FROM moa.graph_changelog WHERE storage_partition_id = $1",
        "DELETE FROM moa.edge_index WHERE storage_partition_id = $1",
        "DELETE FROM moa.node_index WHERE storage_partition_id = $1",
        "DELETE FROM moa.storage_partition_state WHERE storage_partition_id = $1",
    ];
    for statement in storage_deletes {
        sqlx::query(statement)
            .bind(storage_partition_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("{statement}: {error}"))?;
    }

    let session_ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM sessions WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| format!("load remaining sessions: {error}"))?;
    if !session_ids.is_empty() {
        sqlx::query("DELETE FROM session_event_dedupe WHERE session_id = ANY($1)")
            .bind(&session_ids)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("delete session dedupe: {error}"))?;
    }
    sqlx::query("UPDATE sessions SET active_channel_binding_id = NULL WHERE tenant_id = $1")
        .bind(tenant_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("clear active channel binding: {error}"))?;
    for statement in [
        "DELETE FROM context_snapshots WHERE tenant_id = $1",
        "DELETE FROM events WHERE tenant_id = $1",
        "DELETE FROM sessions WHERE tenant_id = $1",
    ] {
        sqlx::query(statement)
            .bind(tenant_id)
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("{statement}: {error}"))?;
    }
    Ok(())
}

async fn delete_tenant_record(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
) -> Result<(), String> {
    sqlx::query(
        r#"
        INSERT INTO tenants (id, slug, name, status, deleted_at)
        VALUES ($1, $2, $3, 'deleted', NOW())
        ON CONFLICT (id) DO UPDATE
        SET status = 'deleted', deleted_at = NOW(), updated_at = NOW()
        "#,
    )
    .bind(tenant_id)
    .bind(format!("deleted-{tenant_id}"))
    .bind("deleted tenant")
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("mark tenant deleted: {error}"))?;
    sqlx::query("DELETE FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("delete tenant row: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use moa_authz::FgaConfig;
    use moa_test_support::postgres::bootstrap_test_db;

    use super::*;

    fn offline_fga() -> FgaClient {
        FgaClient::new(FgaConfig {
            url: "http://127.0.0.1:1".to_string(),
            preshared_key: "tenant-purge-test".to_string(),
            store_id: "tenant-purge-test".to_string(),
            model_id: "tenant-purge-test".to_string(),
            timeout_ms: 100,
        })
        .expect("offline FGA config is valid")
    }

    async fn seed_tenant(pool: &sqlx::PgPool, tenant_id: Uuid, user_id: Uuid) {
        sqlx::query("INSERT INTO tenants (id, slug, name) VALUES ($1, $2, 'purge test')")
            .bind(tenant_id)
            .bind(format!("purge-{tenant_id}"))
            .execute(pool)
            .await
            .expect("insert purge test tenant");
        sqlx::query("INSERT INTO users (id, tenant_id, email, active) VALUES ($1, $2, $3, TRUE)")
            .bind(user_id)
            .bind(tenant_id)
            .bind(format!("{user_id}@example.test"))
            .execute(pool)
            .await
            .expect("insert purge test user");
    }

    #[tokio::test]
    async fn relational_purge_is_idempotent_and_preserves_inverse_tuple_intent_db() {
        // Pins: a replay behind PostgreSQL commit skips row deletion and does not duplicate outbox intent.
        let test_db = bootstrap_test_db().await.expect("bootstrap purge db");
        let pool = test_db.store().pool();
        let tenant_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let operation_id = format!("tenant-purge-{tenant_id}");
        seed_tenant(pool, tenant_id, user_id).await;

        assert_eq!(
            purge_relational(pool, &offline_fga(), tenant_id, &operation_id)
                .await
                .expect("first purge commits"),
            RelationalPurgeOutcome::Committed
        );
        assert_eq!(
            purge_relational(pool, &offline_fga(), tenant_id, &operation_id)
                .await
                .expect("replayed purge observes fence"),
            RelationalPurgeOutcome::AlreadyCommitted
        );

        let tenant_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_one(pool)
            .await
            .expect("count tenant rows");
        let user_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(pool)
            .await
            .expect("count user rows");
        let tuples: Vec<(String, String, String)> = sqlx::query_as(
            r#"
            SELECT tuple_user, tuple_relation, tuple_object
            FROM authz_outbox
            WHERE tenant_id = $1 AND op = 'delete'
            ORDER BY tuple_user, tuple_relation, tuple_object
            "#,
        )
        .bind(tenant_id)
        .fetch_all(pool)
        .await
        .expect("load inverse tuple intent");
        assert_eq!(tenant_rows, 0);
        assert_eq!(user_rows, 0);
        assert_eq!(tuples.len(), 3);
        assert!(tuples.contains(&(
            format!("workspace:{}", moa_core::WORKSPACE_ID),
            "workspace".to_string(),
            format!("tenant:{tenant_id}"),
        )));
        assert!(tuples.contains(&(
            format!("operator:{user_id}"),
            "admin".to_string(),
            format!("tenant:{tenant_id}"),
        )));
        assert!(tuples.contains(&(
            format!("operator:{user_id}"),
            "operator".to_string(),
            format!("tenant:{tenant_id}"),
        )));
    }

    #[tokio::test]
    async fn relational_failure_rolls_back_rows_tuples_and_commit_fence_db() {
        // Pins: any relational delete failure rolls back product rows, inverse tuples, and idempotency fence together.
        let test_db = bootstrap_test_db().await.expect("bootstrap rollback db");
        let pool = test_db.store().pool();
        let tenant_id = Uuid::new_v4();
        let user_id = Uuid::new_v4();
        let operation_id = format!("tenant-purge-{tenant_id}");
        seed_tenant(pool, tenant_id, user_id).await;
        sqlx::query(
            r#"
            CREATE FUNCTION reject_test_tenant_delete() RETURNS trigger LANGUAGE plpgsql AS $$
            BEGIN
                IF OLD.id = '00000000-0000-0000-0000-000000000000'::uuid THEN
                    RETURN OLD;
                END IF;
                RAISE EXCEPTION 'scripted tenant purge rollback';
            END
            $$
            "#,
        )
        .execute(pool)
        .await
        .expect("create rollback trigger function");
        sqlx::query(
            "CREATE TRIGGER reject_test_tenant_delete BEFORE DELETE ON tenants FOR EACH ROW EXECUTE FUNCTION reject_test_tenant_delete()",
        )
        .execute(pool)
        .await
        .expect("create rollback trigger");

        let error = purge_relational(pool, &offline_fga(), tenant_id, &operation_id)
            .await
            .expect_err("scripted tenant delete must fail");
        assert!(error.contains("scripted tenant purge rollback"));

        let tenant_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants WHERE id = $1")
            .bind(tenant_id)
            .fetch_one(pool)
            .await
            .expect("count tenant rows after rollback");
        let user_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(pool)
            .await
            .expect("count user rows after rollback");
        let tuple_rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM authz_outbox WHERE tenant_id = $1")
                .bind(tenant_id)
                .fetch_one(pool)
                .await
                .expect("count tuple rows after rollback");
        let fence_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.tenant_purge_operations WHERE tenant_id = $1",
        )
        .bind(tenant_id)
        .fetch_one(pool)
        .await
        .expect("count purge fences after rollback");
        assert_eq!(
            (tenant_rows, user_rows, tuple_rows, fence_rows),
            (1, 1, 0, 0)
        );
    }
}
