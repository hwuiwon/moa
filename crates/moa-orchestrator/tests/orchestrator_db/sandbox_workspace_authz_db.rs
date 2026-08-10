//! DB-backed atomic authorization coverage for sandbox workspaces.

use anyhow::Result;
use moa_authz::{
    AuthzCheckError, FgaClient, FgaConfig, enqueue_raw, require_authz,
    require_authz_with_delegation,
};
use moa_authz_schema::{MODEL_VERSION, ObjectType, Relation, TupleOp};
use moa_core::{
    traits::{Identity, IdentityType},
    types::{
        identifiers::{ProviderAccountId, SandboxWorkspaceId, SessionId, TenantId},
        sandbox_workspace::{DurabilityClass, SandboxWorkspaceScope},
    },
};
use moa_hands::core::sandbox_workspace::{
    model::{
        CreateWorkspaceRequest, WorkspaceGrant, WorkspaceGrantRelation, WorkspaceGrantSubjectType,
        WorkspaceTransition,
    },
    repository::PostgresWorkspaceRepository,
};
use serde_json::{Value, json};
use uuid::Uuid;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers};

fn fga_client(server: &MockServer) -> FgaClient {
    FgaClient::new(FgaConfig {
        url: server.uri(),
        preshared_key: "sandbox-workspace-authz-test".to_string(),
        store_id: "store-1".to_string(),
        model_id: "model-1".to_string(),
        timeout_ms: 5_000,
    })
    .expect("workspace authz mock config should be valid")
}

fn identity(
    identity_type: IdentityType,
    id: Uuid,
    tenant_id: TenantId,
    acting_on_behalf_of: Option<Uuid>,
) -> Identity {
    Identity {
        identity_type,
        id,
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of,
    }
}

fn check_body(subject: &str, relation: &str, workspace_id: SandboxWorkspaceId) -> Value {
    json!({
        "authorization_model_id": "model-1",
        "tuple_key": {
            "user": subject,
            "relation": relation,
            "object": format!("sandbox_workspace:{workspace_id}"),
        },
    })
}

fn delegated_batch_body(delegator: Uuid, agent: Uuid, workspace_id: SandboxWorkspaceId) -> Value {
    json!({
        "authorization_model_id": "model-1",
        "checks": [
            {
                "tuple_key": {
                    "user": format!("operator:{delegator}"),
                    "relation": "can_act_as",
                    "object": format!("agent:{agent}"),
                },
                "correlation_id": "c0",
            },
            {
                "tuple_key": {
                    "user": format!("agent:{agent}"),
                    "relation": "use",
                    "object": format!("sandbox_workspace:{workspace_id}"),
                },
                "correlation_id": "c1",
            },
        ],
    })
}

async fn mount_check(server: &MockServer, body: Value, allowed: bool) -> wiremock::MockGuard {
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/stores/store-1/check"))
        .and(matchers::body_json(body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "allowed": allowed })))
        .expect(1)
        .mount_as_scoped(server)
        .await
}

async fn mount_delegated_batch(
    server: &MockServer,
    body: Value,
    delegation_allowed: bool,
    workspace_use_allowed: bool,
) -> wiremock::MockGuard {
    Mock::given(matchers::method("POST"))
        .and(matchers::path("/stores/store-1/batch-check"))
        .and(matchers::body_json(body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "result": {
                "c0": { "allowed": delegation_allowed },
                "c1": { "allowed": workspace_use_allowed },
            }
        })))
        .expect(1)
        .mount_as_scoped(server)
        .await
}

#[tokio::test]
async fn sandbox_workspace_direct_operator_and_contacts_use_exact_resource_relations_db() {
    // Pins: direct operators ask for the exact requested manage/use relation,
    // while two contacts in one tenant remain different OpenFGA subjects.
    let server = MockServer::start().await;
    let client = fga_client(&server);
    let tenant_id = TenantId::new();
    let operator_id = Uuid::new_v4();
    let contact_a_id = Uuid::new_v4();
    let contact_b_id = Uuid::new_v4();
    let manage_workspace = SandboxWorkspaceId::new();
    let use_workspace = SandboxWorkspaceId::new();
    let contact_workspace = SandboxWorkspaceId::new();

    let _operator_manage = mount_check(
        &server,
        check_body(
            &format!("operator:{operator_id}"),
            "manage",
            manage_workspace,
        ),
        true,
    )
    .await;
    require_authz(
        &client,
        &identity(IdentityType::Operator, operator_id, tenant_id, None),
        ObjectType::SandboxWorkspace,
        manage_workspace,
        Relation::Manage,
    )
    .await
    .expect("direct operator manage grant should authorize the workspace");

    let _operator_use = mount_check(
        &server,
        check_body(&format!("operator:{operator_id}"), "use", use_workspace),
        true,
    )
    .await;
    require_authz(
        &client,
        &identity(IdentityType::Operator, operator_id, tenant_id, None),
        ObjectType::SandboxWorkspace,
        use_workspace,
        Relation::Use,
    )
    .await
    .expect("direct operator use grant should authorize the workspace");

    let _contact_a = mount_check(
        &server,
        check_body(&format!("contact:{contact_a_id}"), "use", contact_workspace),
        true,
    )
    .await;
    require_authz(
        &client,
        &identity(IdentityType::Contact, contact_a_id, tenant_id, None),
        ObjectType::SandboxWorkspace,
        contact_workspace,
        Relation::Use,
    )
    .await
    .expect("the directly granted contact should use its workspace");

    let _contact_b = mount_check(
        &server,
        check_body(&format!("contact:{contact_b_id}"), "use", contact_workspace),
        false,
    )
    .await;
    let denied = require_authz(
        &client,
        &identity(IdentityType::Contact, contact_b_id, tenant_id, None),
        ObjectType::SandboxWorkspace,
        contact_workspace,
        Relation::Use,
    )
    .await
    .expect_err("a sibling contact in the same tenant must not inherit workspace use");
    assert!(matches!(
        denied,
        AuthzCheckError::Forbidden {
            subject,
            object_type: ObjectType::SandboxWorkspace,
            object_id,
            relation: Relation::Use,
        } if subject == format!("contact:{contact_b_id}")
            && object_id == contact_workspace.to_string()
    ));
}

#[tokio::test]
async fn sandbox_workspace_delegated_agent_requires_both_delegation_and_direct_use_db() {
    // Pins: a delegated agent cannot borrow its operator's workspace rights;
    // the exact can_act_as edge and an agent#use edge must both be allowed.
    let server = MockServer::start().await;
    let client = fga_client(&server);
    let tenant_id = TenantId::new();
    let delegator = Uuid::new_v4();

    for (delegation_allowed, use_allowed, expected_relation) in [
        (false, true, Some(Relation::CanActAs)),
        (true, false, Some(Relation::Use)),
        (true, true, None),
    ] {
        let agent_id = Uuid::new_v4();
        let workspace_id = SandboxWorkspaceId::new();
        let _batch = mount_delegated_batch(
            &server,
            delegated_batch_body(delegator, agent_id, workspace_id),
            delegation_allowed,
            use_allowed,
        )
        .await;
        let result = require_authz_with_delegation(
            &client,
            &identity(IdentityType::Agent, agent_id, tenant_id, Some(delegator)),
            ObjectType::SandboxWorkspace,
            workspace_id,
            Relation::Use,
        )
        .await;

        match expected_relation {
            None => result.expect("both delegation and direct workspace use should authorize"),
            Some(expected_relation) => {
                let error = result.expect_err("either missing edge must fail closed");
                assert!(matches!(
                    error,
                    AuthzCheckError::Forbidden { relation, .. } if relation == expected_relation
                ));
            }
        }
    }
}

#[tokio::test]
async fn sandbox_workspace_authz_create_rollback_retry_and_delete_are_atomic_db() -> Result<()> {
    // Pins: row, exact desired grants, and outbox tuples commit or roll back as
    // one unit, and deletion fences locally before inverse tuples are visible.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = TenantId(Uuid::new_v4());
    let workspace_id = SandboxWorkspaceId(Uuid::new_v4());
    let session_id = SessionId(Uuid::new_v4());
    let contact_id = Uuid::new_v4();
    let provider_account_id = ProviderAccountId(Uuid::new_v4());
    sqlx::query(
        r#"
        INSERT INTO moa.sandbox_provider_accounts (
            provider_account_id, generation, provider, isolation_cell,
            organization_fingerprint, health
        ) VALUES ($1, 1, 'test', $2, $3, 'healthy')
        "#,
    )
    .bind(provider_account_id)
    .bind(format!("cell-{}", Uuid::new_v4()))
    .bind(format!("org-{}", Uuid::new_v4()))
    .execute(&pool)
    .await?;

    let request = CreateWorkspaceRequest {
        workspace_id,
        tenant_id,
        scope: SandboxWorkspaceScope::Worker {
            session_id,
            worker_id: "worker-1".to_string(),
        },
        provider: "test".to_string(),
        provider_account_id,
        provider_account_generation: 1,
        durability_class: DurabilityClass::PortableFilesystem,
        retention_deadline_at: None,
    };
    let grants = vec![
        grant(
            WorkspaceGrantSubjectType::Tenant,
            tenant_id.0,
            WorkspaceGrantRelation::Tenant,
        ),
        grant(
            WorkspaceGrantSubjectType::Session,
            session_id.0,
            WorkspaceGrantRelation::Session,
        ),
        grant(
            WorkspaceGrantSubjectType::Contact,
            contact_id,
            WorkspaceGrantRelation::Owner,
        ),
        grant(
            WorkspaceGrantSubjectType::Contact,
            contact_id,
            WorkspaceGrantRelation::Use,
        ),
    ];
    let repository = PostgresWorkspaceRepository::new(pool.clone());

    let mut rollback = repository.begin_transaction(tenant_id).await?;
    PostgresWorkspaceRepository::create_with_grants_in_transaction(
        rollback.as_mut(),
        &request,
        &grants,
    )
    .await?;
    enqueue(
        rollback.as_mut(),
        tenant_id,
        workspace_id,
        TupleOp::Write,
        &grants,
    )
    .await?;
    rollback.rollback().await?;
    assert_eq!(workspace_count(&pool, workspace_id).await?, 0);
    assert_eq!(outbox_count(&pool, workspace_id).await?, 0);

    let mut create = repository.begin_transaction(tenant_id).await?;
    PostgresWorkspaceRepository::create_with_grants_in_transaction(
        create.as_mut(),
        &request,
        &grants,
    )
    .await?;
    enqueue(
        create.as_mut(),
        tenant_id,
        workspace_id,
        TupleOp::Write,
        &grants,
    )
    .await?;
    create.commit().await?;

    assert_eq!(workspace_count(&pool, workspace_id).await?, 1);
    assert_eq!(outbox_count(&pool, workspace_id).await?, 4);
    let foreign = repository
        .get(TenantId(Uuid::new_v4()), workspace_id)
        .await?;
    assert_eq!(
        foreign, None,
        "cross-tenant workspace IDs must be invisible"
    );

    assert!(
        repository
            .transition(WorkspaceTransition {
                tenant_id,
                workspace_id,
                from: moa_core::types::sandbox_workspace::SandboxWorkspaceState::Creating,
                to: moa_core::types::sandbox_workspace::SandboxWorkspaceState::Ready,
                writer_epoch: 0,
                instance_generation: 0,
            })
            .await?,
        "provider-create completion should make the workspace deletable"
    );
    let current = repository
        .get(tenant_id, workspace_id)
        .await?
        .expect("created workspace should exist");
    let mut delete = repository.begin_transaction(tenant_id).await?;
    let (fenced, inverse) =
        PostgresWorkspaceRepository::fence_for_deletion_with_grants_in_transaction(
            delete.as_mut(),
            tenant_id,
            workspace_id,
            current.writer_epoch,
            current.instance_generation,
        )
        .await?
        .expect("workspace should fence once");
    enqueue(
        delete.as_mut(),
        tenant_id,
        workspace_id,
        TupleOp::Delete,
        &inverse,
    )
    .await?;
    delete.commit().await?;
    assert!(fenced.access_fenced_at.is_some());

    let states: Vec<(String, i64, String)> = sqlx::query_as(
        r#"
        SELECT desired_state, tuple_generation, outbox_state
        FROM moa.sandbox_workspace_grants
        WHERE workspace_id = $1
        ORDER BY relation, subject_type
        "#,
    )
    .bind(workspace_id)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        states,
        vec![("absent".to_string(), 2, "pending".to_string()); 4]
    );
    let outbox: Vec<(String, i64, i32)> = sqlx::query_as(
        r#"
        SELECT op, generation, model_version
        FROM authz_outbox
        WHERE tuple_object = $1
        ORDER BY tuple_relation, tuple_user
        "#,
    )
    .bind(format!("sandbox_workspace:{workspace_id}"))
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        outbox,
        vec![("delete".to_string(), 2, MODEL_VERSION as i32); 4]
    );
    Ok(())
}

fn grant(
    subject_type: WorkspaceGrantSubjectType,
    subject_id: Uuid,
    relation: WorkspaceGrantRelation,
) -> WorkspaceGrant {
    WorkspaceGrant {
        grant_id: Uuid::new_v4(),
        subject_type,
        subject_id,
        subject_relation: None,
        relation,
    }
}

async fn enqueue(
    conn: &mut sqlx::PgConnection,
    tenant_id: TenantId,
    workspace_id: SandboxWorkspaceId,
    op: TupleOp,
    grants: &[WorkspaceGrant],
) -> Result<()> {
    for grant in grants {
        enqueue_raw(
            &mut *conn,
            op,
            &grant.subject_wire(),
            grant.relation.as_str(),
            &format!("sandbox_workspace:{workspace_id}"),
            Some(tenant_id.0),
        )
        .await?;
    }
    Ok(())
}

async fn workspace_count(pool: &sqlx::PgPool, workspace_id: SandboxWorkspaceId) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM moa.sandbox_workspaces WHERE workspace_id = $1")
            .bind(workspace_id)
            .fetch_one(pool)
            .await?,
    )
}

async fn outbox_count(pool: &sqlx::PgPool, workspace_id: SandboxWorkspaceId) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM authz_outbox WHERE tuple_object = $1")
            .bind(format!("sandbox_workspace:{workspace_id}"))
            .fetch_one(pool)
            .await?,
    )
}
