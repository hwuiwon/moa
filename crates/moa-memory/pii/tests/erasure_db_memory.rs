//! DB integration coverage for contact-scoped privacy erasure.

use std::sync::Arc;

use moa_core::types::security::SensitivityClass;
use moa_core::{
    types::agent::SYSTEM_DEFAULT_AGENT_REVISION_UID,
    types::contact::ContactId,
    types::identifiers::StoragePartitionId,
    types::identifiers::TenantId,
    types::memory::{InformationBarrierId, RlsContext},
};
use moa_db::ScopedConn;
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PostgresGraphStore,
};
use moa_memory_pii::erasure::{
    EraseCandidate, GraphErasureAudit, begin_app_scoped_tx, delete_subject_digests,
    delete_subject_retrieval_lineage, enumerate_erase_candidates, hard_purge_erase_candidates,
};
use moa_memory_pii::learning_erasure::{
    ErasureDisposition, ErasureRecordKind, ErasureSubjects, RecordDecision,
    enumerate_learning_closure, erase_learning_closure, record_decisions,
};
use moa_memory_pii::legal_hold::{
    LegalHoldError, begin_destruction_stage_guard, lock_tenant_and_subjects, place_hold,
    release_hold, start_destruction,
};
use moa_session::testing;
use serde_json::json;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

fn test_kms() -> Arc<dyn moa_crypto::KeyManagementProvider> {
    Arc::new(moa_crypto::LocalKmsProvider::new())
}

#[derive(Debug, PartialEq, Eq)]
struct RoleScopeSnapshot {
    backend_pid: i32,
    current_user: String,
    session_user: String,
    tenant_id: Option<String>,
    storage_partition_id: Option<String>,
    contact_id: Option<String>,
    control_plane: Option<String>,
    cleared_barriers: Option<String>,
}

async fn role_scope_snapshot(conn: &mut PgConnection) -> RoleScopeSnapshot {
    let row = sqlx::query(
        r#"
        SELECT
            pg_backend_pid() AS backend_pid,
            current_user::text AS current_user,
            session_user::text AS session_user,
            current_setting('moa.tenant_id', true) AS tenant_id,
            current_setting('moa.storage_partition_id', true) AS storage_partition_id,
            current_setting('moa.contact_id', true) AS contact_id,
            current_setting('moa.control_plane', true) AS control_plane,
            current_setting('moa.cleared_barriers', true) AS cleared_barriers
        "#,
    )
    .fetch_one(conn)
    .await
    .expect("inspect guarded role and RLS scope");

    RoleScopeSnapshot {
        backend_pid: row.get("backend_pid"),
        current_user: row.get("current_user"),
        session_user: row.get("session_user"),
        tenant_id: row.get("tenant_id"),
        storage_partition_id: row.get("storage_partition_id"),
        contact_id: row.get("contact_id"),
        control_plane: row.get("control_plane"),
        cleared_barriers: row.get("cleared_barriers"),
    }
}

#[tokio::test]
async fn destruction_guard_role_scope_transitions_preserve_backend_and_reset_contact_scope_db_memory()
 {
    // Pins: one active destruction guard owns one transaction while typed
    // role transitions move moa_app -> owner -> moa_app contact scope and reset
    // every RLS GUC without checking the connection back into the pool.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let operation_id = format!("privacy-role-scope-{}", Uuid::now_v7());
    let expected_storage_partition = StoragePartitionId::for_tenant(tenant_id).to_string();

    start_destruction(
        session_store.pool(),
        tenant_id,
        &[contact_id.0],
        &operation_id,
        "privacy.erase",
    )
    .await
    .expect("start durable subject destruction");
    let active_fence_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.destruction_operation_fence \
         WHERE tenant_id = $1 AND subject_id = $2 \
           AND operation_id = $3 AND status = 'in_progress'",
    )
    .bind(tenant_id.0)
    .bind(contact_id.0)
    .bind(&operation_id)
    .fetch_one(session_store.pool())
    .await
    .expect("count active destruction fence");
    assert_eq!(
        active_fence_count, 1,
        "the tenant-purge destruction fence is active"
    );

    let mut guard = begin_destruction_stage_guard(
        session_store.pool(),
        tenant_id,
        &[contact_id.0],
        &operation_id,
    )
    .await
    .expect("acquire active destruction stage guard");
    let initial = role_scope_snapshot(guard.connection()).await;
    assert_eq!(initial.current_user, "moa_app");
    assert_eq!(
        initial.tenant_id.as_deref(),
        Some(tenant_id.to_string().as_str())
    );
    assert_eq!(
        initial.storage_partition_id.as_deref(),
        Some(expected_storage_partition.as_str())
    );
    assert_eq!(initial.contact_id.as_deref(), Some(""));
    assert_eq!(initial.control_plane.as_deref(), Some("false"));
    assert_eq!(initial.cleared_barriers.as_deref(), Some(""));

    guard
        .assume_owner_role()
        .await
        .expect("restore canonical owner role");
    let owner = role_scope_snapshot(guard.connection()).await;
    assert_eq!(owner.backend_pid, initial.backend_pid);
    assert_eq!(owner.current_user, owner.session_user);
    assert_eq!(owner.tenant_id, initial.tenant_id);
    assert_eq!(owner.storage_partition_id, initial.storage_partition_id);
    assert_eq!(owner.contact_id, initial.contact_id);
    assert_eq!(owner.control_plane, initial.control_plane);
    assert_eq!(owner.cleared_barriers, initial.cleared_barriers);

    sqlx::query(
        r#"
        SELECT
            set_config('moa.tenant_id', '00000000-0000-0000-0000-000000000000', true),
            set_config('moa.storage_partition_id', 'stale-partition', true),
            set_config('moa.contact_id', '00000000-0000-0000-0000-000000000000', true),
            set_config('moa.control_plane', 'true', true),
            set_config('moa.cleared_barriers', 'stale-barrier', true)
        "#,
    )
    .execute(guard.connection())
    .await
    .expect("poison every prior RLS GUC before the typed transition");
    guard
        .assume_app_contact_scope(tenant_id, contact_id)
        .await
        .expect("install typed app contact scope");
    let contact = role_scope_snapshot(guard.connection()).await;
    assert_eq!(contact.backend_pid, initial.backend_pid);
    assert_eq!(contact.session_user, initial.session_user);
    assert_eq!(contact.current_user, "moa_app");
    assert_eq!(
        contact.tenant_id.as_deref(),
        Some(tenant_id.to_string().as_str())
    );
    assert_eq!(
        contact.storage_partition_id.as_deref(),
        Some(expected_storage_partition.as_str())
    );
    assert_eq!(
        contact.contact_id.as_deref(),
        Some(contact_id.to_string().as_str())
    );
    assert_eq!(contact.control_plane.as_deref(), Some("false"));
    assert_eq!(contact.cleared_barriers.as_deref(), Some(""));

    guard.finish().await.expect("commit guarded transaction");
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated Postgres store");
}

#[tokio::test]
async fn learning_erasure_records_1001_decisions_in_one_idempotent_batch_db_memory() {
    // Pins: the decision ledger accepts a batch larger than PostgreSQL's common
    // 1,000-row work chunk through the set-based UNNEST path, and replay inserts
    // no duplicate audit rows.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::new();
    let subject_user_id = Uuid::now_v7().to_string();
    let decisions = (0..1_001)
        .map(|_| RecordDecision {
            kind: ErasureRecordKind::LearningCandidate,
            record_id: Uuid::now_v7().to_string(),
            disposition: ErasureDisposition::Erased,
            applied: true,
            reason: Some("set-based erasure fixture".to_string()),
        })
        .collect::<Vec<_>>();

    let inserted = record_decisions(
        session_store.pool(),
        tenant_id,
        &subject_user_id,
        "set-based-1001",
        &decisions,
    )
    .await
    .expect("record 1001 erasure decisions");
    let replayed = record_decisions(
        session_store.pool(),
        tenant_id,
        &subject_user_id,
        "set-based-1001",
        &decisions,
    )
    .await
    .expect("replay 1001 erasure decisions");
    let persisted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.privacy_erasure_record_decision
         WHERE tenant_id = $1 AND subject_user_id = $2 AND attempt_id = $3",
    )
    .bind(tenant_id.0)
    .bind(&subject_user_id)
    .bind("set-based-1001")
    .fetch_one(session_store.pool())
    .await
    .expect("count bulk decision rows");

    assert_eq!(inserted, 1_001);
    assert_eq!(replayed, 0);
    assert_eq!(persisted, 1_001);

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated Postgres store");
}

#[tokio::test]
async fn legal_hold_and_subject_destruction_are_linearizable_across_pools_db_memory() {
    // Pins: hold-first blocks destruction, while destruction-first makes a
    // separate pool wait on the tenant lock and then refuse the hold after the
    // durable fence commits.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::new();
    let subject_id = Uuid::now_v7();
    let second_pool = PgPool::connect(&database_url)
        .await
        .expect("connect independent pool");

    let hold = place_hold(
        session_store.pool(),
        tenant_id,
        Some(subject_id),
        "preservation order",
        "legal-admin",
    )
    .await
    .expect("place hold first");
    let duplicate = place_hold(
        &second_pool,
        tenant_id,
        Some(subject_id),
        "duplicate preservation order",
        "other-admin",
    )
    .await
    .expect_err("only one active hold may cover one subject");
    assert!(matches!(duplicate, LegalHoldError::Sqlx(_)));
    let blocked = start_destruction(
        &second_pool,
        tenant_id,
        &[subject_id],
        "erase-hold-first",
        "privacy.erase",
    )
    .await
    .expect_err("hold-first must block destruction");
    assert!(matches!(blocked, LegalHoldError::ActiveHold));
    assert!(
        release_hold(session_store.pool(), tenant_id, hold.id, "legal-admin")
            .await
            .expect("release hold")
    );

    let tenant_hold = place_hold(
        session_store.pool(),
        tenant_id,
        None,
        "tenant preservation order",
        "legal-admin",
    )
    .await
    .expect("place one tenant-wide hold");
    let duplicate_tenant_hold = place_hold(
        &second_pool,
        tenant_id,
        None,
        "duplicate tenant preservation order",
        "other-admin",
    )
    .await
    .expect_err("NULLS NOT DISTINCT permits only one active tenant-wide hold");
    assert!(matches!(duplicate_tenant_hold, LegalHoldError::Sqlx(_)));
    assert!(
        release_hold(
            session_store.pool(),
            tenant_id,
            tenant_hold.id,
            "legal-admin"
        )
        .await
        .expect("release tenant-wide hold")
    );

    let mut plan_tx = session_store
        .pool()
        .begin()
        .await
        .expect("begin fence index plan check");
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(plan_tx.as_mut())
        .await
        .expect("force indexable fence plan");
    let fence_plan = sqlx::query_scalar::<_, String>(
        "EXPLAIN (FORMAT TEXT)
         SELECT 1 FROM moa.destruction_operation_fence
         WHERE tenant_id = $1 AND status = 'in_progress'",
    )
    .bind(tenant_id.0)
    .fetch_all(plan_tx.as_mut())
    .await
    .expect("explain typed in-progress fence lookup");
    assert!(
        fence_plan
            .iter()
            .any(|line| line.contains("destruction_fence_in_progress_tenant")),
        "typed fence lookup must use its partial tenant index: {fence_plan:?}"
    );
    plan_tx.rollback().await.expect("finish fence plan check");

    let mut fence_tx =
        ScopedConn::begin_as_app(session_store.pool(), &RlsContext::tenant(tenant_id), true)
            .await
            .expect("begin fence transaction");
    lock_tenant_and_subjects(fence_tx.as_mut(), tenant_id.0, &[subject_id])
        .await
        .expect("take canonical destruction locks");
    let waiting_pool = second_pool.clone();
    let waiter = tokio::spawn(async move {
        place_hold(
            &waiting_pool,
            tenant_id,
            Some(subject_id),
            "too late",
            "legal-admin",
        )
        .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            while !waiter.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err(),
        "hold placement must wait for the destruction lock"
    );
    sqlx::query(
        "INSERT INTO moa.destruction_operation_fence (tenant_id, subject_id, operation_id, operation_kind) VALUES ($1, $2, 'erase-destruction-first', 'privacy.erase')",
    )
    .bind(tenant_id.0)
    .bind(subject_id)
    .execute(fence_tx.as_mut())
    .await
    .expect("persist destruction fence while locked");
    fence_tx.commit().await.expect("commit destruction fence");
    let refused = waiter
        .await
        .expect("hold waiter joins")
        .expect_err("destruction-first must refuse later hold");
    assert!(matches!(refused, LegalHoldError::DestructionStarted));

    second_pool.close().await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn learning_erasure_removes_experiences_and_recursive_candidate_dependents_db_memory() {
    // Pins: erasure removes experience/attribution rows and walks both
    // promotion-candidate and artifact-revision dependencies before restrictive
    // foreign keys can roll back the transaction. One durable destruction guard
    // also owns the mutations, typed ledger-role transition, rollback, and retry.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool();
    let tenant_id = TenantId::new();
    let tenant_key = tenant_id.to_string();
    let subject_id = Uuid::now_v7();
    let subject_user_id = subject_id.to_string();
    let session_id = Uuid::now_v7();
    let segment_id = Uuid::now_v7();
    let experience_id = Uuid::now_v7();
    let attribution_id = Uuid::now_v7();
    let source_candidate_id = Uuid::now_v7();
    let rollback_candidate_id = Uuid::now_v7();
    let revision_candidate_id = Uuid::now_v7();
    let candidate_ids = vec![
        source_candidate_id,
        rollback_candidate_id,
        revision_candidate_id,
    ];
    let artifact_uid = Uuid::now_v7();
    let revision_uids = (0..1_001).map(|_| Uuid::now_v7()).collect::<Vec<_>>();

    let mut session_tx = pool.begin().await.expect("begin session fixture");
    sqlx::query(
        "INSERT INTO sessions
            (id, storage_partition_id, user_id, tenant_id, model, status)
         VALUES ($1, $2, $3, $4, 'test-model', 'completed')",
    )
    .bind(session_id)
    .bind(&tenant_key)
    .bind(&subject_user_id)
    .bind(tenant_id.0)
    .execute(session_tx.as_mut())
    .await
    .expect("seed subject session");
    sqlx::query(
        "INSERT INTO session_agent_context
            (session_id, storage_partition_id, user_id, tenant_id,
             agent_definition_ref, agent_revision_uid, policy_hash,
             display_name, policy_snapshot)
         VALUES ($1, $2, $3, $4, 'agent://privacy-fixture', $5,
                 'privacy-fixture-hash', 'Privacy fixture', '{}'::JSONB)",
    )
    .bind(session_id)
    .bind(&tenant_key)
    .bind(&subject_user_id)
    .bind(tenant_id.0)
    .bind(SYSTEM_DEFAULT_AGENT_REVISION_UID)
    .execute(session_tx.as_mut())
    .await
    .expect("seed session agent context");
    session_tx.commit().await.expect("commit session fixture");
    sqlx::query(
        "INSERT INTO task_segments
            (id, session_id, storage_partition_id, user_id, tenant_id,
             segment_index, started_at, ended_at, outcome, tools_used,
             skills_activated, turn_count, token_cost)
         VALUES ($1, $2, $3, $4, $3, 0, now(), now(), 'resolved',
                 '{}', '{}', 1, 0)",
    )
    .bind(segment_id)
    .bind(session_id)
    .bind(&tenant_key)
    .bind(&subject_user_id)
    .execute(pool)
    .await
    .expect("seed task segment");
    sqlx::query(
        "INSERT INTO experience_records
            (id, segment_id, session_id, storage_partition_id, user_id, tenant_id,
             task_summary, task_fingerprint, task_fingerprint_payload, task_facets,
             outcome, confidence, assessment_policy_version, extraction_policy_version)
         VALUES ($1, $2, $3, $4, $5, $4, 'redacted subject task', 'task-fingerprint',
                 '{}'::JSONB, '{}'::JSONB, 'resolved', 0.9, 'assessment-v1', 'extract-v1')",
    )
    .bind(experience_id)
    .bind(segment_id)
    .bind(session_id)
    .bind(&tenant_key)
    .bind(&subject_user_id)
    .execute(pool)
    .await
    .expect("seed experience");
    sqlx::query(
        "INSERT INTO experience_attributions
            (id, experience_id, tenant_id, storage_partition_id, user_id,
             subject_type, subject_id, effect, confidence)
         VALUES ($1, $2, $3, $3, $4, 'skill', 'subject-skill', 'helpful', 0.9)",
    )
    .bind(attribution_id)
    .bind(experience_id)
    .bind(&tenant_key)
    .bind(&subject_user_id)
    .execute(pool)
    .await
    .expect("seed experience attribution");
    sqlx::query(
        "INSERT INTO moa.artifact
            (artifact_uid, tenant_id, storage_partition_id, user_id, kind, name, description)
         VALUES ($1, $2, $3, $4, 'skill', $5, 'privacy erasure fixture')",
    )
    .bind(artifact_uid)
    .bind(tenant_id.0)
    .bind(&tenant_key)
    .bind(&subject_user_id)
    .bind(format!("privacy-erasure-{artifact_uid}"))
    .execute(pool)
    .await
    .expect("seed artifact");
    sqlx::query(
        "INSERT INTO moa.artifact_revision
            (revision_uid, artifact_uid, tenant_id, storage_partition_id, user_id,
             definition, canonical_hash, source_format, source_text, status,
             validation_report, version, published_at)
         SELECT revision_uid, $2, $3, $4, $5,
                jsonb_build_object('kind', 'skill', 'ordinal', ordinal),
                digest(revision_uid::TEXT, 'sha256'), 'json',
                convert_to(jsonb_build_object('ordinal', ordinal)::TEXT, 'UTF8'),
                'ready', '{}'::JSONB, ordinal::INTEGER, now()
         FROM unnest($1::UUID[]) WITH ORDINALITY AS revision(revision_uid, ordinal)",
    )
    .bind(&revision_uids)
    .bind(artifact_uid)
    .bind(tenant_id.0)
    .bind(&tenant_key)
    .bind(&subject_user_id)
    .execute(pool)
    .await
    .expect("seed 1001 artifact revisions");
    sqlx::query(
        "INSERT INTO moa.artifact_file
            (file_uid, artifact_uid, revision_uid, tenant_id, storage_partition_id,
             user_id, path, content, content_sha256, file_size_bytes)
         SELECT gen_random_uuid(), $2, revision_uid, $3, $4, $5,
                format('fixture-%s.txt', ordinal), decode('01', 'hex'),
                digest(revision_uid::TEXT, 'sha256'), 1
         FROM unnest($1::UUID[]) WITH ORDINALITY AS revision(revision_uid, ordinal)",
    )
    .bind(&revision_uids)
    .bind(artifact_uid)
    .bind(tenant_id.0)
    .bind(&tenant_key)
    .bind(&subject_user_id)
    .execute(pool)
    .await
    .expect("seed 1001 attributable artifact files");

    let mut tx = pool.begin().await.expect("begin learning fixture");
    sqlx::query(
        "INSERT INTO learning_candidates
            (id, tenant_id, storage_partition_id, user_id, candidate_type,
             proposal_kind, status, payload, risk_class)
         SELECT id, $2, $2, $3, 'skill', proposal_kind, 'proposed', '{}'::JSONB, 'low'
         FROM UNNEST($1::UUID[], ARRAY['skill_draft', 'skill_rollback', 'skill_draft'])
              AS candidate(id, proposal_kind)",
    )
    .bind(&candidate_ids)
    .bind(&tenant_key)
    .bind(&subject_user_id)
    .execute(tx.as_mut())
    .await
    .expect("seed learning candidates");
    sqlx::query(
        "INSERT INTO learning_candidate_source
            (id, candidate_id, tenant_id, storage_partition_id, user_id, source_kind,
             attribution_id, promotion_candidate_id, artifact_revision_uid)
         VALUES
            ($1, $2, $8, $8, $9, 'attribution', $3, NULL, NULL),
            ($4, $5, $8, $8, $9, 'promotion_candidate', NULL, $2, NULL),
            ($6, $7, $8, $8, $9, 'artifact_revision', NULL, NULL, $10)",
    )
    .bind(Uuid::now_v7())
    .bind(source_candidate_id)
    .bind(attribution_id)
    .bind(Uuid::now_v7())
    .bind(rollback_candidate_id)
    .bind(Uuid::now_v7())
    .bind(revision_candidate_id)
    .bind(&tenant_key)
    .bind(&subject_user_id)
    .bind(revision_uids[0])
    .execute(tx.as_mut())
    .await
    .expect("seed recursive candidate sources");
    sqlx::query(
        "INSERT INTO moa.artifact_revision_contribution
            (contribution_uid, storage_partition_id, user_id, revision_uid,
             candidate_id, tenant_id, contribution_kind)
         SELECT gen_random_uuid(), $2, $3, revision_uid,
                $4, $2, 'generated_definition'
         FROM unnest($1::UUID[]) AS revision(revision_uid)",
    )
    .bind(&revision_uids)
    .bind(&tenant_key)
    .bind(&subject_user_id)
    .bind(source_candidate_id)
    .execute(tx.as_mut())
    .await
    .expect("seed revision contribution");
    tx.commit().await.expect("commit learning fixture");

    // The completeness contract is deferred so an owner and its sources can be
    // written in separate statements, but it covers both directions. Removing
    // or moving the final source must fail at the constraint boundary.
    let mut delete_last_source = pool.begin().await.expect("begin last-source delete");
    sqlx::query("DELETE FROM learning_candidate_source WHERE candidate_id = $1")
        .bind(source_candidate_id)
        .execute(delete_last_source.as_mut())
        .await
        .expect("stage last-source delete");
    let delete_error = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(delete_last_source.as_mut())
        .await
        .expect_err("deleting the final candidate source must fail at constraint time");
    assert_eq!(
        delete_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    delete_last_source
        .rollback()
        .await
        .expect("roll back refused last-source delete");

    let mut move_last_source = pool.begin().await.expect("begin last-source move");
    sqlx::query("UPDATE learning_candidate_source SET candidate_id = $1 WHERE candidate_id = $2")
        .bind(rollback_candidate_id)
        .bind(source_candidate_id)
        .execute(move_last_source.as_mut())
        .await
        .expect("stage last-source move");
    let move_error = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(move_last_source.as_mut())
        .await
        .expect_err("moving the final candidate source must fail at constraint time");
    assert_eq!(
        move_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    move_last_source
        .rollback()
        .await
        .expect("roll back refused last-source move");

    let mut delete_owner = pool.begin().await.expect("begin owner/source delete");
    sqlx::query("DELETE FROM learning_candidate_source WHERE candidate_id = $1")
        .bind(rollback_candidate_id)
        .execute(delete_owner.as_mut())
        .await
        .expect("delete source with owner");
    sqlx::query("DELETE FROM learning_candidates WHERE id = $1")
        .bind(rollback_candidate_id)
        .execute(delete_owner.as_mut())
        .await
        .expect("delete source owner");
    sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(delete_owner.as_mut())
        .await
        .expect("source and owner may disappear together");
    delete_owner
        .rollback()
        .await
        .expect("restore source-completeness fixture");

    let learning_ids = [Uuid::now_v7(), Uuid::now_v7()];
    let mut log_fixture = pool.begin().await.expect("begin learning-log fixture");
    sqlx::query(
        "INSERT INTO learning_log
            (id, tenant_id, storage_partition_id, user_id, learning_type, target_id, payload)
         SELECT id, $2, $2, $3, 'skill', id::TEXT, '{}'::JSONB
         FROM unnest($1::UUID[]) AS entry(id)",
    )
    .bind(&learning_ids[..])
    .bind(&tenant_key)
    .bind(&subject_user_id)
    .execute(log_fixture.as_mut())
    .await
    .expect("seed learning-log owners");
    sqlx::query(
        "INSERT INTO learning_log_source
            (id, learning_id, tenant_id, storage_partition_id, user_id,
             source_kind, candidate_id)
         VALUES
            ($1, $2, $7, $7, $8, 'candidate', $3),
            ($4, $5, $7, $7, $8, 'candidate', $6)",
    )
    .bind(Uuid::now_v7())
    .bind(learning_ids[0])
    .bind(source_candidate_id)
    .bind(Uuid::now_v7())
    .bind(learning_ids[1])
    .bind(rollback_candidate_id)
    .bind(&tenant_key)
    .bind(&subject_user_id)
    .execute(log_fixture.as_mut())
    .await
    .expect("seed learning-log sources");
    log_fixture
        .commit()
        .await
        .expect("commit learning-log fixture");

    let mut anchor_plan_tx = pool
        .begin()
        .await
        .expect("begin privacy anchor plan checks");
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(anchor_plan_tx.as_mut())
        .await
        .expect("force privacy anchor index plans");
    let candidate_anchor_plan = sqlx::query_scalar::<_, String>(
        "EXPLAIN (FORMAT TEXT)
         SELECT candidate_id FROM learning_candidate_source
         WHERE tenant_id = $1 AND source_kind = 'attribution' AND privacy_anchor_id = $2",
    )
    .bind(&tenant_key)
    .bind(attribution_id)
    .fetch_all(anchor_plan_tx.as_mut())
    .await
    .expect("explain candidate privacy-anchor lookup");
    assert!(
        candidate_anchor_plan
            .iter()
            .any(|line| line.contains("learning_candidate_source_privacy_anchor_idx")),
        "candidate closure must use the exact privacy-anchor index: {candidate_anchor_plan:?}"
    );
    let log_anchor_plan = sqlx::query_scalar::<_, String>(
        "EXPLAIN (FORMAT TEXT)
         SELECT learning_id FROM learning_log_source
         WHERE tenant_id = $1 AND source_kind = 'candidate' AND privacy_anchor_id = $2",
    )
    .bind(&tenant_key)
    .bind(source_candidate_id)
    .fetch_all(anchor_plan_tx.as_mut())
    .await
    .expect("explain learning-log privacy-anchor lookup");
    assert!(
        log_anchor_plan
            .iter()
            .any(|line| line.contains("learning_log_source_privacy_anchor_idx")),
        "learning-log closure must use the exact privacy-anchor index: {log_anchor_plan:?}"
    );
    anchor_plan_tx
        .rollback()
        .await
        .expect("finish privacy anchor plan checks");

    let mut delete_last_log_source = pool.begin().await.expect("begin last-log-source delete");
    sqlx::query("DELETE FROM learning_log_source WHERE learning_id = $1")
        .bind(learning_ids[0])
        .execute(delete_last_log_source.as_mut())
        .await
        .expect("stage last learning-log source delete");
    let delete_log_error = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(delete_last_log_source.as_mut())
        .await
        .expect_err("deleting the final learning-log source must fail at constraint time");
    assert_eq!(
        delete_log_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    delete_last_log_source
        .rollback()
        .await
        .expect("roll back refused last learning-log source delete");

    let mut move_last_log_source = pool.begin().await.expect("begin last-log-source move");
    sqlx::query("UPDATE learning_log_source SET learning_id = $1 WHERE learning_id = $2")
        .bind(learning_ids[1])
        .bind(learning_ids[0])
        .execute(move_last_log_source.as_mut())
        .await
        .expect("stage last learning-log source move");
    let move_log_error = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(move_last_log_source.as_mut())
        .await
        .expect_err("moving the final learning-log source must fail at constraint time");
    assert_eq!(
        move_log_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    move_last_log_source
        .rollback()
        .await
        .expect("roll back refused last learning-log source move");

    let mut remove_log_fixture = pool.begin().await.expect("begin log fixture cleanup");
    sqlx::query("DELETE FROM learning_log_source WHERE learning_id = ANY($1)")
        .bind(&learning_ids[..])
        .execute(remove_log_fixture.as_mut())
        .await
        .expect("delete sources with their learning-log owners");
    sqlx::query("DELETE FROM learning_log WHERE id = ANY($1)")
        .bind(&learning_ids[..])
        .execute(remove_log_fixture.as_mut())
        .await
        .expect("delete learning-log owners with their sources");
    sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(remove_log_fixture.as_mut())
        .await
        .expect("learning-log sources and owners may disappear together");
    remove_log_fixture
        .commit()
        .await
        .expect("commit learning-log fixture cleanup");

    let closure = enumerate_learning_closure(
        pool,
        tenant_id,
        &ErasureSubjects {
            user_ids: vec![subject_user_id.clone()],
            contact_ids: Vec::new(),
        },
    )
    .await
    .expect("enumerate recursive learning closure");
    assert_eq!(closure.experience_ids, vec![experience_id]);
    assert_eq!(closure.attribution_ids, vec![attribution_id]);
    assert_eq!(
        closure
            .candidate_ids
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        candidate_ids
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
    );
    assert_eq!(
        closure
            .revision_uids
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        revision_uids
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
    );

    let operation_id = format!("privacy-learning-erasure-{subject_id}");
    start_destruction(
        pool,
        tenant_id,
        &[subject_id],
        &operation_id,
        "privacy.erase",
    )
    .await
    .expect("start durable learning-erasure fence");
    let mut erase_guard =
        begin_destruction_stage_guard(pool, tenant_id, &[subject_id], &operation_id)
            .await
            .expect("begin guarded learning erase transaction");
    erase_guard
        .assume_owner_role()
        .await
        .expect("assume owner role for learning mutations");
    let owner_scope = role_scope_snapshot(erase_guard.connection()).await;
    assert_eq!(owner_scope.current_user, owner_scope.session_user);
    assert_eq!(
        owner_scope.tenant_id.as_deref(),
        Some(tenant_id.to_string().as_str())
    );
    assert_eq!(
        owner_scope.storage_partition_id.as_deref(),
        Some(tenant_key.as_str())
    );
    assert_eq!(owner_scope.contact_id.as_deref(), Some(""));
    assert_eq!(owner_scope.control_plane.as_deref(), Some("false"));
    assert_eq!(owner_scope.cleared_barriers.as_deref(), Some(""));
    let decisions = erase_learning_closure(
        &mut erase_guard,
        tenant_id,
        &subject_user_id,
        "attempt-applied",
        &closure,
    )
    .await
    .expect("erase recursive learning closure");
    let ledger_scope = role_scope_snapshot(erase_guard.connection()).await;
    assert_eq!(ledger_scope.backend_pid, owner_scope.backend_pid);
    assert_eq!(ledger_scope.current_user, "moa_app");
    assert_eq!(ledger_scope.session_user, owner_scope.session_user);
    assert_eq!(ledger_scope.tenant_id, owner_scope.tenant_id);
    assert_eq!(
        ledger_scope.storage_partition_id,
        owner_scope.storage_partition_id
    );
    assert_eq!(ledger_scope.contact_id, owner_scope.contact_id);
    assert_eq!(ledger_scope.control_plane, owner_scope.control_plane);
    assert_eq!(ledger_scope.cleared_barriers, owner_scope.cleared_barriers);
    drop(erase_guard);

    let rolled_back = sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
        "SELECT
            (SELECT COUNT(*) FROM experience_records WHERE id = $1),
            (SELECT COUNT(*) FROM experience_attributions WHERE id = $2),
            (SELECT COUNT(*) FROM learning_candidates WHERE id = ANY($3)),
            (SELECT COUNT(*) FROM moa.privacy_erasure_record_decision
             WHERE tenant_id = $4 AND subject_user_id = $5 AND attempt_id = 'attempt-applied'),
            (SELECT COUNT(*) FROM moa.artifact_revision
             WHERE revision_uid = ANY($6) AND status = 'ready')",
    )
    .bind(experience_id)
    .bind(attribution_id)
    .bind(&candidate_ids)
    .bind(tenant_id.0)
    .bind(&subject_user_id)
    .bind(&revision_uids)
    .fetch_one(pool)
    .await
    .expect("read rolled-back learning erasure state");
    assert_eq!(rolled_back, (1, 1, 3, 0, 1_001));

    let mut retry_guard =
        begin_destruction_stage_guard(pool, tenant_id, &[subject_id], &operation_id)
            .await
            .expect("retry under the same durable destruction fence");
    retry_guard
        .assume_owner_role()
        .await
        .expect("assume owner role for retried learning mutations");
    let retried_decisions = erase_learning_closure(
        &mut retry_guard,
        tenant_id,
        &subject_user_id,
        "attempt-applied",
        &closure,
    )
    .await
    .expect("retry recursive learning closure");
    retry_guard
        .finish()
        .await
        .expect("commit guarded learning erase transaction");
    assert_eq!(decisions.len(), 1_006);
    assert_eq!(retried_decisions.len(), decisions.len());
    for (retried, rolled_back) in retried_decisions.iter().zip(&decisions) {
        assert_eq!(retried.kind, rolled_back.kind);
        assert_eq!(retried.record_id, rolled_back.record_id);
        assert_eq!(retried.disposition, rolled_back.disposition);
        assert_eq!(retried.applied, rolled_back.applied);
        assert_eq!(retried.reason, rolled_back.reason);
    }
    assert_eq!(
        decisions
            .iter()
            .filter(|decision| decision.kind == ErasureRecordKind::ExperienceRecord)
            .count(),
        1
    );
    assert_eq!(
        decisions
            .iter()
            .filter(|decision| decision.kind == ErasureRecordKind::ExperienceAttribution)
            .count(),
        1
    );

    let remaining = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT
            (SELECT COUNT(*) FROM experience_records WHERE id = $1),
            (SELECT COUNT(*) FROM experience_attributions WHERE id = $2),
            (SELECT COUNT(*) FROM learning_candidates WHERE id = ANY($3))",
    )
    .bind(experience_id)
    .bind(attribution_id)
    .bind(&candidate_ids)
    .fetch_one(pool)
    .await
    .expect("count erased learning rows");
    assert_eq!(remaining, (0, 0, 0));
    let revisions: (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*),
                COUNT(*) FILTER (WHERE status = 'archived'),
                COUNT(*) FILTER (WHERE definition = '{}'::JSONB AND octet_length(source_text) = 0)
         FROM moa.artifact_revision WHERE revision_uid = ANY($1)",
    )
    .bind(&revision_uids)
    .fetch_one(pool)
    .await
    .expect("read invalidated revision identities");
    assert_eq!(revisions, (1_001, 1_001, 1_001));
    let remaining_files: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM moa.artifact_file WHERE revision_uid = ANY($1)")
            .bind(&revision_uids)
            .fetch_one(pool)
            .await
            .expect("count attributable artifact files");
    assert_eq!(remaining_files, 0);
    let recorded_kinds = sqlx::query_as::<_, (String, i64)>(
        "SELECT record_kind, COUNT(*)
         FROM moa.privacy_erasure_record_decision
         WHERE tenant_id = $1 AND subject_user_id = $2 AND attempt_id = 'attempt-applied'
         GROUP BY record_kind ORDER BY record_kind",
    )
    .bind(tenant_id.0)
    .bind(&subject_user_id)
    .fetch_all(pool)
    .await
    .expect("read recorded decision kinds");
    assert_eq!(
        recorded_kinds,
        vec![
            ("artifact_revision".to_string(), 1_001),
            ("experience_attribution".to_string(), 1),
            ("experience_record".to_string(), 1),
            ("learning_candidate".to_string(), 3),
        ]
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn legal_hold_subject_destruction_blocks_waiting_tenant_purge_start_across_pools_db_memory() {
    // Pins: a tenant-wide purge start waits behind a subject destruction start
    // on another pool, then refuses admission after the subject fence commits.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::new();
    let subject_id = Uuid::now_v7();
    let second_pool = PgPool::connect(&database_url)
        .await
        .expect("connect independent pool");

    let mut subject_tx =
        ScopedConn::begin_as_app(session_store.pool(), &RlsContext::tenant(tenant_id), true)
            .await
            .expect("begin subject destruction transaction");
    lock_tenant_and_subjects(subject_tx.as_mut(), tenant_id.0, &[subject_id])
        .await
        .expect("take canonical subject destruction locks");

    let waiting_pool = second_pool.clone();
    let tenant_purge = tokio::spawn(async move {
        start_destruction(
            &waiting_pool,
            tenant_id,
            &[],
            "purge-after-subject",
            "tenant.purge",
        )
        .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            while !tenant_purge.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_err(),
        "tenant purge start must wait for the tenant destruction lock"
    );

    sqlx::query(
        "INSERT INTO moa.destruction_operation_fence (tenant_id, subject_id, operation_id, operation_kind) VALUES ($1, $2, 'erase-subject-first', 'privacy.erase')",
    )
    .bind(tenant_id.0)
    .bind(subject_id)
    .execute(subject_tx.as_mut())
    .await
    .expect("persist subject destruction fence while locked");
    subject_tx
        .commit()
        .await
        .expect("commit subject destruction fence");

    let conflict = tenant_purge
        .await
        .expect("tenant purge waiter joins")
        .expect_err("subject fence must block tenant-wide destruction admission");
    assert!(matches!(conflict, LegalHoldError::FenceConflict));

    second_pool.close().await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn legal_hold_destruction_fence_blocks_subject_graph_recreation_db_memory() {
    // Pins: the durable subject fence rejects graph writes from every replica
    // throughout resumable erasure gaps.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    start_destruction(
        session_store.pool(),
        tenant_id,
        &[contact_id.0],
        "erase-write-fence",
        "privacy.erase",
    )
    .await
    .expect("start subject destruction");
    let graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, contact_id),
        test_kms(),
    );
    let error = graph
        .create_node(contact_node(
            tenant_id,
            contact_id,
            Uuid::now_v7(),
            "must not be recreated",
        ))
        .await
        .expect_err("fenced subject write must fail");
    assert!(
        error.to_string().contains("destruction is fenced"),
        "unexpected graph write refusal: {error}"
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn legal_hold_outer_guard_rechecks_durable_fence_before_crypto_shred_db_memory() {
    // Pins: a resumed graph stage cannot acquire the outer guard that must stay
    // held through crypto-shred when its durable operation fence no longer
    // matches, even if earlier graph deletion already ran.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::new();
    let subject_id = Uuid::now_v7();
    start_destruction(
        session_store.pool(),
        tenant_id,
        &[subject_id],
        "erase-original",
        "privacy.erase",
    )
    .await
    .expect("start subject destruction");
    let mut conn =
        ScopedConn::begin_as_app(session_store.pool(), &RlsContext::tenant(tenant_id), true)
            .await
            .expect("begin fence mutation");
    sqlx::query(
        "UPDATE moa.destruction_operation_fence SET operation_id = 'erase-other' WHERE tenant_id = $1 AND subject_id = $2",
    )
    .bind(tenant_id.0)
    .bind(subject_id)
    .execute(conn.as_mut())
    .await
    .expect("simulate mismatched resumed operation");
    conn.commit().await.expect("commit mismatched fence");

    let error = match begin_destruction_stage_guard(
        session_store.pool(),
        tenant_id,
        &[subject_id],
        "erase-original",
    )
    .await
    {
        Ok(_) => panic!("mismatched durable fence must block crypto-shred"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("durable destruction fence is missing"),
        "unexpected shred refusal: {error}"
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

fn contact_node(
    tenant_id: TenantId,
    contact_id: ContactId,
    uid: Uuid,
    name: &str,
) -> NodeWriteIntent {
    let subject_user_id = contact_id.to_string();
    NodeWriteIntent {
        barrier: None,
        uid,
        label: NodeLabel::Fact,
        storage_partition_id: Some(StoragePartitionId::for_tenant(tenant_id).to_string()),
        contact_id: Some(subject_user_id.clone()),
        data_subject_id: contact_id.0,
        scope: "contact".to_string(),
        name: name.to_string(),
        properties: json!({
            "name": name,
            "source": "erasure_db_memory",
            "user_id": subject_user_id,
        }),
        pii_class: SensitivityClass::Phi,
        confidence: Some(0.97),
        valid_from: chrono::DateTime::<chrono::Utc>::from_timestamp_micros(
            chrono::Utc::now().timestamp_micros(),
        )
        .expect("microsecond timestamp"),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        embedding_text: None,
        actor_id: contact_id.to_string(),
        actor_kind: "contact".to_string(),
    }
}

#[tokio::test]
async fn privileged_erasure_is_barrier_independent_and_subject_bounded_db_memory() {
    // Pins: empty retrieval clearances hide the candidate from enumeration, but
    // the narrowly granted subject eraser still removes it. Missing or
    // mismatched scope GUCs fail before any row is touched, and a tenant fence
    // makes the restricted eraser fail atomically with SQLSTATE 55000 even when
    // the caller spoofs the purge-operation GUC.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let other_tenant = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId::new();
    let subject_user_id = format!("contact:{contact_id}");
    let graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, contact_id),
        test_kms(),
    );
    let uid = Uuid::now_v7();
    let mut barriered = contact_node(tenant_id, contact_id, uid, "barriered private fact");
    barriered.barrier = Some(InformationBarrierId::parse("legal-matter-alpha").expect("barrier"));
    graph
        .create_node(barriered)
        .await
        .expect("seed barriered contact node");

    let hidden = enumerate_erase_candidates(session_store.pool(), tenant_id, &subject_user_id)
        .await
        .expect("enumerate without barrier clearance");
    assert!(
        hidden.is_empty(),
        "retrieval enumeration must remain barrier gated"
    );

    let audit = GraphErasureAudit {
        tenant_id,
        subject_user: contact_id.0,
        subject_user_id: subject_user_id.clone(),
        reason: "barrier-independent erasure".to_string(),
        approver_id: "admin@example.test".to_string(),
        approval_token_jti: "approval-jti-barrier-erasure".to_string(),
    };

    let mut missing_scope = session_store
        .pool()
        .begin()
        .await
        .expect("begin raw transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut *missing_scope)
        .await
        .expect("set app role");
    let missing_error = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT moa.erase_memory_data_subject($1, $2, $3)",
    )
    .bind(tenant_id.0)
    .bind(contact_id.0)
    .bind(json!({"approver_id": "admin", "approval_token_jti": "missing-guc"}))
    .fetch_one(&mut *missing_scope)
    .await
    .expect_err("missing GUCs must fail");
    assert_eq!(
        missing_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("42501")
    );
    missing_scope
        .rollback()
        .await
        .expect("rollback raw transaction");

    let mut mismatched = begin_app_scoped_tx(session_store.pool(), tenant_id, &subject_user_id)
        .await
        .expect("begin scoped mismatch transaction");
    let mismatch_error = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT moa.erase_memory_data_subject($1, $2, $3)",
    )
    .bind(other_tenant.0)
    .bind(contact_id.0)
    .bind(json!({"approver_id": "admin", "approval_token_jti": "wrong-tenant"}))
    .fetch_one(mismatched.as_mut())
    .await
    .expect_err("cross-tenant argument must fail");
    assert_eq!(
        mismatch_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("42501")
    );
    mismatched
        .rollback()
        .await
        .expect("rollback mismatch transaction");

    let before_fenced_erase: (i64, i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM moa.node_index WHERE uid = $1), \
            (SELECT count(*) FROM moa.vector_sync_outbox WHERE uid = $1), \
            (SELECT count(*) FROM moa.graph_changelog WHERE target_uid = $1)",
    )
    .bind(uid)
    .fetch_one(session_store.pool())
    .await
    .expect("read erasure state before tenant fence");
    sqlx::query(
        "INSERT INTO moa.destruction_operation_fence \
            (tenant_id, subject_id, operation_id, operation_kind) \
         VALUES ($1, NULL, 'privacy-tenant-fence', 'tenant.purge')",
    )
    .bind(tenant_id.0)
    .execute(session_store.pool())
    .await
    .expect("install tenant-wide privacy fence");
    sqlx::query(
        "INSERT INTO moa.tenant_purge_operations \
            (tenant_id, operation_id, status, current_stage) \
         VALUES ($1, 'privacy-tenant-fence', 'in_progress', 'authz')",
    )
    .bind(tenant_id.0)
    .execute(session_store.pool())
    .await
    .expect("install matching progress row for operation-GUC spoof probe");
    let mut fenced = begin_app_scoped_tx(session_store.pool(), tenant_id, &subject_user_id)
        .await
        .expect("begin fenced subject erasure");
    sqlx::query("SELECT set_config('moa.tenant_purge_operation_id', 'privacy-tenant-fence', true)")
        .execute(fenced.as_mut())
        .await
        .expect("spoof tenant purge operation GUC");
    let fenced_error = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT moa.erase_memory_data_subject($1, $2, $3)",
    )
    .bind(tenant_id.0)
    .bind(contact_id.0)
    .bind(json!({
        "approver_id": "admin",
        "approval_token_jti": "fenced-erasure"
    }))
    .fetch_one(fenced.as_mut())
    .await
    .expect_err("tenant-wide fence must reject the restricted privacy eraser");
    assert_eq!(
        fenced_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("55000"),
        "fenced privacy erasure must be a policy refusal, not a privilege error"
    );
    fenced.rollback().await.expect("rollback fenced erasure");
    let after_fenced_erase: (i64, i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM moa.node_index WHERE uid = $1), \
            (SELECT count(*) FROM moa.vector_sync_outbox WHERE uid = $1), \
            (SELECT count(*) FROM moa.graph_changelog WHERE target_uid = $1)",
    )
    .bind(uid)
    .fetch_one(session_store.pool())
    .await
    .expect("read erasure state after fenced refusal");
    assert_eq!(
        after_fenced_erase, before_fenced_erase,
        "a fenced erasure must roll back graph, vector, and changelog mutations"
    );
    sqlx::query(
        "DELETE FROM moa.destruction_operation_fence \
         WHERE tenant_id = $1 AND subject_id IS NULL",
    )
    .bind(tenant_id.0)
    .execute(session_store.pool())
    .await
    .expect("remove tenant-wide privacy fence");

    let mut erase_tx = begin_app_scoped_tx(session_store.pool(), tenant_id, &audit.subject_user_id)
        .await
        .expect("begin caller-owned hidden-subject erase transaction");
    assert_eq!(
        hard_purge_erase_candidates(erase_tx.as_mut(), &audit, &hidden)
            .await
            .expect("erase hidden subject rows"),
        1
    );
    erase_tx
        .commit()
        .await
        .expect("commit caller-owned hidden-subject erase transaction");
    let remaining =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moa.node_index WHERE uid = $1")
            .bind(uid)
            .fetch_one(session_store.pool())
            .await
            .expect("count barriered node after erase");
    assert_eq!(remaining, 0);

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privileged_erasure_grants_do_not_give_app_bypassrls_db_memory() {
    // Pins: the definer resolves only catalog and temporary objects and is
    // NOLOGIN/NOBYPASSRLS; only moa_app can execute the function, and moa_app
    // itself remains NOBYPASSRLS.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let row = sqlx::query(
        r#"
        SELECT proc.prosecdef,
               proc.proconfig,
               owner.rolname AS owner_name,
               owner.rolbypassrls AS owner_bypass,
               app.rolbypassrls AS app_bypass,
               has_function_privilege(
                   'moa_app',
                   'moa.erase_memory_data_subject(uuid,uuid,jsonb)',
                   'EXECUTE'
               ) AS app_execute,
               has_function_privilege(
                   'public',
                   'moa.erase_memory_data_subject(uuid,uuid,jsonb)',
                   'EXECUTE'
               ) AS public_execute
          FROM pg_proc AS proc
          JOIN pg_roles AS owner ON owner.oid = proc.proowner
          JOIN pg_roles AS app ON app.rolname = 'moa_app'
         WHERE proc.oid = 'moa.erase_memory_data_subject(uuid,uuid,jsonb)'::regprocedure
        "#,
    )
    .fetch_one(session_store.pool())
    .await
    .expect("inspect privacy eraser function grants");
    assert!(
        row.try_get::<bool, _>("prosecdef")
            .expect("security definer")
    );
    assert_eq!(
        row.try_get::<String, _>("owner_name").expect("owner name"),
        "moa_privacy_eraser"
    );
    assert!(
        !row.try_get::<bool, _>("owner_bypass")
            .expect("owner bypass")
    );
    assert!(!row.try_get::<bool, _>("app_bypass").expect("app bypass"));
    assert!(row.try_get::<bool, _>("app_execute").expect("app execute"));
    assert!(
        !row.try_get::<bool, _>("public_execute")
            .expect("public execute")
    );
    let proconfig = row
        .try_get::<Option<Vec<String>>, _>("proconfig")
        .expect("function config")
        .unwrap_or_default();
    assert!(
        proconfig
            .iter()
            .any(|entry| entry == "search_path=pg_catalog, pg_temp")
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

fn contact_edge(
    tenant_id: TenantId,
    contact_id: ContactId,
    start_uid: Uuid,
    end_uid: Uuid,
) -> EdgeWriteIntent {
    EdgeWriteIntent {
        uid: Uuid::now_v7(),
        label: EdgeLabel::RelatesTo,
        start_uid,
        end_uid,
        valid_from: chrono::DateTime::<chrono::Utc>::from_timestamp_micros(
            chrono::Utc::now().timestamp_micros(),
        )
        .expect("microsecond timestamp"),
        properties: json!({"source": "erasure_db_memory"}),
        storage_partition_id: Some(StoragePartitionId::for_tenant(tenant_id).to_string()),
        contact_id: Some(contact_id.to_string()),
        scope: "contact".to_string(),
        actor_id: contact_id.to_string(),
        actor_kind: "contact".to_string(),
    }
}

fn embedding_literal() -> String {
    let mut values = vec!["0"; 1024];
    values[0] = "1";
    format!("[{}]", values.join(","))
}

async fn seed_embedding(pool: &PgPool, tenant_id: TenantId, contact_id: ContactId, uid: Uuid) {
    let mut conn = ScopedConn::begin_contact(pool, tenant_id, contact_id)
        .await
        .expect("begin contact-scoped embedding seed");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role for embedding seed");
    sqlx::query(
        r#"
        INSERT INTO moa.embeddings
            (uid, storage_partition_id, user_id, label, pii_class, embedding,
             embedding_model, embedding_model_version, valid_to)
        SELECT uid, storage_partition_id, user_id, label, pii_class,
               $2::public.halfvec, 'erasure-test-model', 1, valid_to
        FROM moa.node_index
        WHERE uid = $1
        "#,
    )
    .bind(uid)
    .bind(embedding_literal())
    .execute(conn.as_mut())
    .await
    .expect("seed contact embedding row");
    conn.commit().await.expect("commit contact embedding seed");
}

#[tokio::test]
async fn hard_purge_contact_candidates_writes_summary_under_app_role_db_memory() {
    // Pins: privacy erasure deletes contact-owned graph memory through the
    // subject-bounded definer and leaves one redacted contact summary.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId::new();
    let subject_user_id = format!("contact:{contact_id}");
    let graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, contact_id),
        test_kms(),
    );
    let uid = Uuid::now_v7();
    graph
        .create_node(contact_node(
            tenant_id,
            contact_id,
            uid,
            "contact erasure fact",
        ))
        .await
        .expect("seed contact graph node");

    let candidates = enumerate_erase_candidates(session_store.pool(), tenant_id, &subject_user_id)
        .await
        .expect("enumerate contact erase candidates");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].uid, uid);
    assert_eq!(candidates[0].label, "Fact");
    // PHI/restricted content is sealed at rest, so the indexed plaintext `name`
    // that erase enumeration reads is the redaction placeholder, not the secret
    // — the candidate listing (and the erase response sample) never leaks it.
    assert_eq!(candidates[0].name, "[RESTRICTED]");
    assert_eq!(candidates[0].pii_class, "phi");

    let audit = GraphErasureAudit {
        tenant_id,
        subject_user: contact_id.0,
        subject_user_id,
        reason: "dsar erasure request".to_string(),
        approver_id: "admin@example.test".to_string(),
        approval_token_jti: "approval-jti-erasure-db-memory".to_string(),
    };
    let mut erase_tx = begin_app_scoped_tx(session_store.pool(), tenant_id, &audit.subject_user_id)
        .await
        .expect("begin caller-owned contact erase transaction");
    let erased = hard_purge_erase_candidates(erase_tx.as_mut(), &audit, &candidates)
        .await
        .expect("hard purge contact candidates");
    erase_tx
        .commit()
        .await
        .expect("commit caller-owned contact erase transaction");
    assert_eq!(erased, 1);
    assert!(
        graph
            .get_node(uid)
            .await
            .expect("read purged graph node")
            .is_none(),
        "purged node should not remain visible"
    );

    let mut conn = ScopedConn::begin_contact(session_store.pool(), tenant_id, contact_id)
        .await
        .expect("begin contact-scoped changelog read");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    let erase_rows = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM moa.graph_changelog
        WHERE op = 'erase'
          AND target_uid = $1
          AND contact_id = $2
        "#,
    )
    .bind(uid)
    .bind(contact_id.0)
    .fetch_one(conn.as_mut())
    .await
    .expect("count contact node erase rows");
    assert_eq!(erase_rows, 0, "per-node payload history must be removed");

    let summary = sqlx::query_as::<_, (String, Option<Uuid>, serde_json::Value)>(
        r#"
        SELECT scope, contact_id, payload
        FROM moa.graph_changelog
        WHERE op = 'erase'
          AND target_kind = 'contact'
          AND target_uid = $1
        "#,
    )
    .bind(contact_id.0)
    .fetch_one(conn.as_mut())
    .await
    .expect("read contact erasure summary row");
    assert_eq!(summary.0, "contact");
    assert_eq!(summary.1, Some(contact_id.0));
    assert_eq!(summary.2["redacted"], true);
    assert_eq!(summary.2["nodes_deleted"], 1);
    conn.commit().await.expect("commit changelog read");

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn hard_purge_tolerates_absent_candidate_db_memory() {
    // Pins: an already-absent candidate counts as completed progress rather than a
    // terminal NotFound error, so a resumed erasure that re-enumerates a partially
    // purged subject never strands on the first missing node.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId::new();
    let subject_user_id = format!("contact:{contact_id}");
    let graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, contact_id),
        test_kms(),
    );
    let present_uid = Uuid::now_v7();
    graph
        .create_node(contact_node(
            tenant_id,
            contact_id,
            present_uid,
            "present fact",
        ))
        .await
        .expect("seed present contact node");

    let mut candidates =
        enumerate_erase_candidates(session_store.pool(), tenant_id, &subject_user_id)
            .await
            .expect("enumerate contact erase candidates");
    assert_eq!(candidates.len(), 1);
    // Prepend a candidate whose node is already gone (concurrent purge or resume).
    candidates.insert(
        0,
        EraseCandidate {
            uid: Uuid::now_v7(),
            label: "Fact".to_string(),
            name: "already purged".to_string(),
            pii_class: "phi".to_string(),
        },
    );

    let audit = GraphErasureAudit {
        tenant_id,
        subject_user: contact_id.0,
        subject_user_id,
        reason: "resumed erasure request".to_string(),
        approver_id: "admin@example.test".to_string(),
        approval_token_jti: "approval-jti-absent-candidate-db-memory".to_string(),
    };
    let mut erase_tx = begin_app_scoped_tx(session_store.pool(), tenant_id, &audit.subject_user_id)
        .await
        .expect("begin caller-owned resumed erase transaction");
    let erased = hard_purge_erase_candidates(erase_tx.as_mut(), &audit, &candidates)
        .await
        .expect("hard purge tolerates already-absent candidate");
    erase_tx
        .commit()
        .await
        .expect("commit caller-owned resumed erase transaction");
    assert_eq!(erased, 1);
    assert!(
        graph
            .get_node(present_uid)
            .await
            .expect("read purged present node")
            .is_none(),
        "the present candidate must still be purged"
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn delete_subject_digest_and_lineage_rows_db_memory() {
    // Pins: erasure closure deletes the subject's standing memory-digest and
    // retrieval-lineage rows, which graph-node purges never touch.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId::new();
    let subject_user_id = format!("contact:{contact_id}");
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id).to_string();

    seed_digest_row(
        session_store.pool(),
        tenant_id,
        contact_id,
        &storage_partition_id,
    )
    .await;
    seed_lineage_row(
        session_store.pool(),
        tenant_id,
        contact_id,
        &storage_partition_id,
    )
    .await;

    let mut erase_tx = begin_app_scoped_tx(session_store.pool(), tenant_id, &subject_user_id)
        .await
        .expect("begin caller-owned digest and lineage erase transaction");
    let digests_deleted = delete_subject_digests(erase_tx.as_mut(), tenant_id, &subject_user_id)
        .await
        .expect("delete subject digests");
    assert_eq!(digests_deleted, 1);
    let lineage_deleted =
        delete_subject_retrieval_lineage(erase_tx.as_mut(), tenant_id, &subject_user_id)
            .await
            .expect("delete subject retrieval lineage");
    assert_eq!(lineage_deleted, 1);
    erase_tx
        .commit()
        .await
        .expect("commit caller-owned digest and lineage erase transaction");

    let remaining_digests = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.memory_digests WHERE storage_partition_id = $1",
    )
    .bind(&storage_partition_id)
    .fetch_one(session_store.pool())
    .await
    .expect("count remaining digest rows");
    assert_eq!(remaining_digests, 0);
    let remaining_lineage = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.retrieval_lineage WHERE storage_partition_id = $1",
    )
    .bind(&storage_partition_id)
    .fetch_one(session_store.pool())
    .await
    .expect("count remaining lineage rows");
    assert_eq!(remaining_lineage, 0);

    // A re-run is idempotent: nothing remains to delete.
    let mut replay_tx = begin_app_scoped_tx(session_store.pool(), tenant_id, &subject_user_id)
        .await
        .expect("begin caller-owned digest replay transaction");
    let digests_deleted_again =
        delete_subject_digests(replay_tx.as_mut(), tenant_id, &subject_user_id)
            .await
            .expect("re-run digest deletion");
    assert_eq!(digests_deleted_again, 0);
    replay_tx
        .commit()
        .await
        .expect("commit caller-owned digest replay transaction");

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

async fn seed_digest_row(
    pool: &PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
    storage_partition_id: &str,
) {
    let mut conn = ScopedConn::begin_contact(pool, tenant_id, contact_id)
        .await
        .expect("begin contact-scoped digest seed");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role for digest seed");
    sqlx::query(
        r#"
        INSERT INTO moa.memory_digests
            (storage_partition_id, user_id, content, version, updated_at)
        VALUES ($1, $2, $3, 1, now())
        "#,
    )
    .bind(storage_partition_id)
    .bind(contact_id.to_string())
    .bind("What I know about this contact:\n- prefers dark mode\n")
    .execute(conn.as_mut())
    .await
    .expect("seed memory digest row");
    conn.commit().await.expect("commit digest seed");
}

async fn seed_lineage_row(
    pool: &PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
    storage_partition_id: &str,
) {
    let mut conn = ScopedConn::begin_contact(pool, tenant_id, contact_id)
        .await
        .expect("begin contact-scoped lineage seed");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role for lineage seed");
    sqlx::query(
        r#"
        INSERT INTO moa.retrieval_lineage
            (storage_partition_id, user_id, session_id, turn_seq, uid, rank, retrieved_at)
        VALUES ($1, $2, $3, 1, $4, 1, now())
        "#,
    )
    .bind(storage_partition_id)
    .bind(contact_id.to_string())
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .execute(conn.as_mut())
    .await
    .expect("seed retrieval lineage row");
    conn.commit().await.expect("commit lineage seed");
}

#[tokio::test]
async fn hard_purge_contact_candidates_includes_historical_versions_db_memory() {
    // Pins: a contact hard purge enumerates and erases every attributable node version,
    // including invalidated and superseded history, plus incident graph/vector rows and
    // exact audit records, without touching another contact in the same tenant.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_id = ContactId::new();
    let other_contact_id = ContactId::new();
    let canonical_subject_user_id = contact_id.to_string();
    let legacy_subject_user_id = format!("contact:{contact_id}");
    let graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, contact_id),
        test_kms(),
    );
    let other_graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, other_contact_id),
        test_kms(),
    );

    let active_uid = Uuid::now_v7();
    let mut active_node = contact_node(
        tenant_id,
        contact_id,
        active_uid,
        "target active private fact",
    );
    active_node.pii_class = SensitivityClass::None;
    graph
        .create_node(active_node)
        .await
        .expect("seed active target node");

    let invalidated_uid = Uuid::now_v7();
    graph
        .create_node(contact_node(
            tenant_id,
            contact_id,
            invalidated_uid,
            "target invalidated private fact",
        ))
        .await
        .expect("seed target node to invalidate");
    graph
        .create_edge(contact_edge(
            tenant_id,
            contact_id,
            invalidated_uid,
            active_uid,
        ))
        .await
        .expect("seed edge incident to invalidated target node");
    graph
        .invalidate_node(invalidated_uid, "historical erasure regression")
        .await
        .expect("invalidate target node");

    let superseded_uid = Uuid::now_v7();
    graph
        .create_node(contact_node(
            tenant_id,
            contact_id,
            superseded_uid,
            "target superseded private fact",
        ))
        .await
        .expect("seed target node to supersede");
    let replacement_uid = Uuid::now_v7();
    let written_replacement_uid = graph
        .supersede_node(
            superseded_uid,
            contact_node(
                tenant_id,
                contact_id,
                replacement_uid,
                "target replacement private fact",
            ),
        )
        .await
        .expect("supersede target node");
    assert_eq!(written_replacement_uid, replacement_uid);
    graph
        .create_edge(contact_edge(
            tenant_id,
            contact_id,
            active_uid,
            replacement_uid,
        ))
        .await
        .expect("seed edge incident to active target nodes");

    let other_uid = Uuid::now_v7();
    let mut other_node = contact_node(
        tenant_id,
        other_contact_id,
        other_uid,
        "other contact private fact",
    );
    other_node.pii_class = SensitivityClass::None;
    other_graph
        .create_node(other_node)
        .await
        .expect("seed other-contact node");

    let mut target_uids = vec![active_uid, invalidated_uid, superseded_uid, replacement_uid];
    target_uids.sort_unstable();
    seed_embedding(session_store.pool(), tenant_id, contact_id, active_uid).await;
    seed_embedding(session_store.pool(), tenant_id, other_contact_id, other_uid).await;

    let canonical_candidates =
        enumerate_erase_candidates(session_store.pool(), tenant_id, &canonical_subject_user_id)
            .await
            .expect("enumerate canonical contact erase candidates");
    let canonical_candidate_uids = canonical_candidates
        .iter()
        .map(|candidate| candidate.uid)
        .collect::<Vec<_>>();
    assert_eq!(canonical_candidate_uids, target_uids);

    let legacy_candidates =
        enumerate_erase_candidates(session_store.pool(), tenant_id, &legacy_subject_user_id)
            .await
            .expect("enumerate legacy contact erase candidates");
    let legacy_candidate_uids = legacy_candidates
        .iter()
        .map(|candidate| candidate.uid)
        .collect::<Vec<_>>();
    assert_eq!(legacy_candidate_uids, target_uids);
    assert!(!legacy_candidate_uids.contains(&other_uid));

    let target_node_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moa.node_index WHERE uid = ANY($1)")
            .bind(&target_uids)
            .fetch_one(session_store.pool())
            .await
            .expect("count seeded target nodes");
    assert_eq!(target_node_count, 4);
    let target_incident_edge_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.edge_index WHERE start_uid = ANY($1) OR end_uid = ANY($1)",
    )
    .bind(&target_uids)
    .fetch_one(session_store.pool())
    .await
    .expect("count seeded target incident edges");
    assert_eq!(target_incident_edge_count, 3);
    let target_embedding_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moa.embeddings WHERE uid = ANY($1)")
            .bind(&target_uids)
            .fetch_one(session_store.pool())
            .await
            .expect("count seeded target embeddings");
    assert_eq!(target_embedding_count, 1);

    let audit = GraphErasureAudit {
        tenant_id,
        subject_user: contact_id.0,
        subject_user_id: legacy_subject_user_id.clone(),
        reason: "all-version dsar erasure request".to_string(),
        approver_id: "admin@example.test".to_string(),
        approval_token_jti: "approval-jti-all-version-erasure-db-memory".to_string(),
    };
    let mut erase_tx = begin_app_scoped_tx(session_store.pool(), tenant_id, &audit.subject_user_id)
        .await
        .expect("begin caller-owned all-version erase transaction");
    let erased = hard_purge_erase_candidates(erase_tx.as_mut(), &audit, &legacy_candidates)
        .await
        .expect("hard purge every target contact version");
    erase_tx
        .commit()
        .await
        .expect("commit caller-owned all-version erase transaction");
    assert_eq!(erased, 4);

    let remaining_target_nodes =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moa.node_index WHERE uid = ANY($1)")
            .bind(&target_uids)
            .fetch_one(session_store.pool())
            .await
            .expect("count target nodes after hard purge");
    assert_eq!(remaining_target_nodes, 0);
    let remaining_target_edges = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.edge_index WHERE start_uid = ANY($1) OR end_uid = ANY($1)",
    )
    .bind(&target_uids)
    .fetch_one(session_store.pool())
    .await
    .expect("count target incident edges after hard purge");
    assert_eq!(remaining_target_edges, 0);
    let remaining_target_embeddings =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moa.embeddings WHERE uid = ANY($1)")
            .bind(&target_uids)
            .fetch_one(session_store.pool())
            .await
            .expect("count target embeddings after hard purge");
    assert_eq!(remaining_target_embeddings, 0);
    let queued_external_deletes = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.vector_sync_outbox WHERE uid = ANY($1) AND op = 'delete'",
    )
    .bind(&target_uids)
    .fetch_one(session_store.pool())
    .await
    .expect("count transactional external-vector deletes");
    assert_eq!(queued_external_deletes, 4);

    let other_node_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moa.node_index WHERE uid = $1")
            .bind(other_uid)
            .fetch_one(session_store.pool())
            .await
            .expect("count preserved other-contact node");
    assert_eq!(other_node_count, 1);
    let other_embedding_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM moa.embeddings WHERE uid = $1")
            .bind(other_uid)
            .fetch_one(session_store.pool())
            .await
            .expect("count preserved other-contact embedding");
    assert_eq!(other_embedding_count, 1);

    let node_audit_counts = sqlx::query_as::<_, (Uuid, i64)>(
        r#"
        SELECT target_uid, COUNT(*)
        FROM moa.graph_changelog
        WHERE op = 'erase'
          AND target_kind = 'node'
          AND target_uid = ANY($1)
        GROUP BY target_uid
        ORDER BY target_uid
        "#,
    )
    .bind(&target_uids)
    .fetch_all(session_store.pool())
    .await
    .expect("read per-node erase audit counts");
    assert!(
        node_audit_counts.is_empty(),
        "subject payload history must be removed"
    );

    let summary_payloads = sqlx::query_scalar::<_, serde_json::Value>(
        r#"
        SELECT payload
        FROM moa.graph_changelog
        WHERE op = 'erase'
          AND target_kind = 'contact'
          AND target_uid = $1
        ORDER BY change_id
        "#,
    )
    .bind(contact_id.0)
    .fetch_all(session_store.pool())
    .await
    .expect("read contact erase summary audit rows");
    assert_eq!(
        summary_payloads,
        vec![json!({
            "redacted": true,
            "nodes_deleted": 4,
            "edges_deleted": 3,
            "embeddings_deleted": 1,
        })]
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
