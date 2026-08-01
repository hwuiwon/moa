//! Concrete PostgreSQL transaction for durable tenant purge.

use moa_authz::{FgaClient, FgaTuple, enqueue_raw};
use moa_authz_schema::TupleOp;
use sqlx::{Postgres, Transaction};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PurgeBinding {
    Tenant,
    StoragePartition,
}

#[derive(Debug, Clone, Copy)]
struct PurgeStep {
    table: &'static str,
    cleanup_sql: &'static str,
    binding: PurgeBinding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
/// Outcome of the idempotent relational tenant-purge transaction.
pub enum RelationalPurgeOutcome {
    /// This invocation committed relational deletion.
    Committed,
    /// The same operation had already committed relational deletion.
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

type InverseTuple = (String, String, String);

/// Executes the fenced, catalog-checked relational tenant purge.
pub async fn purge_relational(
    pool: &sqlx::PgPool,
    fga: &FgaClient,
    tenant_id: Uuid,
    operation_id: &str,
) -> Result<RelationalPurgeOutcome, String> {
    let tenant = moa_core::types::identifiers::TenantId::from(tenant_id);
    let stage_guard =
        moa_memory_pii::legal_hold::begin_destruction_stage_guard(pool, tenant, &[], operation_id)
            .await
            .map_err(|error| format!("relational destruction fence: {error}"))?;

    let purge_result = purge_relational_transaction(pool, fga, tenant_id, operation_id).await;
    let guard_result = stage_guard
        .finish()
        .await
        .map_err(|error| format!("release relational destruction fence: {error}"));
    match (purge_result, guard_result) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(purge_error), Err(guard_error)) => Err(format!(
            "{purge_error}; additionally failed to {guard_error}"
        )),
    }
}

async fn purge_relational_transaction(
    pool: &sqlx::PgPool,
    fga: &FgaClient,
    tenant_id: Uuid,
    operation_id: &str,
) -> Result<RelationalPurgeOutcome, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("db begin: {error}"))?;
    let operation = async {
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
        return Ok(RelationalPurgeOutcome::AlreadyCommitted);
    }
    let storage_partition_id =
        moa_core::types::identifiers::StoragePartitionId::for_tenant(tenant_id.into()).to_string();
    sqlx::query(
        r#"
        SELECT
            pg_catalog.set_config('moa.tenant_id', $1, true),
            pg_catalog.set_config('moa.storage_partition_id', $2, true)
        "#,
    )
    .bind(tenant_id.to_string())
    .bind(&storage_partition_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("scope tenant purge transaction: {error}"))?;
    if moa_memory_vector::sync::has_active_vector_sync_claims_in_tx(
        &mut tx,
        &storage_partition_id,
    )
    .await
    .map_err(|error| format!("relational vector-sync claim check: {error}"))?
    {
        return Err(
            "relational purge is waiting for active vector-sync claims to settle or expire"
                .to_string(),
        );
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

    let mut inverse_tuples = BTreeSet::new();
    enqueue_workspace_tuple(&mut tx, tenant_id, &mut inverse_tuples).await?;
    for user_id in &user_ids {
        for relation in ["admin", "operator"] {
            enqueue_inverse_tuple(
                &mut tx,
                tenant_id,
                &mut inverse_tuples,
                format!("operator:{user_id}"),
                relation,
                format!("tenant:{tenant_id}"),
                "tenant user tuple delete",
            )
            .await?;
        }
    }
    for key_id in &api_key_ids {
        enqueue_api_key_tuples(&mut tx, tenant_id, *key_id, &mut inverse_tuples).await?;
    }
    for session_id in &session_ids {
        enqueue_inverse_tuple(
            &mut tx,
            tenant_id,
            &mut inverse_tuples,
            format!("tenant:{tenant_id}"),
            "tenant",
            format!("session:{session_id}"),
            "session tenant tuple delete",
        )
        .await?;
        for user_id in &user_ids {
            for relation in ["owner", "participant"] {
                enqueue_inverse_tuple(
                    &mut tx,
                    tenant_id,
                    &mut inverse_tuples,
                    format!("operator:{user_id}"),
                    relation,
                    format!("session:{session_id}"),
                    "session user tuple delete",
                )
                .await?;
            }
        }
    }
    for target in &contact_targets {
        for relation in ["owner", "contact"] {
            enqueue_inverse_tuple(
                &mut tx,
                tenant_id,
                &mut inverse_tuples,
                format!("contact:{}", target.contact_id),
                relation,
                format!("session:{}", target.session_id),
                "session contact tuple delete",
            )
            .await?;
        }
    }
    for target in &agent_targets {
        enqueue_agent_tuple_deletes(
            &mut tx,
            tenant_id,
            target,
            &agent_can_act_as_tuples,
            &mut inverse_tuples,
        )
        .await?;
    }

    delete_tenant_rows(
        &mut tx,
        tenant_id,
        &storage_partition_id,
        &inverse_tuples,
    )
    .await?;
    delete_tenant_record(&mut tx, tenant_id).await?;
    assert_tenant_record_deleted(&mut tx, tenant_id).await?;
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
        Ok(RelationalPurgeOutcome::Committed)
    }
    .await;

    match operation {
        Ok(RelationalPurgeOutcome::Committed) => tx
            .commit()
            .await
            .map(|_| RelationalPurgeOutcome::Committed)
            .map_err(|error| format!("db commit: {error}")),
        Ok(RelationalPurgeOutcome::AlreadyCommitted) => tx
            .rollback()
            .await
            .map(|_| RelationalPurgeOutcome::AlreadyCommitted)
            .map_err(|error| format!("release tenant purge fence: {error}")),
        Err(operation_error) => match tx.rollback().await {
            Ok(()) => Err(operation_error),
            Err(rollback_error) => Err(format!(
                "{operation_error}; additionally failed to roll back tenant purge transaction: {rollback_error}"
            )),
        },
    }
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
    inverse_tuples: &mut BTreeSet<InverseTuple>,
) -> Result<(), String> {
    enqueue_inverse_tuple(
        tx,
        tenant_id,
        inverse_tuples,
        format!("workspace:{}", moa_core::WORKSPACE_ID),
        "workspace",
        format!("tenant:{tenant_id}"),
        "tenant workspace delete tuple",
    )
    .await
}

async fn enqueue_api_key_tuples(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    key_id: Uuid,
    inverse_tuples: &mut BTreeSet<InverseTuple>,
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
        enqueue_inverse_tuple(
            tx,
            tenant_id,
            inverse_tuples,
            user,
            &relation,
            object,
            "api key tuple delete",
        )
        .await?;
    }
    Ok(())
}

async fn enqueue_agent_tuple_deletes(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    target: &AgentTupleTarget,
    can_act_as_tuples: &[FgaTuple],
    inverse_tuples: &mut BTreeSet<InverseTuple>,
) -> Result<(), String> {
    let agent_object = format!("agent:{}", target.agent_id);
    for tuple in can_act_as_tuples
        .iter()
        .filter(|tuple| tuple.relation == "can_act_as" && tuple.object == agent_object)
    {
        enqueue_inverse_tuple(
            tx,
            tenant_id,
            inverse_tuples,
            tuple.user.clone(),
            &tuple.relation,
            tuple.object.clone(),
            "agent delegation tuple delete",
        )
        .await?;
    }
    enqueue_inverse_tuple(
        tx,
        tenant_id,
        inverse_tuples,
        format!("tenant:{tenant_id}"),
        "tenant",
        agent_object.clone(),
        "agent tenant tuple delete",
    )
    .await?;
    if let Some(operator_user_id) = target.operator_user_id {
        enqueue_inverse_tuple(
            tx,
            tenant_id,
            inverse_tuples,
            format!("operator:{operator_user_id}"),
            "operator",
            agent_object,
            "agent operator tuple delete",
        )
        .await?;
    }
    Ok(())
}

async fn enqueue_inverse_tuple(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    inverse_tuples: &mut BTreeSet<InverseTuple>,
    user: String,
    relation: &str,
    object: String,
    context: &str,
) -> Result<(), String> {
    enqueue_raw(
        &mut **tx,
        TupleOp::Delete,
        &user,
        relation,
        &object,
        Some(tenant_id),
    )
    .await
    .map_err(|error| format!("{context}: {error}"))?;
    inverse_tuples.insert((user, relation.to_string(), object));
    Ok(())
}

async fn delete_tenant_rows(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    storage_partition_id: &str,
    inverse_tuples: &BTreeSet<InverseTuple>,
) -> Result<(), String> {
    let tenant_deletes = [
        "DELETE FROM oauth_tokens WHERE tenant_id = $1",
        "DELETE FROM oauth_authorization_codes WHERE tenant_id = $1",
        "DELETE FROM oauth_authorization_transactions WHERE tenant_id = $1",
        "DELETE FROM token_vault_connections WHERE tenant_id = $1",
        "DELETE FROM moa.dual_control_request WHERE tenant_id = $1",
        "DELETE FROM moa.audit_jti_used WHERE tenant_id = $1",
        "DELETE FROM moa.erasure_jobs WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_object_ingestion_claims WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_semantic_graph_extractions WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_contact_group_memberships WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_contact_groups WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_chunks WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_blocks WHERE tenant_id = $1",
        // Move every object out of its current ACL position before deleting the
        // immutable evidence it names. The restrictive FK makes this ordering
        // mandatory instead of silently clearing only the pointer.
        "UPDATE moa.knowledge_objects SET acl_state = 'incomplete', acl_revision = NULL, current_acl_snapshot_id = NULL WHERE tenant_id = $1 AND current_acl_snapshot_id IS NOT NULL",
        "DELETE FROM moa.knowledge_source_acl_entries WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_source_acl_snapshots WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_source_principal_group_bindings WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_source_principal_bindings WHERE tenant_id = $1",
        "DELETE FROM moa.knowledge_source_acl_epochs WHERE tenant_id = $1",
        // The fingerprint key goes last and must go at all: leaving it behind
        // would keep a purged tenant's keyed principal material recoverable from
        // any surviving copy of an entry or binding row.
        "DELETE FROM moa.knowledge_source_acl_keys WHERE tenant_id = $1",
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
        // Keyed on `tenant_id` rather than the storage partition: the erasure
        // decision ledger records dispositions for a tenant's subjects and has no
        // partition column.
        "DELETE FROM moa.privacy_erasure_record_decision WHERE tenant_id = $1",
        "DELETE FROM moa.hand_leases WHERE tenant_id = $1",
        "DELETE FROM moa.tenant_sandbox_policy WHERE tenant_id = $1",
        "DELETE FROM moa.execution_node_materialization WHERE tenant_id = $1",
        "DELETE FROM moa.execution_planner_call_audit WHERE tenant_id = $1",
        "DELETE FROM moa.execution_compile_audit WHERE tenant_id = $1",
        "DELETE FROM moa.execution_route_audit WHERE tenant_id = $1",
        "DELETE FROM moa.execution_action_review_outbox WHERE tenant_id = $1",
        "DELETE FROM moa.execution_task WHERE tenant_id = $1",
        "DELETE FROM moa.execution_template_admission WHERE tenant_id = $1",
        "DELETE FROM moa.execution_run WHERE tenant_id = $1",
        "DELETE FROM moa.execution_planning_context WHERE tenant_id = $1",
        // Archived session history, single-sourced from the crate that owns the
        // table. The column name lives only inside a string literal, so nothing
        // connects this entry to the schema; `moa-session` exports the canonical
        // statement so the purge and the test that proves it cannot drift apart.
        // V000364 declares the `session_id` foreign key WITHOUT `ON DELETE
        // CASCADE` deliberately, so removing this step fails the later
        // `DELETE FROM sessions` on a foreign-key violation instead of silently
        // leaving a purged tenant's conversation history behind in the archive.
        moa_session::archive::TENANT_PURGE_SQL,
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
    let storage_deletes = [
        "DELETE FROM moa.agent_deployment WHERE storage_partition_id = $1",
        "DELETE FROM moa.agent_installation WHERE storage_partition_id = $1",
        // Behavior Lab score provenance goes before the trials it references.
        // V000361 declares that foreign key WITHOUT `ON DELETE CASCADE`
        // deliberately, so removing this line fails the trial delete on a
        // foreign-key violation instead of silently leaving a purged tenant's
        // evaluator evidence behind. A cascade here would make this step
        // unfalsifiable: neutering it would change nothing observable.
        "DELETE FROM moa.experiment_score_provenance WHERE storage_partition_id = $1",
        // Resource reservations reference both the trial and the run, so they go
        // before either. V000371 does declare `ON DELETE CASCADE` on those keys,
        // but the cascade is not what this step relies on: an explicit delete is
        // what keeps the purge falsifiable, and a tenant's spend ledger is exactly
        // the kind of record that must not survive an erasure by accident.
        "DELETE FROM moa.experiment_resource_reservation WHERE storage_partition_id = $1",
        "DELETE FROM moa.experiment_trial WHERE storage_partition_id = $1",
        "DELETE FROM moa.experiment_run_artifact_revision WHERE storage_partition_id = $1",
        "DELETE FROM moa.experiment_run WHERE storage_partition_id = $1",
        // Fidelity studies restrict deletion of their immutable policy revision,
        // so evidence is removed before the certified policy registry row.
        "DELETE FROM moa.simulator_fidelity_study WHERE storage_partition_id = $1",
        "DELETE FROM moa.simulator_policy WHERE storage_partition_id = $1",
        "DELETE FROM analytics.score_run WHERE storage_partition_id = $1",
        // Learning-derived attribution goes before everything it references:
        // `learning_candidates`, the artifact revision/file rows, and the
        // sessions/experiences its source columns name. V000360 declares those
        // foreign keys WITHOUT `ON DELETE CASCADE` deliberately, so deleting a
        // parent first fails loudly rather than silently taking the child with
        // it — a cascade here would make these steps unfalsifiable, since
        // removing them would change nothing observable.
        "DELETE FROM moa.artifact_suite_contribution WHERE storage_partition_id = $1",
        "DELETE FROM moa.artifact_revision_contribution WHERE storage_partition_id = $1",
        "DELETE FROM learning_log_source WHERE storage_partition_id = $1",
        "DELETE FROM learning_candidate_decision WHERE storage_partition_id = $1",
        "DELETE FROM learning_candidate_source WHERE storage_partition_id = $1",
        "DELETE FROM moa.skill_embedding WHERE storage_partition_id = $1",
        "DELETE FROM moa.artifact_file WHERE storage_partition_id = $1",
        // Release control owns its exact-partition erasure seam. It deletes the
        // pointer before opening the append-only audit hatch, then removes both
        // audit tables without granting raw DML to the application role.
        "SELECT moa.purge_artifact_release_partition($1)",
        "UPDATE moa.artifact SET latest_revision_uid = NULL WHERE storage_partition_id = $1",
        "DELETE FROM moa.artifact_revision WHERE storage_partition_id = $1",
        "DELETE FROM moa.artifact WHERE storage_partition_id = $1",
        "DELETE FROM learning_candidates WHERE storage_partition_id = $1",
        "DELETE FROM experience_attributions WHERE storage_partition_id = $1",
        "DELETE FROM experience_records WHERE storage_partition_id = $1",
        "DELETE FROM learning_log WHERE storage_partition_id = $1",
        "DELETE FROM task_segments WHERE storage_partition_id = $1",
        // The durable lineage journal holds accepted-but-unwritten rows destined
        // for `turn_lineage`, so it is purged first: leaving a queued row behind
        // would let a writer drain it into a tenant that no longer exists.
        "DELETE FROM analytics.lineage_journal WHERE storage_partition_id = $1",
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
        "DELETE FROM moa.vector_sync_outbox WHERE storage_partition_id = $1",
        "DELETE FROM moa.embeddings WHERE storage_partition_id = $1",
        "DELETE FROM moa.graph_changelog WHERE storage_partition_id = $1",
        "DELETE FROM moa.edge_index WHERE storage_partition_id = $1",
        "DELETE FROM moa.node_index WHERE storage_partition_id = $1",
        "DELETE FROM moa.storage_partition_state WHERE storage_partition_id = $1",
    ];
    let purge_steps = tenant_deletes
        .into_iter()
        .map(|sql| purge_step(PurgeBinding::Tenant, sql))
        .chain(
            storage_deletes
                .into_iter()
                .map(|sql| purge_step(PurgeBinding::StoragePartition, sql)),
        )
        .collect::<Vec<_>>();
    for step in &purge_steps {
        let release_control = step.cleanup_sql == "SELECT moa.purge_artifact_release_partition($1)";
        if release_control {
            sqlx::query("SET LOCAL ROLE moa_app")
                .execute(&mut **tx)
                .await
                .map_err(|error| format!("assume application role for release purge: {error}"))?;
        }
        let query = sqlx::query(step.cleanup_sql);
        let query = match step.binding {
            PurgeBinding::Tenant => query.bind(tenant_id),
            PurgeBinding::StoragePartition => query.bind(storage_partition_id),
        };
        query
            .execute(&mut **tx)
            .await
            .map_err(|error| format!("purge {}: {error}", step.table))?;
        if release_control {
            sqlx::query("RESET ROLE")
                .execute(&mut **tx)
                .await
                .map_err(|error| format!("restore tenant purge role: {error}"))?;
            let role_restored: bool = sqlx::query_scalar("SELECT current_user = session_user")
                .fetch_one(&mut **tx)
                .await
                .map_err(|error| format!("verify tenant purge role restoration: {error}"))?;
            if !role_restored {
                return Err("release purge left the transaction in the application role".into());
            }
        }
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
    // `events` is append-only, enforced by a per-row BEFORE DELETE trigger that
    // raises unless `moa.events_maintenance` is set. Without this line a tenant
    // purge cannot delete any tenant that ever held a conversation — it fails
    // with P0001 `events table is append-only`. The purge suite could not see it
    // because a per-row trigger does not fire on zero rows and the fixture seeded
    // no events, so the purge passed in tests and failed against every real
    // tenant.
    //
    // `true` makes this transaction-local: it dies with the transaction, so the
    // hatch is open for exactly the statements below and for no one else. Erasing
    // a tenant's transcript on a right-to-erasure request is the one legitimate
    // reason to delete from an append-only log, and it is why the hatch exists —
    // but a purge that quietly disabled the guard process-wide would be a worse
    // bug than the one it fixes.
    sqlx::query("SELECT set_config('moa.events_maintenance', 'on', true)")
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("enable append-only maintenance for tenant purge: {error}"))?;
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
    redact_intentional_residue(tx, tenant_id).await?;
    assert_no_unapproved_residue(
        tx,
        tenant_id,
        storage_partition_id,
        &purge_steps,
        inverse_tuples,
    )
    .await?;
    Ok(())
}

fn purge_step(binding: PurgeBinding, cleanup_sql: &'static str) -> PurgeStep {
    let words = cleanup_sql.split_ascii_whitespace().collect::<Vec<_>>();
    let table = match words.first().copied() {
        Some("DELETE") => words.get(2).copied(),
        Some("UPDATE") => words.get(1).copied(),
        Some("SELECT") if cleanup_sql == "SELECT moa.purge_artifact_release_partition($1)" => {
            Some("moa.artifact_serving_pointer")
        }
        _ => None,
    }
    .unwrap_or("<invalid-purge-step>");
    PurgeStep {
        table,
        cleanup_sql,
        binding,
    }
}

async fn redact_intentional_residue(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE moa.kek SET wrapped_kek = NULL, destroyed_at = COALESCE(destroyed_at, NOW()) WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("crypto-shred tenant KEK rows: {error}"))?;
    sqlx::query(
        "UPDATE moa.legal_hold SET subject_id = NULL, reason = '[REDACTED]', placed_by = '[REDACTED]', released_by = '[REDACTED]' WHERE tenant_id = $1 AND released_at IS NOT NULL",
    )
    .bind(tenant_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("redact released legal-hold tombstones: {error}"))?;
    Ok(())
}

async fn assert_no_unapproved_residue(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    storage_partition_id: &str,
    steps: &[PurgeStep],
    inverse_tuples: &BTreeSet<InverseTuple>,
) -> Result<(), String> {
    assert_catalog_coverage(tx, steps).await?;
    for step in steps {
        if !step.cleanup_sql.starts_with("DELETE FROM ") {
            continue;
        }
        let residue_sql = step
            .cleanup_sql
            .replacen("DELETE FROM", "SELECT count(*) FROM", 1);
        let query = sqlx::query_scalar::<_, i64>(&residue_sql);
        let count = match step.binding {
            PurgeBinding::Tenant => query.bind(tenant_id),
            PurgeBinding::StoragePartition => query.bind(storage_partition_id),
        }
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| format!("residue check {}: {error}", step.table))?;
        if count != 0 {
            return Err(format!(
                "tenant purge left {count} unapproved rows in {}",
                step.table
            ));
        }
    }

    let invalid_kek: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.kek WHERE tenant_id = $1 AND (wrapped_kek IS NOT NULL OR destroyed_at IS NULL)",
    )
    .bind(tenant_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("KEK residue check: {error}"))?;
    let invalid_holds: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.legal_hold WHERE tenant_id = $1 AND (subject_id IS NOT NULL OR released_at IS NULL OR reason <> '[REDACTED]' OR placed_by <> '[REDACTED]' OR released_by <> '[REDACTED]')",
    )
    .bind(tenant_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("legal-hold residue check: {error}"))?;
    let (invalid_authz_outbox, missing_authz_outbox) =
        authz_outbox_residue(tx, tenant_id, inverse_tuples).await?;
    if invalid_kek != 0
        || invalid_holds != 0
        || invalid_authz_outbox != 0
        || missing_authz_outbox != 0
    {
        return Err(format!(
            "tenant purge left invalid intentional residue: kek={invalid_kek}, legal_hold={invalid_holds}, authz_outbox_invalid={invalid_authz_outbox}, authz_outbox_missing={missing_authz_outbox}"
        ));
    }
    Ok(())
}

async fn authz_outbox_residue(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
    inverse_tuples: &BTreeSet<InverseTuple>,
) -> Result<(usize, usize), String> {
    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
        r#"
        SELECT op, status, tuple_user, tuple_relation, tuple_object
        FROM authz_outbox
        WHERE tenant_id = $1
        "#,
    )
    .bind(tenant_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("authz outbox residue check: {error}"))?;
    let mut unmatched = inverse_tuples.clone();
    let mut invalid = 0;
    for (op, status, user, relation, object) in rows {
        let identity = (user, relation, object);
        if op != "delete" || status != "pending" || !unmatched.remove(&identity) {
            invalid += 1;
        }
    }
    Ok((invalid, unmatched.len()))
}

async fn assert_catalog_coverage(
    tx: &mut Transaction<'_, Postgres>,
    steps: &[PurgeStep],
) -> Result<(), String> {
    let mut registered = steps
        .iter()
        .map(|step| qualify_table(step.table))
        .collect::<BTreeSet<_>>();
    registered.extend(
        [
            "public.context_snapshots",
            "public.events",
            "public.sessions",
            "moa.destruction_operation_fence",
            "moa.kek",
            "moa.legal_hold",
            "moa.tenant_purge_operations",
            // Deleted together through `moa.purge_artifact_release_partition`;
            // the pointer table is represented by the executable purge step.
            "moa.artifact_release_eval_overlay",
            "moa.artifact_release_attempt",
            "moa.artifact_release_dispatch_outbox",
            "moa.artifact_release_case_pack",
            "moa.artifact_activation_audit",
            "moa.artifact_activation_attestation",
            "moa.artifact_release_candidate",
            "moa.artifact_release_policy",
            // Platform-only simulator certification authority. These tables
            // carry nullable scope columns solely for global RLS and can never
            // own tenant rows; purging them would destroy release authority for
            // every tenant.
            "moa.simulator_certification_mandate",
            "moa.simulator_certification_evidence_import",
            "public.authz_outbox",
            "public.tenants",
            // Owned by the durable credential vault, which removes them in the
            // strictly earlier `CredentialsPurged` stage through its own
            // forced-RLS, purge-gated path. They cannot be deleted from this
            // transaction: forced RLS admits only `moa_app` with the
            // transaction-local purge flag set, so a raw DELETE here would
            // silently affect nothing while appearing to cover them.
            "public.tenant_credential_versions",
            "public.tenant_credential_operations",
            // Swept in the same earlier stage: the claim table holds credential
            // references only, and its strict forced-RLS policy admits `moa_app`
            // alone, so this transaction's role cannot see or delete its rows.
            "moa.knowledge_link_claims",
        ]
        .into_iter()
        .map(str::to_string),
    );
    let catalog = sqlx::query_as::<_, (String, String)>(
        r#"
        SELECT namespace.nspname, table_row.relname
        FROM pg_catalog.pg_class AS table_row
        JOIN pg_catalog.pg_namespace AS namespace ON namespace.oid = table_row.relnamespace
        JOIN pg_catalog.pg_attribute AS column_row ON column_row.attrelid = table_row.oid
        WHERE table_row.relkind IN ('r', 'p')
          AND NOT table_row.relispartition
          AND namespace.nspname IN ('public', 'moa', 'analytics', 'pii_vault')
          AND column_row.attnum > 0
          AND NOT column_row.attisdropped
          AND column_row.attname IN ('tenant_id', 'storage_partition_id')
        GROUP BY namespace.nspname, table_row.relname
        ORDER BY namespace.nspname, table_row.relname
        "#,
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("load tenant-owned table catalog: {error}"))?;
    let uncovered = catalog
        .into_iter()
        .map(|(schema, table)| format!("{schema}.{table}"))
        .filter(|table| !registered.contains(table))
        .collect::<Vec<_>>();
    if !uncovered.is_empty() {
        return Err(format!(
            "tenant purge catalog has unregistered tenant-owned tables: {}",
            uncovered.join(", ")
        ));
    }
    Ok(())
}

fn qualify_table(table: &str) -> String {
    if table.contains('.') {
        table.to_string()
    } else {
        format!("public.{table}")
    }
}

async fn delete_tenant_record(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
) -> Result<(), String> {
    sqlx::query("DELETE FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("delete tenant row: {error}"))?;
    Ok(())
}

async fn assert_tenant_record_deleted(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: Uuid,
) -> Result<(), String> {
    let residue: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants WHERE id = $1")
        .bind(tenant_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| format!("tenant record residue check: {error}"))?;
    if residue == 0 {
        Ok(())
    } else {
        Err(format!(
            "tenant purge left {residue} unapproved rows in tenants"
        ))
    }
}

/// Loads one keyset page of node uids owned by the tenant, ordered by uid.
///
/// The external-vector purge stage walks the tenant's graph nodes in stable uid
/// order so remote deletes run without holding a PostgreSQL connection. Returns
/// at most `limit` uids strictly greater than `after_uid`, or all of the
/// tenant's uids from the start when `after_uid` is `None`.
pub(super) async fn load_external_vector_uid_page(
    pool: &sqlx::PgPool,
    tenant_id: moa_core::types::identifiers::TenantId,
    after_uid: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Uuid>, String> {
    sqlx::query_scalar(
        r#"
        SELECT uid
        FROM moa.node_index
        WHERE tenant_id = $1
          AND ($2::UUID IS NULL OR uid > $2)
        ORDER BY uid
        LIMIT $3
        "#,
    )
    .bind(tenant_id.0)
    .bind(after_uid)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load tenant vector ids: {error}"))
}

#[cfg(test)]
mod tests {
    use moa_artifacts::document::ArtifactDocument;
    use moa_artifacts::registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile};
    use moa_artifacts::release::TenantScope;
    use moa_authz::FgaConfig;
    use moa_core::types::action_policy::ActionRuleScope;
    use moa_core::types::identifiers::TenantId;
    use moa_test_support::postgres::bootstrap_test_db;
    use serde_json::json;

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

    async fn start_test_destruction(pool: &sqlx::PgPool, tenant_id: Uuid, operation_id: &str) {
        moa_memory_pii::legal_hold::start_destruction(
            pool,
            tenant_id.into(),
            &[],
            operation_id,
            "tenant.purge",
        )
        .await
        .expect("start durable tenant destruction fence");
    }

    async fn seed_activated_skill(
        pool: &sqlx::PgPool,
        tenant_id: Uuid,
        label: &str,
    ) -> (Uuid, Uuid, Uuid) {
        let tenant_id = TenantId::from(tenant_id);
        let scope = ActionRuleScope::Tenant { tenant_id };
        let name = format!("purge-{label}-{}", Uuid::now_v7());
        let document: ArtifactDocument = serde_json::from_value(json!({
            "api_version": "moa.artifact/v1",
            "kind": "skill",
            "metadata": {
                "name": name,
                "description": "tenant purge activation fixture",
                "tags": []
            },
            "definition": {
                "type": "skill",
                "spec": {
                    "instructions": { "path": "SKILL.md" },
                    "inputs": { "type": "object" },
                    "outputs": { "type": "object" }
                }
            }
        }))
        .expect("purge activation fixture document should decode");
        let source = document
            .to_yaml()
            .expect("purge activation fixture should serialize");
        let registry = ArtifactRegistry::new(pool.clone());
        let draft = registry
            .create_draft(
                &scope,
                NewArtifactDraft {
                    document: &document,
                    source_format: "yaml",
                    source_text: source.as_bytes(),
                    files: &[NewArtifactFile::new(
                        "SKILL.md",
                        b"# Purge fixture\n".to_vec(),
                    )],
                },
            )
            .await
            .expect("purge activation fixture draft should persist");
        let activated = moa_artifacts::test_fixtures::activate_revision(
            pool,
            TenantScope::new(tenant_id),
            moa_artifacts::release::ActivationTarget::SkillVisibility {
                artifact_uid: draft.artifact_uid,
            },
            draft.revision_uid,
        )
        .await
        .expect("purge activation fixture should activate through the real gate");
        let serving = registry
            .load_serving(&scope, moa_artifacts::document::ArtifactKind::Skill, &name)
            .await
            .expect("purge activation fixture serving lookup should succeed")
            .expect("purge activation fixture should serve");
        assert_eq!(serving.revision_uid, draft.revision_uid);
        (draft.artifact_uid, draft.revision_uid, activated.audit_uid)
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
        start_test_destruction(pool, tenant_id, &operation_id).await;

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
    async fn relational_purge_removes_activation_history_without_opening_neighbor_audit_db() {
        // Pins: append-only activation evidence is deletable only by the
        // transaction-local purge hatch for the exact tenant partition. The release
        // rows are explicitly removed before their artifact parents, and an
        // adjacent tenant's candidate, pointer, and audit survive unchanged.
        let test_db = bootstrap_test_db()
            .await
            .expect("bootstrap activation-history purge db");
        let pool = test_db.store().pool();
        let purged_tenant = Uuid::new_v4();
        let neighbor_tenant = Uuid::new_v4();
        seed_tenant(pool, purged_tenant, Uuid::new_v4()).await;
        seed_tenant(pool, neighbor_tenant, Uuid::new_v4()).await;
        let (purged_artifact, purged_revision, purged_audit) =
            seed_activated_skill(pool, purged_tenant, "target").await;
        let (neighbor_artifact, neighbor_revision, neighbor_audit) =
            seed_activated_skill(pool, neighbor_tenant, "neighbor").await;

        let ordinary_delete =
            sqlx::query("DELETE FROM moa.artifact_activation_audit WHERE audit_uid = $1")
                .bind(purged_audit)
                .execute(pool)
                .await;
        assert!(
            ordinary_delete.is_err(),
            "normal SQL must not bypass the append-only activation-audit guard"
        );

        let mut wrong_partition = pool
            .begin()
            .await
            .expect("begin wrong-partition maintenance attempt");
        sqlx::query(
            "SELECT set_config(\
                'moa.artifact_activation_audit_maintenance_partition', $1, true\
            )",
        )
        .bind(purged_tenant.to_string())
        .execute(&mut *wrong_partition)
        .await
        .expect("set exact target partition for maintenance attempt");
        let neighbor_delete =
            sqlx::query("DELETE FROM moa.artifact_activation_audit WHERE audit_uid = $1")
                .bind(neighbor_audit)
                .execute(&mut *wrong_partition)
                .await;
        assert!(
            neighbor_delete.is_err(),
            "a maintenance hatch for one partition must not authorize a neighbor delete"
        );
        wrong_partition
            .rollback()
            .await
            .expect("rollback rejected maintenance attempt");

        let operation_id = format!("tenant-purge-{purged_tenant}");
        start_test_destruction(pool, purged_tenant, &operation_id).await;
        assert_eq!(
            purge_relational(pool, &offline_fga(), purged_tenant, &operation_id)
                .await
                .expect("purge should erase activated release state"),
            RelationalPurgeOutcome::Committed
        );

        for (table, id_column, id) in [
            ("moa.artifact_activation_audit", "audit_uid", purged_audit),
            (
                "moa.artifact_release_candidate",
                "revision_uid",
                purged_revision,
            ),
            (
                "moa.artifact_serving_pointer",
                "artifact_uid",
                purged_artifact,
            ),
            ("moa.artifact", "artifact_uid", purged_artifact),
        ] {
            let count: i64 = sqlx::query_scalar(&format!(
                "SELECT count(*) FROM {table} WHERE {id_column} = $1"
            ))
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("count purged release row");
            assert_eq!(count, 0, "purge left a row in {table}");
        }

        for (table, id_column, id) in [
            ("moa.artifact_activation_audit", "audit_uid", neighbor_audit),
            (
                "moa.artifact_release_candidate",
                "revision_uid",
                neighbor_revision,
            ),
            (
                "moa.artifact_serving_pointer",
                "artifact_uid",
                neighbor_artifact,
            ),
            ("moa.artifact", "artifact_uid", neighbor_artifact),
        ] {
            let count: i64 = sqlx::query_scalar(&format!(
                "SELECT count(*) FROM {table} WHERE {id_column} = $1"
            ))
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("count neighboring release row");
            assert_eq!(count, 1, "purge changed neighbor state in {table}");
        }
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
        start_test_destruction(pool, tenant_id, &operation_id).await;
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
        assert!(
            error.contains("scripted tenant purge rollback"),
            "the tenant delete must be what failed, so the rollback this test \
             pins is the one a real relational failure takes; a different error \
             means the purge aborted before reaching the scripted trigger: {error}"
        );

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
