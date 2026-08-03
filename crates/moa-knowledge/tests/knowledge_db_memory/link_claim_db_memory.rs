//! DB integration coverage for operation-fenced knowledge link claims.

use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_knowledge::{
    domain::{
        KnowledgeConnection, KnowledgeCredentialOwnership, LinkClaimReservation, LinkClaimState,
        LinkClaimTransition, NewLinkClaim,
    },
    repository::{KnowledgeRepository, PostgresKnowledgeRepository},
};
use moa_test_support::postgres;
use serde_json::json;
use uuid::Uuid;

fn repository(db: &postgres::TestDb, tenant_id: TenantId) -> PostgresKnowledgeRepository {
    PostgresKnowledgeRepository::scoped_for_app_role(
        db.store().pool().clone(),
        RlsContext::tenant(tenant_id),
    )
}

fn new_claim(tenant_id: TenantId, connection_uid: Uuid, operation_id: &str) -> NewLinkClaim {
    NewLinkClaim {
        tenant_id,
        operation_id: operation_id.to_string(),
        request_hash: format!("hash-{operation_id}"),
        owner_identity_id: Some(Uuid::now_v7()),
        connection_uid,
    }
}

fn credential_written(candidate_credential_ref: String) -> LinkClaimTransition {
    LinkClaimTransition::CredentialWritten {
        credential_ownership: KnowledgeCredentialOwnership::MoaManaged,
        candidate_credential_ref: Some(candidate_credential_ref),
        previous_vault_credential_ref: None,
    }
}

fn connection(tenant_id: TenantId) -> KnowledgeConnection {
    let now = moa_test_support::fixtures::pg_now();
    KnowledgeConnection {
        connection_uid: Uuid::now_v7(),
        tenant_id,
        provider: "merge".to_string(),
        connector: "knowledgebase".to_string(),
        provider_account_id: format!("linked-account-{}", Uuid::now_v7()),
        metadata: json!({}),
        source_selection: json!({}),
        information_barrier: None,
        created_at: now,
        updated_at: now,
        last_synced_at: None,
    }
}

async fn insert_connector_parent(
    db: &postgres::TestDb,
    connection: &KnowledgeConnection,
    lifecycle_status: &str,
) {
    sqlx::query(
        r#"
        INSERT INTO moa.connector_connections (
            connection_uid, tenant_id, display_name, built_in_key, built_in_version,
            non_secret_config, lifecycle_status, health_status
        )
        VALUES ($1, $2, $3, 'knowledge:merge', 1, '{}'::JSONB, $4, 'ready')
        "#,
    )
    .bind(connection.connection_uid)
    .bind(connection.tenant_id.0)
    .bind(&connection.connector)
    .bind(lifecycle_status)
    .execute(db.store().pool())
    .await
    .expect("insert generic connector parent for knowledge fixture");
}

#[tokio::test]
async fn link_claim_transitions_are_compare_and_swap_and_terminal_states_stick_db_memory() {
    // Pins: the claim state machine is enforced by the database, not by call
    // order. A transition from the wrong state applies nothing, so a replayed or
    // concurrent link observes the loss instead of rewriting a newer claim, and
    // a compensated operation can never be revived.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap link claim db");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repository = repository(&db, tenant_id);
    let connection_uid = Uuid::now_v7();
    let operation_id = format!("link-{}", Uuid::now_v7());

    let reserved = repository
        .reserve_link_claim(new_claim(tenant_id, connection_uid, &operation_id))
        .await
        .expect("reserve claim");
    assert!(matches!(reserved, LinkClaimReservation::Reserved(_)));

    // Finalizing straight from `reserved` skips the credential write.
    assert!(
        repository
            .advance_link_claim(
                tenant_id,
                &operation_id,
                LinkClaimTransition::Finalized {
                    sync_run_uid: Uuid::now_v7(),
                },
            )
            .await
            .expect("attempt premature finalization")
            .is_none(),
        "finalization must be impossible before the credential exists"
    );

    let parent = repository
        .advance_link_claim(
            tenant_id,
            &operation_id,
            LinkClaimTransition::ParentClaimed {
                parent_created_by_claim: true,
                credential_expected_generation: 1,
            },
        )
        .await
        .expect("record parent claim")
        .expect("parent transition should apply");
    assert_eq!(parent.state, LinkClaimState::ParentClaimed);
    assert!(parent.parent_created_by_claim);

    let candidate = Uuid::now_v7().to_string();
    let written = repository
        .advance_link_claim(
            tenant_id,
            &operation_id,
            credential_written(candidate.clone()),
        )
        .await
        .expect("record candidate")
        .expect("transition should apply");
    assert_eq!(written.state, LinkClaimState::CredentialWritten);
    assert_eq!(
        written.candidate_credential_ref.as_deref(),
        Some(&*candidate)
    );

    // Repeating the same transition is not a silent no-op success: the claim has
    // already left `parent_claimed`, so the compare-and-swap reports the loss.
    assert!(
        repository
            .advance_link_claim(
                tenant_id,
                &operation_id,
                credential_written(Uuid::now_v7().to_string()),
            )
            .await
            .expect("attempt duplicate credential write")
            .is_none(),
        "a second credential write must not overwrite the recorded candidate"
    );

    repository
        .advance_link_claim(tenant_id, &operation_id, LinkClaimTransition::Compensating)
        .await
        .expect("enter compensation")
        .expect("transition should apply");
    repository
        .advance_link_claim(tenant_id, &operation_id, LinkClaimTransition::Compensated)
        .await
        .expect("finish compensation")
        .expect("transition should apply");

    assert!(
        repository
            .advance_link_claim(
                tenant_id,
                &operation_id,
                LinkClaimTransition::Finalized {
                    sync_run_uid: Uuid::now_v7(),
                },
            )
            .await
            .expect("attempt post-compensation finalization")
            .is_none(),
        "a compensated operation is terminal and can never finalize"
    );
    let claim = repository
        .get_link_claim(tenant_id, &operation_id)
        .await
        .expect("read claim")
        .expect("claim should exist");
    assert_eq!(claim.state, LinkClaimState::Compensated);
}

#[tokio::test]
async fn ownerless_service_actor_can_resume_but_cannot_create_link_claim_db_memory() {
    // Pins: only an authenticated operator may establish durable ownership of a
    // new link. A closed service actor can replay the exact already-owned claim,
    // but an ownerless first attempt writes no row.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap ownerless link claim db");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repository = repository(&db, tenant_id);
    let connection_uid = Uuid::now_v7();
    let operation_id = format!("link-{}", Uuid::now_v7());
    let mut ownerless = new_claim(tenant_id, connection_uid, &operation_id);
    ownerless.owner_identity_id = None;

    assert_eq!(
        repository
            .reserve_link_claim(ownerless.clone())
            .await
            .expect("ownerless reservation should return a typed outcome"),
        LinkClaimReservation::OwnerRequired
    );
    assert_eq!(
        repository
            .get_link_claim(tenant_id, &operation_id)
            .await
            .expect("read after refused ownerless reservation"),
        None,
        "ownerless first use must not insert a claim"
    );

    let owned = new_claim(tenant_id, connection_uid, &operation_id);
    let owner = owned
        .owner_identity_id
        .expect("operator fixture should carry an owner");
    assert!(matches!(
        repository
            .reserve_link_claim(owned)
            .await
            .expect("operator should reserve claim"),
        LinkClaimReservation::Reserved(_)
    ));

    let replay = repository
        .reserve_link_claim(ownerless)
        .await
        .expect("service actor should resume exact claim");
    let LinkClaimReservation::Existing(replay) = replay else {
        panic!("exact ownerless replay should return the existing claim");
    };
    assert_eq!(replay.owner_identity_id, Some(owner));
    assert_eq!(replay.connection_uid, connection_uid);
}

#[tokio::test]
async fn parent_claim_phase_records_exact_parent_ownership_before_credentials_db_memory() {
    // Pins: the claim durably distinguishes a parent it created from a shared
    // pre-existing parent before a credential can be recorded, so compensation
    // cannot delete shared connector state.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap parent ownership claim db");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repository = repository(&db, tenant_id);

    for parent_created_by_claim in [false, true] {
        let connection_uid = Uuid::now_v7();
        let operation_id = format!("link-{}", Uuid::now_v7());
        repository
            .reserve_link_claim(new_claim(tenant_id, connection_uid, &operation_id))
            .await
            .expect("reserve parent ownership claim");

        assert!(
            repository
                .advance_link_claim(
                    tenant_id,
                    &operation_id,
                    credential_written(Uuid::now_v7().to_string()),
                )
                .await
                .expect("attempt credential before parent")
                .is_none(),
            "credential write must not skip the parent claim phase"
        );

        let parent = repository
            .advance_link_claim(
                tenant_id,
                &operation_id,
                LinkClaimTransition::ParentClaimed {
                    parent_created_by_claim,
                    credential_expected_generation: 1,
                },
            )
            .await
            .expect("record parent ownership")
            .expect("parent claim transition should apply");
        assert_eq!(parent.state, LinkClaimState::ParentClaimed);
        assert_eq!(
            parent.parent_created_by_claim, parent_created_by_claim,
            "the durable ownership bit must match the managed-parent outcome"
        );
    }
}

#[tokio::test]
async fn reusing_a_link_operation_id_with_different_inputs_is_a_typed_conflict_db_memory() {
    // Pins: the request hash is compared, not overwritten. Reusing an operation
    // id for a different connection reports a conflict rather than adopting the
    // new connection under the existing claim.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap link claim conflict db");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repository = repository(&db, tenant_id);
    let operation_id = format!("link-{}", Uuid::now_v7());
    let connection_uid = Uuid::now_v7();

    repository
        .reserve_link_claim(new_claim(tenant_id, connection_uid, &operation_id))
        .await
        .expect("reserve claim");

    let replay = repository
        .reserve_link_claim(new_claim(tenant_id, connection_uid, &operation_id))
        .await
        .expect("replay reservation");
    assert!(
        matches!(replay, LinkClaimReservation::Existing(_)),
        "an identical reservation must resume the recorded claim"
    );

    let mut divergent = new_claim(tenant_id, Uuid::now_v7(), &operation_id);
    divergent.request_hash = format!("hash-{operation_id}");
    let conflict = repository
        .reserve_link_claim(divergent)
        .await
        .expect("divergent reservation");
    assert_eq!(
        conflict,
        LinkClaimReservation::Conflict,
        "the same id claiming a different connection must be a typed conflict"
    );
}

#[tokio::test]
async fn forced_rls_denies_missing_and_wrong_tenant_claim_access_as_moa_app_db_memory() {
    // Pins: claim rows are reachable only under the owning tenant's forced-RLS
    // context. A missing `moa.tenant_id` denies rather than widening — the claim
    // table deliberately has no control-plane escape hatch — and another tenant's
    // context cannot read or advance the claim.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap link claim rls db");
    let owner = TenantId::from(Uuid::now_v7());
    let intruder = TenantId::from(Uuid::now_v7());
    let operation_id = format!("link-{}", Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    repository(&db, owner)
        .reserve_link_claim(new_claim(owner, connection_uid, &operation_id))
        .await
        .expect("reserve claim as the owning tenant");

    // Wrong tenant: the row is invisible, so a read finds nothing and a
    // compare-and-swap applies to no row.
    let intruder_repository = repository(&db, intruder);
    assert_eq!(
        intruder_repository
            .get_link_claim(owner, &operation_id)
            .await
            .expect("read claim under the wrong tenant"),
        None,
        "another tenant must not be able to read this claim"
    );
    assert!(
        intruder_repository
            .advance_link_claim(
                owner,
                &operation_id,
                credential_written(Uuid::now_v7().to_string()),
            )
            .await
            .expect("attempt cross-tenant transition")
            .is_none(),
        "another tenant must not be able to advance this claim"
    );

    // Missing tenant context: `moa_app` with no `moa.tenant_id` sees nothing.
    let mut tx = db
        .store()
        .pool()
        .begin()
        .await
        .expect("begin unscoped probe");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut *tx)
        .await
        .expect("assume moa_app");
    let visible: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.knowledge_link_claims WHERE operation_id = $1",
    )
    .bind(&operation_id)
    .fetch_one(&mut *tx)
    .await
    .expect("count claims without tenant context");
    tx.rollback().await.expect("rollback unscoped probe");
    assert_eq!(
        visible, 0,
        "a missing tenant context must deny rather than widen to every tenant"
    );

    // The owning tenant still sees its own claim.
    assert!(
        repository(&db, owner)
            .get_link_claim(owner, &operation_id)
            .await
            .expect("read claim as the owning tenant")
            .is_some()
    );
}

#[tokio::test]
async fn provider_trigger_boundary_is_write_once_and_survives_status_updates_db_memory() {
    // Pins: the durable dispatch boundary is the only thing that distinguishes a
    // run that was never dispatched from one that was. It must not move on
    // replay, and an ordinary status update must not erase it — either would let
    // a link finalize on a provider call that never happened.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap trigger boundary db");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repository = repository(&db, tenant_id);
    let fixture = connection(tenant_id);
    insert_connector_parent(&db, &fixture, "active").await;
    let connection = repository
        .upsert_connection(fixture)
        .await
        .expect("store connection");
    let mut run = moa_knowledge::domain::KnowledgeSyncRun {
        sync_run_uid: Uuid::now_v7(),
        tenant_id,
        connection_uid: connection.connection_uid,
        parser: None,
        max_records: None,
        information_barrier: None,
        status: moa_knowledge::domain::SyncRunStatus::Queued,
        records_seen: 0,
        records_changed: 0,
        records_deleted: 0,
        records_ingested: 0,
        records_failed: 0,
        objects_parsed: 0,
        chunks_embedded: 0,
        graph_nodes_upserted: 0,
        graph_edges_upserted: 0,
        error_code: None,
        started_at: moa_test_support::fixtures::pg_now(),
        finished_at: None,
        provider_trigger_completed_at: None,
    };
    repository
        .create_sync_run(run.clone())
        .await
        .expect("create sync run");
    assert!(
        repository
            .get_sync_run(run.sync_run_uid)
            .await
            .expect("read run")
            .expect("run should exist")
            .provider_trigger_completed_at
            .is_none(),
        "a queued run must not claim a dispatch that never happened"
    );

    repository
        .mark_provider_trigger_completed(run.sync_run_uid)
        .await
        .expect("record dispatch");
    let first = repository
        .get_sync_run(run.sync_run_uid)
        .await
        .expect("read run")
        .expect("run should exist")
        .provider_trigger_completed_at
        .expect("boundary should be durable");

    repository
        .mark_provider_trigger_completed(run.sync_run_uid)
        .await
        .expect("replay dispatch");
    run.status = moa_knowledge::domain::SyncRunStatus::ProviderSyncing;
    repository
        .update_sync_run(run.clone())
        .await
        .expect("advance run status");

    let reread = repository
        .get_sync_run(run.sync_run_uid)
        .await
        .expect("read run")
        .expect("run should exist");
    assert_eq!(
        reread.provider_trigger_completed_at,
        Some(first),
        "the boundary records first observation and survives status updates"
    );
}
