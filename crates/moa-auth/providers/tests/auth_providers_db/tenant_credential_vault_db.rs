//! Durable tenant credential vault behavior against an isolated Postgres schema.

use std::sync::Arc;

use moa_auth_providers::PostgresCredentialVault;
use moa_core::traits::CredentialVault;
use moa_core::types::credentials::{
    CredentialContext, CredentialError, CredentialIdentity, CredentialKind, CredentialOperation,
    CredentialPrincipal, CredentialRef, CredentialServiceActor, CredentialSource, DeploymentSecret,
    DeploymentSecrets,
};
use moa_core::types::identifiers::TenantId;
use moa_crypto::{KeyManagementProvider, LocalKmsProvider};
use secrecy::SecretString;
use uuid::Uuid;

use super::support::TestDatabase;

fn kms() -> Arc<dyn KeyManagementProvider> {
    Arc::new(LocalKmsProvider::new())
}

fn caller(identity_id: Uuid) -> CredentialPrincipal {
    CredentialPrincipal::Caller {
        identity_id,
        delegated_by: None,
    }
}

fn context(
    tenant_id: TenantId,
    principal: CredentialPrincipal,
    operation: CredentialOperation,
    operation_id: &str,
    request_hash: &str,
) -> CredentialContext {
    CredentialContext {
        tenant_id,
        principal,
        operation,
        operation_id: operation_id.to_string(),
        request_hash: request_hash.to_string(),
    }
}

fn identity(tenant_id: TenantId, connection_uid: Uuid) -> CredentialIdentity {
    CredentialIdentity {
        tenant_id,
        connection_uid,
        kind: CredentialKind::ProviderApiKey,
    }
}

#[tokio::test]
async fn credential_created_on_one_pool_resolves_through_an_independent_pool_db() {
    // Pins: the vault is a durable owner, not process-local state. A credential
    // written through one pool resolves byte-identically through a separate pool
    // against the same database, which is what makes replica A -> replica B and
    // post-restart workflow reconstruction work.
    let database = TestDatabase::new("cred_replica").await;
    // Both replicas share one KMS, as they do in production: the keyring is
    // shared Postgres state, not per-process material. Two independent
    // LocalKmsProvider instances would model a split-brain keyring instead.
    let shared_kms = kms();
    let writer = PostgresCredentialVault::new(database.pool(), Arc::clone(&shared_kms));
    let reader =
        PostgresCredentialVault::new(database.independent_pool().await, Arc::clone(&shared_kms));

    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let created = writer
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from("merge_live_key_replica_case".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "op-create-replica",
                "hash-create-replica",
            ),
        )
        .await
        .expect("create credential");

    let resolved = reader
        .resolve(
            &CredentialSource::TenantConnection {
                reference: created.reference,
            },
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "op-resolve-replica",
                "hash-resolve-replica",
            ),
        )
        .await
        .expect("resolve credential from an independent pool");

    assert_eq!(
        resolved.expose_for_outbound_request(),
        "merge_live_key_replica_case"
    );
    assert_eq!(created.version, 1);
    assert!(created.active);
    assert!(!created.revoked);
}

#[tokio::test]
async fn replayed_operation_id_returns_one_row_and_a_changed_hash_conflicts_db() {
    // Pins: replay safety. The same operation id with the same request hash
    // replays exactly one audit row and one credential version; the same id with
    // different material is a typed conflict rather than a second secret.
    let database = TestDatabase::new("cred_replay").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let ctx = context(
        tenant_id,
        caller(owner),
        CredentialOperation::Create,
        "op-create-replay",
        "hash-create-replay",
    );

    let first = vault
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from("first_material".to_string()),
            &ctx,
        )
        .await
        .expect("first create");
    let replayed = vault
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from("first_material".to_string()),
            &ctx,
        )
        .await
        .expect("replayed create returns the original outcome");

    assert_eq!(first.reference, replayed.reference);
    assert_eq!(replayed.version, 1);

    let audit_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tenant_credential_operations
         WHERE tenant_id = $1 AND operation_id = 'op-create-replay'",
    )
    .bind(tenant_id.0)
    .fetch_one(database.raw_pool())
    .await
    .expect("count audit rows");
    assert_eq!(audit_rows, 1, "replay must not append a second audit row");

    let versions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM tenant_credential_versions WHERE tenant_id = $1")
            .bind(tenant_id.0)
            .fetch_one(database.raw_pool())
            .await
            .expect("count versions");
    assert_eq!(versions, 1, "replay must not store a second version");

    let conflict = vault
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from("different_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "op-create-replay",
                "hash-create-DIFFERENT",
            ),
        )
        .await
        .expect_err("reusing an operation id with different inputs must conflict");
    assert_eq!(conflict, CredentialError::IdempotencyConflict);
}

#[tokio::test]
async fn rotation_supersedes_under_cas_and_old_versions_stop_resolving_db() {
    // Pins: rotation is compare-and-swap. The superseded version can no longer be
    // resolved, a second rotation from the stale reference is refused rather than
    // silently losing the newer credential, and only one version stays active.
    let database = TestDatabase::new("cred_rotate").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();

    let first = vault
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from("v1_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "op-create-rotate",
                "hash-create-rotate",
            ),
        )
        .await
        .expect("create v1");

    let second = vault
        .rotate(
            first.reference,
            SecretString::from("v2_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Rotate,
                "op-rotate-1",
                "hash-rotate-1",
            ),
        )
        .await
        .expect("rotate to v2");
    assert_eq!(second.version, 2);

    let stale = vault
        .resolve(
            &CredentialSource::TenantConnection {
                reference: first.reference,
            },
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "op-resolve-stale",
                "hash-resolve-stale",
            ),
        )
        .await
        .expect_err("the superseded version must not resolve");
    assert_eq!(stale, CredentialError::StaleVersion);

    let lost_update = vault
        .rotate(
            first.reference,
            SecretString::from("v2_conflicting".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Rotate,
                "op-rotate-2",
                "hash-rotate-2",
            ),
        )
        .await
        .expect_err("rotating from a stale reference must not overwrite the newer version");
    assert_eq!(lost_update, CredentialError::VersionConflict);

    let active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tenant_credential_versions WHERE tenant_id = $1 AND active",
    )
    .bind(tenant_id.0)
    .fetch_one(database.raw_pool())
    .await
    .expect("count active versions");
    assert_eq!(active, 1, "exactly one version may be active");

    let current = vault
        .resolve(
            &CredentialSource::TenantConnection {
                reference: second.reference,
            },
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "op-resolve-current",
                "hash-resolve-current",
            ),
        )
        .await
        .expect("the active version resolves");
    assert_eq!(current.expose_for_outbound_request(), "v2_material");
}

#[tokio::test]
async fn revoked_and_cross_tenant_references_fail_closed_db() {
    // Pins: a revoked version is unusable, and a reference belonging to another
    // tenant fails with a typed error before any material is opened — the caller
    // cannot learn anything about a credential it does not own.
    let database = TestDatabase::new("cred_deny").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let other_tenant = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();

    let created = vault
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from("revocable_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "op-create-deny",
                "hash-create-deny",
            ),
        )
        .await
        .expect("create credential");

    let cross_tenant = vault
        .resolve(
            &CredentialSource::TenantConnection {
                reference: created.reference,
            },
            &context(
                other_tenant,
                caller(Uuid::now_v7()),
                CredentialOperation::Resolve,
                "op-resolve-cross",
                "hash-resolve-cross",
            ),
        )
        .await
        .expect_err("another tenant must not resolve this credential");
    assert_eq!(cross_tenant, CredentialError::NotFound);

    vault
        .revoke(
            created.reference,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Revoke,
                "op-revoke",
                "hash-revoke",
            ),
        )
        .await
        .expect("revoke credential");

    let revoked = vault
        .resolve(
            &CredentialSource::TenantConnection {
                reference: created.reference,
            },
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "op-resolve-revoked",
                "hash-resolve-revoked",
            ),
        )
        .await
        .expect_err("a revoked version must not resolve");
    assert_eq!(revoked, CredentialError::Revoked);
}

#[tokio::test]
async fn service_actors_may_resolve_but_never_mutate_db() {
    // Pins: the durable service-actor allowlist is read-only, so a reconstructed
    // workflow can resolve its own connection's credential without acquiring the
    // ability to create, rotate, revoke, or delete one.
    let database = TestDatabase::new("cred_actor").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let service = CredentialPrincipal::Service {
        actor: CredentialServiceActor::KnowledgeSyncListing,
    };

    let created = vault
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from("service_actor_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "op-create-actor",
                "hash-create-actor",
            ),
        )
        .await
        .expect("create credential");

    let resolved = vault
        .resolve(
            &CredentialSource::TenantConnection {
                reference: created.reference,
            },
            &context(
                tenant_id,
                service,
                CredentialOperation::Resolve,
                "op-resolve-actor",
                "hash-resolve-actor",
            ),
        )
        .await
        .expect("the sync-listing actor may resolve");
    assert_eq!(
        resolved.expose_for_outbound_request(),
        "service_actor_material"
    );

    let denied = vault
        .revoke(
            created.reference,
            &context(
                tenant_id,
                service,
                CredentialOperation::Revoke,
                "op-revoke-actor",
                "hash-revoke-actor",
            ),
        )
        .await
        .expect_err("a service actor must not revoke");
    assert_eq!(denied, CredentialError::Unauthorized);
}

#[tokio::test]
async fn forced_rls_denies_missing_and_wrong_tenant_context_as_moa_app_db() {
    // Pins: forced row-level security is proven through the production role, not
    // an owner-URL read. As `moa_app`, a correct `moa.tenant_id` sees the row
    // while a missing or wrong one sees nothing, on both the credential table and
    // its audit projection.
    let database = TestDatabase::new("cred_rls").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();

    vault
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from("rls_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "op-create-rls",
                "hash-create-rls",
            ),
        )
        .await
        .expect("create credential");

    for (label, tenant_setting) in [
        ("correct", Some(tenant_id.0.to_string())),
        ("missing", None),
        ("wrong", Some(Uuid::now_v7().to_string())),
    ] {
        let mut tx = database
            .raw_pool()
            .begin()
            .await
            .expect("begin rls probe transaction");
        if let Some(value) = tenant_setting.as_ref() {
            sqlx::query("SELECT set_config('moa.tenant_id', $1, true)")
                .bind(value)
                .execute(&mut *tx)
                .await
                .expect("apply tenant guc");
        }
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(&mut *tx)
            .await
            .expect("assume moa_app");

        let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tenant_credential_versions")
            .fetch_one(&mut *tx)
            .await
            .expect("count visible credential rows");
        let audit: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tenant_credential_operations")
            .fetch_one(&mut *tx)
            .await
            .expect("count visible audit rows");
        tx.rollback().await.expect("rollback rls probe");

        let expected = i64::from(label == "correct");
        assert_eq!(
            versions, expected,
            "{label} tenant context must see {expected} credential row(s)"
        );
        assert_eq!(
            audit, expected,
            "{label} tenant context must see {expected} audit row(s)"
        );
    }
}

#[tokio::test]
async fn audit_rows_are_append_only_and_carry_no_secret_material_db() {
    // Pins: the audit is a secret-free, append-only projection. `moa_app` cannot
    // rewrite history, and no stored column contains plaintext or ciphertext.
    let database = TestDatabase::new("cred_audit").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let plaintext = "audit_scan_plaintext_marker";

    let created = vault
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from(plaintext.to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "op-create-audit",
                "hash-create-audit",
            ),
        )
        .await
        .expect("create credential");
    vault
        .resolve(
            &CredentialSource::TenantConnection {
                reference: created.reference,
            },
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "op-resolve-audit",
                "hash-resolve-audit",
            ),
        )
        .await
        .expect("resolve credential");

    let rendered: String = sqlx::query_scalar(
        "SELECT COALESCE(string_agg(t::TEXT, ' '), '') FROM tenant_credential_operations t
         WHERE tenant_id = $1",
    )
    .bind(tenant_id.0)
    .fetch_one(database.raw_pool())
    .await
    .expect("render audit rows");
    assert!(
        !rendered.contains(plaintext),
        "audit rows must never contain plaintext"
    );
    assert!(rendered.contains("resolve"), "the resolve must be audited");
    assert!(rendered.contains("create"), "the create must be audited");

    let sealed: Vec<u8> = sqlx::query_scalar(
        "SELECT material_sealed FROM tenant_credential_versions WHERE credential_uid = $1",
    )
    .bind(created.reference.as_uuid())
    .fetch_one(database.raw_pool())
    .await
    .expect("read sealed material");
    assert!(
        !String::from_utf8_lossy(&sealed).contains(plaintext),
        "stored material must be ciphertext, not plaintext"
    );

    let mut tx = database
        .raw_pool()
        .begin()
        .await
        .expect("begin append-only probe");
    sqlx::query("SELECT set_config('moa.tenant_id', $1, true)")
        .bind(tenant_id.0.to_string())
        .execute(&mut *tx)
        .await
        .expect("apply tenant guc");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut *tx)
        .await
        .expect("assume moa_app");
    let rewrite = sqlx::query("UPDATE tenant_credential_operations SET outcome = 'denied'")
        .execute(&mut *tx)
        .await;
    tx.rollback().await.expect("rollback append-only probe");
    assert!(
        rewrite.is_err(),
        "the application role must not rewrite audit history"
    );
}

#[tokio::test]
async fn connection_delete_removes_versions_and_audit_and_is_idempotent_db() {
    // Pins: tenant lifecycle deletion removes every version plus its permitted
    // audit projection, and repeating the sweep for an already-purged connection
    // succeeds without removing anything further.
    let database = TestDatabase::new("cred_purge").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();

    let created = vault
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from("purge_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "op-create-purge",
                "hash-create-purge",
            ),
        )
        .await
        .expect("create credential");
    vault
        .rotate(
            created.reference,
            SecretString::from("purge_material_v2".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Rotate,
                "op-rotate-purge",
                "hash-rotate-purge",
            ),
        )
        .await
        .expect("rotate credential");

    let removed = vault
        .delete_connection(
            connection_uid,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Delete,
                "op-delete-purge",
                "hash-delete-purge",
            ),
        )
        .await
        .expect("delete connection credentials");
    assert_eq!(removed, 2, "both versions must be removed");

    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM tenant_credential_versions WHERE tenant_id = $1")
            .bind(tenant_id.0)
            .fetch_one(database.raw_pool())
            .await
            .expect("count remaining versions");
    assert_eq!(remaining, 0);

    let remaining_audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tenant_credential_operations
         WHERE tenant_id = $1 AND connection_uid = $2",
    )
    .bind(tenant_id.0)
    .bind(connection_uid)
    .fetch_one(database.raw_pool())
    .await
    .expect("count remaining audit rows");
    assert_eq!(remaining_audit, 0);

    let repeated = vault
        .delete_connection(
            connection_uid,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Delete,
                "op-delete-purge-again",
                "hash-delete-purge-again",
            ),
        )
        .await
        .expect("repeating the purge succeeds");
    assert_eq!(repeated, 0, "an already-purged connection removes nothing");
}

#[tokio::test]
async fn deployment_secrets_resolve_outside_tenant_storage_db() {
    // Pins: deployment-owned transport secrets are a separate typed source. They
    // resolve without touching tenant credential storage, and an unconfigured one
    // fails closed instead of falling back to a tenant credential.
    let database = TestDatabase::new("cred_deployment").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms()).with_deployment_secrets(
        DeploymentSecrets::new().with(
            DeploymentSecret::PostmarkServerToken,
            Some("postmark-server-token".to_string()),
        ),
    );
    let tenant_id = TenantId::from(Uuid::now_v7());
    let owner = Uuid::now_v7();

    let resolved = vault
        .resolve(
            &CredentialSource::Deployment {
                secret: DeploymentSecret::PostmarkServerToken,
            },
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "op-resolve-deployment",
                "hash-resolve-deployment",
            ),
        )
        .await
        .expect("configured deployment secret resolves");
    assert_eq!(
        resolved.expose_for_outbound_request(),
        "postmark-server-token"
    );

    let missing = vault
        .resolve(
            &CredentialSource::Deployment {
                secret: DeploymentSecret::TwilioAuthToken,
            },
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "op-resolve-deployment-missing",
                "hash-resolve-deployment-missing",
            ),
        )
        .await
        .expect_err("an unconfigured deployment secret must fail closed");
    assert_eq!(missing, CredentialError::DeploymentSecretMissing);

    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tenant_credential_versions")
        .fetch_one(database.raw_pool())
        .await
        .expect("count tenant credential rows");
    assert_eq!(
        stored, 0,
        "deployment secrets must not create tenant credential rows"
    );
}

#[tokio::test]
async fn wrong_kind_reference_does_not_open_material_db() {
    // Pins: the credential kind is part of the persistence identity and is bound
    // into the ciphertext, so a row relabelled to another kind cannot be opened
    // even by a caller inside the owning tenant.
    let database = TestDatabase::new("cred_kind").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();

    let created = vault
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from("kind_bound_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "op-create-kind",
                "hash-create-kind",
            ),
        )
        .await
        .expect("create credential");

    sqlx::query(
        "UPDATE tenant_credential_versions SET kind = 'mcp_bearer' WHERE credential_uid = $1",
    )
    .bind(created.reference.as_uuid())
    .execute(database.raw_pool())
    .await
    .expect("relabel the stored kind");

    let error = vault
        .resolve(
            &CredentialSource::TenantConnection {
                reference: created.reference,
            },
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "op-resolve-kind",
                "hash-resolve-kind",
            ),
        )
        .await
        .expect_err("a relabelled credential must not open");
    assert!(
        matches!(error, CredentialError::Storage(_)),
        "expected an authenticated-decryption failure, got {error:?}"
    );
}

#[tokio::test]
async fn unknown_reference_reports_not_found_without_probing_other_tenants_db() {
    // Pins: an unknown reference is indistinguishable from another tenant's
    // reference, so the error surface cannot be used to probe for existence.
    let database = TestDatabase::new("cred_unknown").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());

    let error = vault
        .resolve(
            &CredentialSource::TenantConnection {
                reference: CredentialRef::from_uuid(Uuid::now_v7()),
            },
            &context(
                tenant_id,
                caller(Uuid::now_v7()),
                CredentialOperation::Resolve,
                "op-resolve-unknown",
                "hash-resolve-unknown",
            ),
        )
        .await
        .expect_err("an unknown reference must not resolve");
    assert_eq!(error, CredentialError::NotFound);
}

#[tokio::test]
async fn bounded_tenant_purge_loops_to_completion_and_is_resumable_db() {
    // Pins: the tenant sweep is a bounded batch the purge workflow loops until 0.
    // Each call removes at most `limit` rows, versions drain before their audit
    // projection so a crash never orphans history ahead of the version it
    // describes, another tenant is untouched, and the loop terminates exactly
    // once both tables are empty.
    let database = TestDatabase::new("cred_tenant_purge").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let other_tenant = TenantId::from(Uuid::now_v7());
    let owner = Uuid::now_v7();

    for index in 0..5 {
        vault
            .create(
                identity(tenant_id, Uuid::now_v7()),
                SecretString::from(format!("material_{index}")),
                &context(
                    tenant_id,
                    caller(owner),
                    CredentialOperation::Create,
                    &format!("op-create-sweep-{index}"),
                    &format!("hash-create-sweep-{index}"),
                ),
            )
            .await
            .expect("create credential");
    }
    let survivor = vault
        .create(
            identity(other_tenant, Uuid::now_v7()),
            SecretString::from("other_tenant_material".to_string()),
            &context(
                other_tenant,
                caller(Uuid::now_v7()),
                CredentialOperation::Create,
                "op-create-survivor",
                "hash-create-survivor",
            ),
        )
        .await
        .expect("create another tenant's credential");

    // Batch size 2 against 5 versions + 5 audit rows: several bounded calls.
    let mut batches = Vec::new();
    for round in 0..20 {
        let removed = vault
            .purge_tenant(
                2,
                &context(
                    tenant_id,
                    caller(owner),
                    CredentialOperation::Delete,
                    &format!("op-sweep-{round}"),
                    &format!("hash-sweep-{round}"),
                ),
            )
            .await
            .expect("bounded tenant purge");
        batches.push(removed);
        if removed == 0 {
            break;
        }
    }

    assert!(
        batches.iter().all(|removed| *removed <= 2),
        "every batch must respect the limit, got {batches:?}"
    );
    assert_eq!(
        batches.last().copied(),
        Some(0),
        "the loop must terminate on a zero batch, got {batches:?}"
    );
    assert_eq!(
        batches.iter().sum::<u64>(),
        10,
        "5 versions plus 5 audit rows must all be removed, got {batches:?}"
    );

    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM tenant_credential_versions WHERE tenant_id = $1")
            .bind(tenant_id.0)
            .fetch_one(database.raw_pool())
            .await
            .expect("count remaining versions");
    assert_eq!(remaining, 0);

    let remaining_audit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tenant_credential_operations WHERE tenant_id = $1",
    )
    .bind(tenant_id.0)
    .fetch_one(database.raw_pool())
    .await
    .expect("count remaining audit rows");
    assert_eq!(remaining_audit, 0);

    let survivor_resolved = vault
        .resolve(
            &CredentialSource::TenantConnection {
                reference: survivor.reference,
            },
            &context(
                other_tenant,
                caller(Uuid::now_v7()),
                CredentialOperation::Resolve,
                "op-resolve-survivor",
                "hash-resolve-survivor",
            ),
        )
        .await
        .expect("another tenant's credential must survive the sweep");
    assert_eq!(
        survivor_resolved.expose_for_outbound_request(),
        "other_tenant_material"
    );
}

#[tokio::test]
async fn tenant_purge_requires_the_delete_scoped_context_db() {
    // Pins: the sweep is reachable only through the narrow purge lifecycle path.
    // A context carrying any other operation is refused before any row is
    // touched, so an ordinary resolve-scoped caller cannot erase tenant state.
    let database = TestDatabase::new("cred_purge_scope").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let owner = Uuid::now_v7();

    vault
        .create(
            identity(tenant_id, Uuid::now_v7()),
            SecretString::from("scoped_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "op-create-scope",
                "hash-create-scope",
            ),
        )
        .await
        .expect("create credential");

    let denied = vault
        .purge_tenant(
            10,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "op-sweep-wrong-scope",
                "hash-sweep-wrong-scope",
            ),
        )
        .await
        .expect_err("a non-delete context must not purge");
    assert_eq!(denied, CredentialError::Unauthorized);

    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM tenant_credential_versions WHERE tenant_id = $1")
            .bind(tenant_id.0)
            .fetch_one(database.raw_pool())
            .await
            .expect("count remaining versions");
    assert_eq!(remaining, 1, "the refused sweep must not remove anything");
}
