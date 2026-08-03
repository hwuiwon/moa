//! Durable tenant credential vault behavior against an isolated Postgres schema.

use std::sync::Arc;

use moa_auth_providers::PostgresCredentialVault;
use moa_core::traits::CredentialVault;
use moa_core::types::credentials::{
    CredentialContext, CredentialError, CredentialIdentity, CredentialKind, CredentialOperation,
    CredentialPrincipal, CredentialRef, CredentialServiceActor, CredentialSlotName,
    CredentialSource, CredentialStagingToken, CredentialVersion, DeploymentSecret,
    DeploymentSecrets,
};
use moa_core::types::identifiers::TenantId;
use moa_crypto::{EncryptionContext, KeyManagementProvider, LocalKmsProvider};
use secrecy::SecretString;
use uuid::Uuid;

use super::support::TestDatabase;

type CredentialOperationAuditFact = (
    String,
    Option<Uuid>,
    Option<Uuid>,
    Option<String>,
    Option<String>,
    Option<i64>,
);

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
    identity_in_slot(tenant_id, connection_uid, CredentialSlotName::PRIMARY)
}

fn identity_in_slot(
    tenant_id: TenantId,
    connection_uid: Uuid,
    slot_name: CredentialSlotName,
) -> CredentialIdentity {
    CredentialIdentity {
        tenant_id,
        connection_uid,
        kind: CredentialKind::ProviderApiKey,
        slot_name,
    }
}

async fn describe_version(
    vault: &PostgresCredentialVault,
    tenant_id: TenantId,
    owner: Uuid,
    connection_uid: Uuid,
    reference: CredentialRef,
) -> CredentialVersion {
    let described = vault
        .describe_batch(
            &[(connection_uid, reference)],
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "describe-staged-version",
                "describe-staged-version-hash",
            ),
        )
        .await
        .expect("describe exact staged version");
    assert_eq!(described.len(), 1, "exact staged reference must be visible");
    described
        .into_iter()
        .next()
        .expect("one exact description")
        .1
}

#[tokio::test]
async fn active_status_is_exact_audited_and_never_returns_a_reference_db() {
    // Pins: connector management can inspect one authorized credential series
    // without opening material or learning a reference/version, while each
    // readiness check records only its exact secret-free selector.
    let database = TestDatabase::new("cred_active_status").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let other_tenant = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let series = identity(tenant_id, connection_uid);

    let missing_ctx = context(
        tenant_id,
        caller(owner),
        CredentialOperation::Resolve,
        "active-status-missing",
        "active-status-missing-hash",
    );
    assert!(
        !vault
            .has_active(&series, &missing_ctx)
            .await
            .expect("missing exact series should report false")
    );
    vault
        .create(
            series.clone(),
            SecretString::from("active_status_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "active-status-create",
                "active-status-create-hash",
            ),
        )
        .await
        .expect("create active status fixture");
    assert!(
        vault
            .has_active(
                &series,
                &context(
                    tenant_id,
                    caller(owner),
                    CredentialOperation::Resolve,
                    "active-status-present",
                    "active-status-present-hash",
                ),
            )
            .await
            .expect("active exact series should report true")
    );
    let wrong_operation = vault
        .has_active(
            &series,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Revoke,
                "active-status-wrong-operation",
                "active-status-wrong-operation-hash",
            ),
        )
        .await
        .expect_err("readiness requires an exact resolve context");
    assert_eq!(wrong_operation, CredentialError::Unauthorized);
    let wrong_tenant = vault
        .has_active(
            &series,
            &context(
                other_tenant,
                caller(owner),
                CredentialOperation::Resolve,
                "active-status-wrong-tenant",
                "active-status-wrong-tenant-hash",
            ),
        )
        .await
        .expect_err("readiness cannot cross tenant scope");
    assert_eq!(wrong_tenant, CredentialError::WrongTenant);

    let audit: Vec<CredentialOperationAuditFact> = sqlx::query_as(
        "SELECT operation_id, credential_uid, connection_uid, kind, slot_name, version \
         FROM tenant_credential_operations \
         WHERE tenant_id = $1 AND operation_id LIKE 'active-status-%' \
           AND operation = 'resolve' ORDER BY operation_id",
    )
    .bind(tenant_id.0)
    .fetch_all(database.raw_pool())
    .await
    .expect("read status audit rows");
    assert_eq!(
        audit,
        vec![
            (
                "active-status-missing".to_string(),
                None,
                Some(connection_uid),
                Some("provider_api_key".to_string()),
                Some("primary".to_string()),
                None,
            ),
            (
                "active-status-present".to_string(),
                None,
                Some(connection_uid),
                Some("provider_api_key".to_string()),
                Some("primary".to_string()),
                None,
            ),
        ]
    );
}

#[tokio::test]
async fn active_status_batch_is_tenant_scoped_and_preserves_input_positions_db() {
    // Pins: knowledge status checks select exact managed credential identities
    // in one secret-free query, including duplicates, without returning vault
    // references or collapsing positional results.
    let database = TestDatabase::new("cred_active_status_batch").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let other_tenant = TenantId::from(Uuid::now_v7());
    let owner = Uuid::now_v7();
    let active = identity(tenant_id, Uuid::now_v7());
    let missing = identity(tenant_id, Uuid::now_v7());

    vault
        .create(
            active.clone(),
            SecretString::from("batch_active_status_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "active-status-batch-create",
                "active-status-batch-create-hash",
            ),
        )
        .await
        .expect("create batch readiness fixture");

    let statuses = vault
        .has_active_batch(
            &[active.clone(), missing, active.clone()],
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "active-status-batch",
                "active-status-batch-hash",
            ),
        )
        .await
        .expect("read exact batch readiness");
    assert_eq!(statuses, vec![true, false, true]);

    let mut cross_tenant = active;
    cross_tenant.tenant_id = other_tenant;
    let error = vault
        .has_active_batch(
            &[cross_tenant],
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "active-status-batch-cross-tenant",
                "active-status-batch-cross-tenant-hash",
            ),
        )
        .await
        .expect_err("batch readiness must reject mixed tenant selectors");
    assert_eq!(error, CredentialError::WrongTenant);
}

#[tokio::test]
async fn initial_stage_is_inactive_until_replay_safe_activation_db() {
    // Pins: connector credential ingress may durably seal material before the
    // connection generation fence, but no initial credential becomes usable
    // until the separate activation CAS commits.
    let database = TestDatabase::new("cred_initial_stage").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let primary_identity = identity(tenant_id, connection_uid);
    let stage_ctx = context(
        tenant_id,
        caller(owner),
        CredentialOperation::Stage,
        "initial-stage",
        "initial-stage-hash",
    );

    let wrong_operation = vault
        .stage(
            primary_identity.clone(),
            SecretString::from("must_not_be_stored".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "initial-stage-wrong-operation",
                "initial-stage-wrong-operation-hash",
            ),
        )
        .await
        .expect_err("stage must reject a create context");
    assert_eq!(wrong_operation, CredentialError::Unauthorized);

    let staged = vault
        .stage(
            primary_identity.clone(),
            SecretString::from("initial_staged_material".to_string()),
            &stage_ctx,
        )
        .await
        .expect("stage initial credential");
    assert_eq!(staged.identity(), &primary_identity);
    assert_eq!(staged.version(), 1);
    assert_eq!(staged.expected_prior_active(), None);

    let stored = describe_version(
        &vault,
        tenant_id,
        owner,
        connection_uid,
        staged.staged_reference(),
    )
    .await;
    assert!(!stored.active, "staging must not activate material");
    assert!(!stored.revoked);
    let unresolved = vault
        .resolve_active(
            &primary_identity,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "initial-stage-resolve-before-activation",
                "initial-stage-resolve-before-activation-hash",
            ),
        )
        .await
        .expect_err("inactive staged material must not resolve");
    assert_eq!(unresolved, CredentialError::NotFound);

    let replayed_stage = vault
        .stage(
            primary_identity,
            SecretString::from("initial_staged_material".to_string()),
            &stage_ctx,
        )
        .await
        .expect("replay initial stage");
    assert_eq!(replayed_stage, staged, "stage replay must return one token");

    let activate_ctx = context(
        tenant_id,
        caller(owner),
        CredentialOperation::Activate,
        "initial-activate",
        "initial-activate-hash",
    );
    let wrong_activation = vault
        .activate_staged(
            &staged,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Rotate,
                "initial-activate-wrong-operation",
                "initial-activate-wrong-operation-hash",
            ),
        )
        .await
        .expect_err("activation must reject a rotate context");
    assert_eq!(wrong_activation, CredentialError::Unauthorized);

    let activated = vault
        .activate_staged(&staged, &activate_ctx)
        .await
        .expect("activate initial staged credential");
    assert_eq!(activated.reference, staged.staged_reference());
    assert_eq!(activated.version, staged.version());
    assert!(activated.active);
    assert!(!activated.revoked);
    let replayed_activation = vault
        .activate_staged(&staged, &activate_ctx)
        .await
        .expect("replay initial activation");
    assert_eq!(replayed_activation, activated);
    let stage_replayed_after_activation = vault
        .stage(
            staged.identity().clone(),
            SecretString::from("initial_staged_material".to_string()),
            &stage_ctx,
        )
        .await
        .expect("stage replay remains stable after activation");
    assert_eq!(stage_replayed_after_activation, staged);

    let resolved = vault
        .resolve_active(
            staged.identity(),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "initial-stage-resolve-after-activation",
                "initial-stage-resolve-after-activation-hash",
            ),
        )
        .await
        .expect("activated staged material resolves");
    assert_eq!(
        resolved.expose_for_outbound_request(),
        "initial_staged_material"
    );
}

#[tokio::test]
async fn activation_rollback_restores_exact_prior_and_replays_one_audit_db() {
    // Pins: a post-activation generation-fence failure can compensate without
    // plaintext by revoking only the still-active candidate and restoring its
    // exact predecessor; stale candidates and another slot's refs fail closed.
    let database = TestDatabase::new("cred_activation_rollback_prior").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let series = identity(tenant_id, connection_uid);
    let oldest = vault
        .create(
            series.clone(),
            SecretString::from("rollback_oldest_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "rollback-oldest-create",
                "rollback-oldest-create-hash",
            ),
        )
        .await
        .expect("create oldest rollback version");
    let prior = vault
        .rotate(
            oldest.reference,
            SecretString::from("rollback_prior_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Rotate,
                "rollback-prior-rotate",
                "rollback-prior-rotate-hash",
            ),
        )
        .await
        .expect("rotate to exact rollback predecessor");
    let staged = vault
        .stage(
            series.clone(),
            SecretString::from("rollback_candidate_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Stage,
                "rollback-stage",
                "rollback-stage-hash",
            ),
        )
        .await
        .expect("stage rollback candidate");
    let candidate = vault
        .activate_staged(
            &staged,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Activate,
                "rollback-activate",
                "rollback-activate-hash",
            ),
        )
        .await
        .expect("activate rollback candidate");

    let successor = vault
        .stage(
            series.clone(),
            SecretString::from("rollback_successor_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Stage,
                "rollback-successor-stage",
                "rollback-successor-stage-hash",
            ),
        )
        .await
        .expect("stage successor that makes the candidate stale");
    let successor = vault
        .activate_staged(
            &successor,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Activate,
                "rollback-successor-activate",
                "rollback-successor-activate-hash",
            ),
        )
        .await
        .expect("activate successor that makes the candidate stale");
    let stale = vault
        .rollback_activation(
            candidate.reference,
            Some(prior.reference),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::RollbackActivation,
                "rollback-stale-candidate",
                "rollback-stale-candidate-hash",
            ),
        )
        .await
        .expect_err("a superseded activation cannot compensate a newer active version");
    assert_eq!(stale, CredentialError::StaleVersion);
    vault
        .rollback_activation(
            successor.reference,
            Some(candidate.reference),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::RollbackActivation,
                "rollback-successor-cleanup",
                "rollback-successor-cleanup-hash",
            ),
        )
        .await
        .expect("restore candidate after stale-candidate assertion");
    let mismatched = vault
        .rollback_activation(
            candidate.reference,
            Some(oldest.reference),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::RollbackActivation,
                "rollback-mismatched-prior",
                "rollback-mismatched-prior-hash",
            ),
        )
        .await
        .expect_err("an older same-series version is not the exact predecessor");
    assert_eq!(mismatched, CredentialError::VersionConflict);
    assert!(
        describe_version(
            &vault,
            tenant_id,
            owner,
            connection_uid,
            candidate.reference,
        )
        .await
        .active,
        "failed rollback attempts must leave the candidate active"
    );

    let rollback_ctx = context(
        tenant_id,
        caller(owner),
        CredentialOperation::RollbackActivation,
        "rollback-exact-prior",
        "rollback-exact-prior-hash",
    );
    let rolled_back = vault
        .rollback_activation(candidate.reference, Some(prior.reference), &rollback_ctx)
        .await
        .expect("rollback exact activation");
    assert_eq!(rolled_back.reference, candidate.reference);
    assert!(!rolled_back.active);
    assert!(rolled_back.revoked);
    assert_eq!(
        vault
            .rollback_activation(candidate.reference, Some(prior.reference), &rollback_ctx)
            .await
            .expect("exact rollback retry replays"),
        rolled_back
    );

    let prior_after =
        describe_version(&vault, tenant_id, owner, connection_uid, prior.reference).await;
    assert!(prior_after.active);
    assert!(!prior_after.revoked);
    let resolved = vault
        .resolve_active(
            &series,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "rollback-resolve-restored-prior",
                "rollback-resolve-restored-prior-hash",
            ),
        )
        .await
        .expect("restored predecessor resolves");
    assert_eq!(
        resolved.expose_for_outbound_request(),
        "rollback_prior_material"
    );

    let audit: (Option<Uuid>, Option<Uuid>, String) = sqlx::query_as(
        "SELECT credential_uid, expected_prior_credential_uid, operation \
         FROM tenant_credential_operations \
         WHERE tenant_id = $1 AND operation_id = 'rollback-exact-prior'",
    )
    .bind(tenant_id.0)
    .fetch_one(database.raw_pool())
    .await
    .expect("read rollback audit");
    assert_eq!(
        audit,
        (
            Some(candidate.reference.as_uuid()),
            Some(prior.reference.as_uuid()),
            "rollback_activation".to_string(),
        )
    );
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tenant_credential_operations \
         WHERE tenant_id = $1 AND operation_id = 'rollback-exact-prior'",
    )
    .bind(tenant_id.0)
    .fetch_one(database.raw_pool())
    .await
    .expect("count rollback audit rows");
    assert_eq!(audit_count, 1);
}

#[tokio::test]
async fn initial_activation_rollback_revokes_candidate_and_leaves_series_inactive_db() {
    // Pins: compensating an initial create has no predecessor to restore; the
    // candidate is retained as revoked audit history and the series has no
    // active credential. An exact retry does not append another audit row.
    let database = TestDatabase::new("cred_activation_rollback_initial").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let series = identity(tenant_id, connection_uid);
    let staged = vault
        .stage(
            series.clone(),
            SecretString::from("rollback_initial_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Stage,
                "rollback-initial-stage",
                "rollback-initial-stage-hash",
            ),
        )
        .await
        .expect("stage initial candidate");
    let candidate = vault
        .activate_staged(
            &staged,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Activate,
                "rollback-initial-activate",
                "rollback-initial-activate-hash",
            ),
        )
        .await
        .expect("activate initial candidate");
    let rollback_ctx = context(
        tenant_id,
        caller(owner),
        CredentialOperation::RollbackActivation,
        "rollback-initial",
        "rollback-initial-hash",
    );
    let rolled_back = vault
        .rollback_activation(candidate.reference, None, &rollback_ctx)
        .await
        .expect("rollback initial activation");
    assert!(!rolled_back.active);
    assert!(rolled_back.revoked);
    assert_eq!(
        vault
            .rollback_activation(candidate.reference, None, &rollback_ctx)
            .await
            .expect("replay initial rollback"),
        rolled_back
    );
    assert!(
        !vault
            .has_active(
                &series,
                &context(
                    tenant_id,
                    caller(owner),
                    CredentialOperation::Resolve,
                    "rollback-initial-status",
                    "rollback-initial-status-hash",
                ),
            )
            .await
            .expect("read initial rollback status")
    );
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tenant_credential_operations \
         WHERE tenant_id = $1 AND operation_id = 'rollback-initial'",
    )
    .bind(tenant_id.0)
    .fetch_one(database.raw_pool())
    .await
    .expect("count initial rollback audit");
    assert_eq!(audit_count, 1);
}

#[tokio::test]
async fn revoking_a_losing_stage_keeps_the_prior_active_db() {
    // Pins: a connection-generation CAS loser compensates by revoking only its
    // inactive staged version; the previously active credential remains usable.
    let database = TestDatabase::new("cred_stage_compensation").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let series = identity(tenant_id, connection_uid);
    let prior = vault
        .create(
            series.clone(),
            SecretString::from("prior_active_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "stage-compensation-create",
                "stage-compensation-create-hash",
            ),
        )
        .await
        .expect("create prior active version");
    let staged = vault
        .stage(
            series.clone(),
            SecretString::from("losing_staged_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Stage,
                "stage-compensation-stage",
                "stage-compensation-stage-hash",
            ),
        )
        .await
        .expect("stage replacement");
    assert_eq!(staged.expected_prior_active(), Some(prior.reference));

    let still_prior = vault
        .resolve_active(
            &series,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "stage-compensation-resolve-before-revoke",
                "stage-compensation-resolve-before-revoke-hash",
            ),
        )
        .await
        .expect("staging leaves prior active");
    assert_eq!(
        still_prior.expose_for_outbound_request(),
        "prior_active_material"
    );

    vault
        .revoke(
            staged.staged_reference(),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Revoke,
                "stage-compensation-revoke",
                "stage-compensation-revoke-hash",
            ),
        )
        .await
        .expect("revoke losing staged version");
    let prior_after =
        describe_version(&vault, tenant_id, owner, connection_uid, prior.reference).await;
    let staged_after = describe_version(
        &vault,
        tenant_id,
        owner,
        connection_uid,
        staged.staged_reference(),
    )
    .await;
    assert!(prior_after.active);
    assert!(!prior_after.revoked);
    assert!(!staged_after.active);
    assert!(staged_after.revoked);

    let resolved_after = vault
        .resolve_active(
            &series,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "stage-compensation-resolve-after-revoke",
                "stage-compensation-resolve-after-revoke-hash",
            ),
        )
        .await
        .expect("revoking staged loser leaves prior active");
    assert_eq!(
        resolved_after.expose_for_outbound_request(),
        "prior_active_material"
    );
}

#[tokio::test]
async fn staged_activation_is_tenant_and_slot_exact_db() {
    // Pins: a host-local staging token cannot be activated under another tenant
    // and rotating one named slot never changes another slot on the connection.
    let database = TestDatabase::new("cred_stage_scope").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let other_tenant = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let primary = identity(tenant_id, connection_uid);
    let secondary_slot = CredentialSlotName::try_from("secondary").expect("fixture slot is valid");
    let secondary = identity_in_slot(tenant_id, connection_uid, secondary_slot);
    let primary_prior = vault
        .create(
            primary.clone(),
            SecretString::from("scope_primary_prior".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "stage-scope-create-primary",
                "stage-scope-create-primary-hash",
            ),
        )
        .await
        .expect("create primary prior");
    let secondary_prior = vault
        .create(
            secondary.clone(),
            SecretString::from("scope_secondary_prior".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "stage-scope-create-secondary",
                "stage-scope-create-secondary-hash",
            ),
        )
        .await
        .expect("create secondary prior");
    let staged = vault
        .stage(
            primary.clone(),
            SecretString::from("scope_primary_replacement".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Stage,
                "stage-scope-stage-primary",
                "stage-scope-stage-primary-hash",
            ),
        )
        .await
        .expect("stage primary replacement");
    assert_eq!(
        staged.expected_prior_active(),
        Some(primary_prior.reference)
    );

    let cross_tenant = vault
        .activate_staged(
            &staged,
            &context(
                other_tenant,
                caller(owner),
                CredentialOperation::Activate,
                "stage-scope-cross-tenant",
                "stage-scope-cross-tenant-hash",
            ),
        )
        .await
        .expect_err("another tenant cannot activate the token");
    assert_eq!(cross_tenant, CredentialError::WrongTenant);

    vault
        .activate_staged(
            &staged,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Activate,
                "stage-scope-activate-primary",
                "stage-scope-activate-primary-hash",
            ),
        )
        .await
        .expect("activate primary replacement");
    let secondary_after = describe_version(
        &vault,
        tenant_id,
        owner,
        connection_uid,
        secondary_prior.reference,
    )
    .await;
    assert!(secondary_after.active);
    assert!(!secondary_after.revoked);
    let resolved_secondary = vault
        .resolve_active(
            &secondary,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "stage-scope-resolve-secondary",
                "stage-scope-resolve-secondary-hash",
            ),
        )
        .await
        .expect("primary activation leaves secondary slot resolvable");
    assert_eq!(
        resolved_secondary.expose_for_outbound_request(),
        "scope_secondary_prior"
    );
}

#[tokio::test]
async fn forged_or_stale_predecessor_fails_without_changing_versions_db() {
    // Pins: activation validates the token's exact predecessor, so a stale or
    // forged generation handoff cannot deactivate the current credential.
    let database = TestDatabase::new("cred_stage_stale_prior").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let series = identity(tenant_id, connection_uid);
    let prior = vault
        .create(
            series.clone(),
            SecretString::from("stale_prior_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "stale-prior-create",
                "stale-prior-create-hash",
            ),
        )
        .await
        .expect("create prior");
    let staged = vault
        .stage(
            series.clone(),
            SecretString::from("stale_prior_candidate".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Stage,
                "stale-prior-stage",
                "stale-prior-stage-hash",
            ),
        )
        .await
        .expect("stage candidate");
    let forged =
        CredentialStagingToken::new(staged.staged_reference(), series, staged.version(), None);
    let error = vault
        .activate_staged(
            &forged,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Activate,
                "stale-prior-activate",
                "stale-prior-activate-hash",
            ),
        )
        .await
        .expect_err("wrong predecessor must lose the activation CAS");
    assert_eq!(error, CredentialError::VersionConflict);

    let prior_after =
        describe_version(&vault, tenant_id, owner, connection_uid, prior.reference).await;
    let staged_after = describe_version(
        &vault,
        tenant_id,
        owner,
        connection_uid,
        staged.staged_reference(),
    )
    .await;
    assert!(prior_after.active);
    assert!(!prior_after.revoked);
    assert!(!staged_after.active);
    assert!(!staged_after.revoked);
}

#[tokio::test]
async fn concurrent_stages_and_activations_leave_exactly_one_active_winner_db() {
    // Pins: two replicas may stage against one generation, but predecessor CAS
    // permits exactly one activation and leaves no second active credential.
    let database = TestDatabase::new("cred_stage_concurrent").await;
    let shared_kms = kms();
    let first_vault = Arc::new(PostgresCredentialVault::new(
        database.pool(),
        Arc::clone(&shared_kms),
    ));
    let second_vault = Arc::new(PostgresCredentialVault::new(
        database.independent_pool().await,
        Arc::clone(&shared_kms),
    ));
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let series = identity(tenant_id, connection_uid);
    let prior = first_vault
        .create(
            series.clone(),
            SecretString::from("concurrent_prior".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "concurrent-stage-create",
                "concurrent-stage-create-hash",
            ),
        )
        .await
        .expect("create concurrent prior");
    let first_stage_ctx = context(
        tenant_id,
        caller(owner),
        CredentialOperation::Stage,
        "concurrent-stage-first",
        "concurrent-stage-first-hash",
    );
    let second_stage_ctx = context(
        tenant_id,
        caller(owner),
        CredentialOperation::Stage,
        "concurrent-stage-second",
        "concurrent-stage-second-hash",
    );
    let (first_staged, second_staged) = tokio::join!(
        first_vault.stage(
            series.clone(),
            SecretString::from("concurrent_candidate_first".to_string()),
            &first_stage_ctx,
        ),
        second_vault.stage(
            series.clone(),
            SecretString::from("concurrent_candidate_second".to_string()),
            &second_stage_ctx,
        )
    );
    let first_staged = first_staged.expect("first concurrent stage");
    let second_staged = second_staged.expect("second concurrent stage");
    assert_eq!(first_staged.expected_prior_active(), Some(prior.reference));
    assert_eq!(second_staged.expected_prior_active(), Some(prior.reference));
    assert_ne!(
        first_staged.staged_reference(),
        second_staged.staged_reference()
    );
    assert_ne!(first_staged.version(), second_staged.version());

    let prior_while_staged = first_vault
        .resolve_active(
            &series,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "concurrent-stage-prior-resolve",
                "concurrent-stage-prior-resolve-hash",
            ),
        )
        .await
        .expect("prior remains active while both replacements are staged");
    assert_eq!(
        prior_while_staged.expose_for_outbound_request(),
        "concurrent_prior"
    );

    let first_activate_ctx = context(
        tenant_id,
        caller(owner),
        CredentialOperation::Activate,
        "concurrent-activate-first",
        "concurrent-activate-first-hash",
    );
    let second_activate_ctx = context(
        tenant_id,
        caller(owner),
        CredentialOperation::Activate,
        "concurrent-activate-second",
        "concurrent-activate-second-hash",
    );
    let (first_result, second_result) = tokio::join!(
        first_vault.activate_staged(&first_staged, &first_activate_ctx),
        second_vault.activate_staged(&second_staged, &second_activate_ctx),
    );
    let (winner, loser) = match (first_result, second_result) {
        (Ok(winner), Err(CredentialError::VersionConflict)) => (winner, &second_staged),
        (Err(CredentialError::VersionConflict), Ok(winner)) => (winner, &first_staged),
        _ => panic!("exactly one activation must win with one version conflict"),
    };
    assert!(winner.active);
    assert!(!winner.revoked);

    first_vault
        .revoke(
            loser.staged_reference(),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Revoke,
                "concurrent-stage-revoke-loser",
                "concurrent-stage-revoke-loser-hash",
            ),
        )
        .await
        .expect("revoke inactive activation loser");

    let rows: Vec<(Uuid, bool, bool)> = sqlx::query_as(
        r#"
        SELECT credential_uid, active, revoked
        FROM tenant_credential_versions
        WHERE tenant_id = $1
          AND connection_uid = $2
          AND kind = $3
          AND slot_name = $4
        ORDER BY version
        "#,
    )
    .bind(tenant_id.0)
    .bind(connection_uid)
    .bind(CredentialKind::ProviderApiKey.as_str())
    .bind(CredentialSlotName::PRIMARY.as_str())
    .fetch_all(database.raw_pool())
    .await
    .expect("read final concurrent series state");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows.iter().filter(|(_, active, _)| *active).count(), 1);
    assert_eq!(rows.iter().filter(|(_, _, revoked)| *revoked).count(), 1);
    assert_eq!(
        rows.iter()
            .find(|(reference, _, _)| *reference == winner.reference.as_uuid())
            .map(|(_, active, revoked)| (*active, *revoked)),
        Some((true, false))
    );
    assert_eq!(
        rows.iter()
            .find(|(reference, _, _)| *reference == loser.staged_reference().as_uuid())
            .map(|(_, active, revoked)| (*active, *revoked)),
        Some((false, true))
    );
    assert_eq!(
        rows.iter()
            .find(|(reference, _, _)| *reference == prior.reference.as_uuid())
            .map(|(_, active, revoked)| (*active, *revoked)),
        Some((false, false))
    );
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
async fn migrated_primary_credential_keeps_legacy_encryption_context_db() {
    // Pins: V50 backfills existing rows into the primary slot without resealing
    // ciphertext, so the post-migration vault must still open material sealed
    // with the exact pre-slot encryption context.
    let database = TestDatabase::new("cred_primary_compat").await;
    let shared_kms = kms();
    let vault = PostgresCredentialVault::new(database.pool(), Arc::clone(&shared_kms));
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let credential_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let legacy_context = EncryptionContext::new(
        tenant_id.0,
        connection_uid,
        CredentialKind::ProviderApiKey.as_str(),
        "tenant_credential",
    );
    let sealed = moa_crypto::encrypt(
        shared_kms.as_ref(),
        b"legacy_primary_material",
        &legacy_context,
    )
    .await
    .expect("seal pre-slot credential fixture");
    let key_id = sealed.key_handle.as_str().to_string();
    let sealed_bytes = sealed.to_bytes();

    // Omitting slot_name intentionally exercises V50's primary default, which
    // is the shape of every credential row that predates named slots.
    sqlx::query(
        r#"
        INSERT INTO tenant_credential_versions (
            credential_uid, tenant_id, connection_uid, kind, version,
            material_sealed, kms_key_id, active, revoked, owner_identity_id
        )
        VALUES ($1, $2, $3, $4, 1, $5, $6, TRUE, FALSE, $7)
        "#,
    )
    .bind(credential_uid)
    .bind(tenant_id.0)
    .bind(connection_uid)
    .bind(CredentialKind::ProviderApiKey.as_str())
    .bind(sealed_bytes)
    .bind(key_id)
    .bind(owner)
    .execute(database.raw_pool())
    .await
    .expect("insert migrated primary fixture");

    let resolved = vault
        .resolve(
            &CredentialSource::TenantConnection {
                reference: CredentialRef::from_uuid(credential_uid),
            },
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "resolve-migrated-primary",
                "resolve-migrated-primary-hash",
            ),
        )
        .await
        .expect("post-migration vault should open legacy primary ciphertext");

    assert_eq!(
        resolved.expose_for_outbound_request(),
        "legacy_primary_material"
    );
}

#[tokio::test]
async fn named_slots_rotate_independently_and_enforce_one_active_series_db() {
    // Pins: two slots of one credential kind on one connection are independent
    // series, while a second active version inside either slot is impossible.
    let database = TestDatabase::new("cred_named_slots").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let secondary_slot =
        CredentialSlotName::try_from("secondary").expect("fixture slot should be valid");

    let primary = vault
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from("primary_v1".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "slot-create-primary",
                "slot-create-primary-hash",
            ),
        )
        .await
        .expect("create primary slot");
    let secondary_v1 = vault
        .create(
            identity_in_slot(tenant_id, connection_uid, secondary_slot.clone()),
            SecretString::from("secondary_v1".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "slot-create-secondary",
                "slot-create-secondary-hash",
            ),
        )
        .await
        .expect("create secondary slot");

    let duplicate = vault
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from("forbidden_second_primary".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "slot-create-primary-duplicate",
                "slot-create-primary-duplicate-hash",
            ),
        )
        .await
        .expect_err("one slot cannot acquire a second active series");
    assert_eq!(duplicate, CredentialError::VersionConflict);

    let secondary_v2 = vault
        .rotate(
            secondary_v1.reference,
            SecretString::from("secondary_v2".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Rotate,
                "slot-rotate-secondary",
                "slot-rotate-secondary-hash",
            ),
        )
        .await
        .expect("rotate only the secondary slot");

    assert_eq!(primary.identity.slot_name, CredentialSlotName::PRIMARY);
    assert_eq!(primary.version, 1);
    assert_eq!(secondary_v1.identity.slot_name, secondary_slot);
    assert_eq!(secondary_v1.version, 1);
    assert_eq!(secondary_v2.identity.slot_name, secondary_slot);
    assert_eq!(secondary_v2.version, 2);

    let primary_material = vault
        .resolve(
            &CredentialSource::TenantConnection {
                reference: primary.reference,
            },
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "slot-resolve-primary",
                "slot-resolve-primary-hash",
            ),
        )
        .await
        .expect("secondary rotation must not stale primary");
    assert_eq!(primary_material.expose_for_outbound_request(), "primary_v1");

    let stale = vault
        .resolve(
            &CredentialSource::TenantConnection {
                reference: secondary_v1.reference,
            },
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "slot-resolve-secondary-stale",
                "slot-resolve-secondary-stale-hash",
            ),
        )
        .await
        .expect_err("rotated secondary v1 must be stale");
    assert_eq!(stale, CredentialError::StaleVersion);

    let secondary_material = vault
        .resolve(
            &CredentialSource::TenantConnection {
                reference: secondary_v2.reference,
            },
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "slot-resolve-secondary-v2",
                "slot-resolve-secondary-v2-hash",
            ),
        )
        .await
        .expect("secondary v2 should resolve");
    assert_eq!(
        secondary_material.expose_for_outbound_request(),
        "secondary_v2"
    );

    let series: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        r#"
        SELECT slot_name,
               COUNT(*),
               COUNT(*) FILTER (WHERE active),
               MAX(version)
        FROM tenant_credential_versions
        WHERE tenant_id = $1 AND connection_uid = $2 AND kind = $3
        GROUP BY slot_name
        ORDER BY slot_name
        "#,
    )
    .bind(tenant_id.0)
    .bind(connection_uid)
    .bind(CredentialKind::ProviderApiKey.as_str())
    .fetch_all(database.raw_pool())
    .await
    .expect("inspect exact named series");
    assert_eq!(
        series,
        vec![
            ("primary".to_string(), 1, 1, 1),
            ("secondary".to_string(), 2, 1, 2),
        ]
    );

    let audited_slots: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT operation_id, slot_name
        FROM tenant_credential_operations
        WHERE tenant_id = $1 AND operation_id LIKE 'slot-%'
        ORDER BY operation_id
        "#,
    )
    .bind(tenant_id.0)
    .fetch_all(database.raw_pool())
    .await
    .expect("inspect slot-aware audit rows");
    assert_eq!(
        audited_slots,
        vec![
            ("slot-create-primary".to_string(), "primary".to_string()),
            ("slot-create-secondary".to_string(), "secondary".to_string()),
            ("slot-resolve-primary".to_string(), "primary".to_string()),
            (
                "slot-resolve-secondary-v2".to_string(),
                "secondary".to_string()
            ),
            ("slot-rotate-secondary".to_string(), "secondary".to_string()),
        ]
    );
}

#[tokio::test]
async fn credential_active_resolution_selects_exact_series_and_replays_one_version_db() {
    // Pins: connector dispatch resolves by the complete series identity, not a
    // caller-persisted version reference, while an operation replay remains
    // bound to the version recorded by its original audit row.
    let database = TestDatabase::new("cred_active_exact").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let secondary_slot =
        CredentialSlotName::try_from("secondary").expect("fixture slot should be valid");
    let secondary_identity = identity_in_slot(tenant_id, connection_uid, secondary_slot.clone());

    vault
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from("primary_active_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "active-create-primary",
                "active-create-primary-hash",
            ),
        )
        .await
        .expect("create primary series");
    let secondary_v1 = vault
        .create(
            secondary_identity.clone(),
            SecretString::from("secondary_active_v1".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "active-create-secondary",
                "active-create-secondary-hash",
            ),
        )
        .await
        .expect("create secondary series");

    let resolve_ctx = context(
        tenant_id,
        caller(owner),
        CredentialOperation::Resolve,
        "active-resolve-secondary",
        "active-resolve-secondary-hash",
    );
    let resolved_v1 = vault
        .resolve_active(&secondary_identity, &resolve_ctx)
        .await
        .expect("resolve exact active secondary series");
    assert_eq!(
        resolved_v1.expose_for_outbound_request(),
        "secondary_active_v1"
    );

    let audited: (Uuid, Uuid, String, String, i64) = sqlx::query_as(
        r#"
        SELECT credential_uid, connection_uid, kind, slot_name, version
        FROM tenant_credential_operations
        WHERE tenant_id = $1 AND operation_id = $2
        "#,
    )
    .bind(tenant_id.0)
    .bind(&resolve_ctx.operation_id)
    .fetch_one(database.raw_pool())
    .await
    .expect("inspect active-resolution audit selector");
    assert_eq!(
        audited,
        (
            secondary_v1.reference.as_uuid(),
            connection_uid,
            CredentialKind::ProviderApiKey.as_str().to_string(),
            secondary_slot.as_str().to_string(),
            1,
        )
    );

    let missing_kind = CredentialIdentity {
        kind: CredentialKind::OAuth,
        ..secondary_identity.clone()
    };
    let missing_error = vault
        .resolve_active(
            &missing_kind,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "active-resolve-missing-kind",
                "active-resolve-missing-kind-hash",
            ),
        )
        .await
        .expect_err("another material kind must not fall back to the active API key");
    assert_eq!(missing_error, CredentialError::NotFound);

    let secondary_v2 = vault
        .rotate(
            secondary_v1.reference,
            SecretString::from("secondary_active_v2".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Rotate,
                "active-rotate-secondary",
                "active-rotate-secondary-hash",
            ),
        )
        .await
        .expect("rotate secondary series");
    assert_eq!(secondary_v2.version, 2);

    let resolved_v2 = vault
        .resolve_active(
            &secondary_identity,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "active-resolve-secondary-v2",
                "active-resolve-secondary-v2-hash",
            ),
        )
        .await
        .expect("active selector should follow rotation");
    assert_eq!(
        resolved_v2.expose_for_outbound_request(),
        "secondary_active_v2"
    );

    let stale_replay = vault
        .resolve_active(&secondary_identity, &resolve_ctx)
        .await
        .expect_err("replay must not silently rebind its audit row to version two");
    assert_eq!(stale_replay, CredentialError::StaleVersion);

    let replay_audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tenant_credential_operations WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id.0)
    .bind(&resolve_ctx.operation_id)
    .fetch_one(database.raw_pool())
    .await
    .expect("count replay-stable active-resolution audit rows");
    assert_eq!(replay_audit_count, 1);
}

#[tokio::test]
async fn credential_active_resolution_commits_audit_before_decrypt_db() {
    // Pins: even authenticated-decryption failure cannot erase evidence that
    // the exact active credential version was selected for outbound use.
    let database = TestDatabase::new("cred_active_audit_first").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let credential_identity = identity(tenant_id, connection_uid);
    let created = vault
        .create(
            credential_identity.clone(),
            SecretString::from("must_never_escape_the_vault".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "active-audit-create",
                "active-audit-create-hash",
            ),
        )
        .await
        .expect("create credential to corrupt");

    sqlx::query(
        "UPDATE tenant_credential_versions SET material_sealed = decode('00', 'hex') WHERE credential_uid = $1",
    )
    .bind(created.reference.as_uuid())
    .execute(database.raw_pool())
    .await
    .expect("corrupt ciphertext after creation");

    let error = vault
        .resolve_active(
            &credential_identity,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "active-audit-resolve",
                "active-audit-resolve-hash",
            ),
        )
        .await
        .expect_err("corrupt ciphertext must not decrypt");
    assert!(
        matches!(error, CredentialError::Storage(_)),
        "expected typed storage failure, got {error:?}"
    );

    let audit: (Uuid, String) = sqlx::query_as(
        r#"
        SELECT credential_uid, outcome
        FROM tenant_credential_operations
        WHERE tenant_id = $1 AND operation_id = 'active-audit-resolve'
        "#,
    )
    .bind(tenant_id.0)
    .fetch_one(database.raw_pool())
    .await
    .expect("resolution audit must commit before decryption");
    assert_eq!(
        audit,
        (created.reference.as_uuid(), "succeeded".to_string())
    );
}

#[tokio::test]
async fn named_slot_is_bound_into_authenticated_encryption_context_db() {
    // Pins: changing only a stored slot label cannot reinterpret ciphertext as
    // belonging to a different credential series, even within one tenant,
    // connection, and material kind.
    let database = TestDatabase::new("cred_slot_binding").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let created = vault
        .create(
            identity_in_slot(
                tenant_id,
                connection_uid,
                CredentialSlotName::try_from("secondary").expect("fixture slot should be valid"),
            ),
            SecretString::from("slot_bound_material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "slot-bind-create",
                "slot-bind-create-hash",
            ),
        )
        .await
        .expect("create named-slot credential");

    sqlx::query(
        "UPDATE tenant_credential_versions SET slot_name = 'tertiary' WHERE credential_uid = $1",
    )
    .bind(created.reference.as_uuid())
    .execute(database.raw_pool())
    .await
    .expect("relabel slot to model corrupted or moved row");

    let error = vault
        .resolve(
            &CredentialSource::TenantConnection {
                reference: created.reference,
            },
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "slot-bind-resolve",
                "slot-bind-resolve-hash",
            ),
        )
        .await
        .expect_err("slot-relabelled ciphertext must not open");
    assert!(
        matches!(error, CredentialError::Storage(_)),
        "expected authenticated-decryption failure, got {error:?}"
    );
}

#[tokio::test]
async fn credential_batch_describe_returns_only_exact_authorized_pairs_db() {
    // Pins: an operator connection list resolves present, superseded, revoked,
    // and missing metadata in one exact-pair batch without credential
    // enumeration or plaintext resolution.
    let database = TestDatabase::new("cred_batch_describe").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let owner = Uuid::now_v7();
    let active_connection = Uuid::now_v7();
    let rotated_connection = Uuid::now_v7();
    let revoked_connection = Uuid::now_v7();
    let missing_connection = Uuid::now_v7();

    let active = vault
        .create(
            identity(tenant_id, active_connection),
            SecretString::from("active-material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "batch-create-active",
                "batch-create-active-hash",
            ),
        )
        .await
        .expect("create active credential");
    let superseded = vault
        .create(
            identity(tenant_id, rotated_connection),
            SecretString::from("old-material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "batch-create-old",
                "batch-create-old-hash",
            ),
        )
        .await
        .expect("create credential to supersede");
    vault
        .rotate(
            superseded.reference,
            SecretString::from("new-material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Rotate,
                "batch-rotate",
                "batch-rotate-hash",
            ),
        )
        .await
        .expect("supersede old credential");
    let revoked = vault
        .create(
            identity(tenant_id, revoked_connection),
            SecretString::from("revoked-material".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "batch-create-revoked",
                "batch-create-revoked-hash",
            ),
        )
        .await
        .expect("create credential to revoke");
    vault
        .revoke(
            revoked.reference,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Revoke,
                "batch-revoke",
                "batch-revoke-hash",
            ),
        )
        .await
        .expect("revoke credential");

    let described = vault
        .describe_batch(
            &[
                (active_connection, active.reference),
                (rotated_connection, superseded.reference),
                (revoked_connection, revoked.reference),
                (missing_connection, CredentialRef::from_uuid(Uuid::now_v7())),
                (Uuid::now_v7(), active.reference),
            ],
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Resolve,
                "batch-describe",
                "batch-describe-hash",
            ),
        )
        .await
        .expect("batch describe exact credential pairs");

    assert_eq!(
        described.len(),
        3,
        "missing and mismatched pairs are omitted"
    );
    let described = described
        .into_iter()
        .collect::<std::collections::HashMap<_, _>>();
    let active = described
        .get(&active_connection)
        .expect("active exact pair should be returned");
    assert!(active.active);
    assert!(!active.revoked);
    let superseded = described
        .get(&rotated_connection)
        .expect("superseded exact pair should be returned");
    assert!(!superseded.active);
    assert!(!superseded.revoked);
    let revoked = described
        .get(&revoked_connection)
        .expect("revoked exact pair should be returned");
    assert!(revoked.revoked);
}

#[tokio::test]
async fn replayed_operation_id_returns_one_row_and_a_changed_hash_conflicts_db() {
    // Pins: replay safety. The same operation id with the same request hash
    // replays exactly one audit row and one credential version; the same id with
    // another slot or different material is a typed conflict rather than a
    // second secret or a silently misaddressed replay.
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

    let cross_slot_replay = vault
        .create(
            identity_in_slot(
                tenant_id,
                connection_uid,
                CredentialSlotName::try_from("secondary").expect("fixture slot should be valid"),
            ),
            SecretString::from("first_material".to_string()),
            &ctx,
        )
        .await
        .expect_err("an operation replay cannot silently select another slot");
    assert_eq!(
        cross_slot_replay,
        CredentialError::IdempotencyConflict,
        "slot identity must participate in audit replay validation"
    );

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
async fn connection_revoke_preserves_rows_and_audit_with_exact_replay_db() {
    // Pins: ordinary disconnect revokes every version in each named slot for
    // one tenant connection, retains all sealed rows and audit, cannot cross a
    // tenant boundary, and replays without another mutation or audit row.
    let database = TestDatabase::new("cred_connection_revoke").await;
    let vault = PostgresCredentialVault::new(database.pool(), kms());
    let tenant_id = TenantId::from(Uuid::now_v7());
    let other_tenant = TenantId::from(Uuid::now_v7());
    let connection_uid = Uuid::now_v7();
    let other_connection_uid = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let other_owner = Uuid::now_v7();

    let primary = vault
        .create(
            identity(tenant_id, connection_uid),
            SecretString::from("disconnect_primary_v1".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "connection-revoke-create-primary",
                "connection-revoke-create-primary-hash",
            ),
        )
        .await
        .expect("create primary credential");
    vault
        .rotate(
            primary.reference,
            SecretString::from("disconnect_primary_v2".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Rotate,
                "connection-revoke-rotate-primary",
                "connection-revoke-rotate-primary-hash",
            ),
        )
        .await
        .expect("rotate primary credential");
    let secondary_slot =
        CredentialSlotName::try_from("secondary").expect("secondary fixture slot should be valid");
    vault
        .create(
            identity_in_slot(tenant_id, connection_uid, secondary_slot),
            SecretString::from("disconnect_secondary_v1".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "connection-revoke-create-secondary",
                "connection-revoke-create-secondary-hash",
            ),
        )
        .await
        .expect("create secondary credential");
    vault
        .create(
            identity(other_tenant, connection_uid),
            SecretString::from("other_tenant_same_connection".to_string()),
            &context(
                other_tenant,
                caller(other_owner),
                CredentialOperation::Create,
                "connection-revoke-create-other-tenant",
                "connection-revoke-create-other-tenant-hash",
            ),
        )
        .await
        .expect("create neighboring tenant credential");
    vault
        .create(
            identity(tenant_id, other_connection_uid),
            SecretString::from("same_tenant_other_connection".to_string()),
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Create,
                "connection-revoke-create-other-connection",
                "connection-revoke-create-other-connection-hash",
            ),
        )
        .await
        .expect("create another connection credential in the target tenant");

    let wrong_operation = vault
        .revoke_connection(
            connection_uid,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Delete,
                "connection-revoke-wrong-operation",
                "connection-revoke-wrong-operation-hash",
            ),
        )
        .await
        .expect_err("ordinary disconnect must require an exact revoke context");
    assert_eq!(wrong_operation, CredentialError::Unauthorized);

    let revoke_ctx = context(
        tenant_id,
        caller(owner),
        CredentialOperation::Revoke,
        "connection-revoke-all",
        "connection-revoke-all-hash",
    );
    let revoked = vault
        .revoke_connection(connection_uid, &revoke_ctx)
        .await
        .expect("revoke every target connection version");
    assert_eq!(revoked, 3);
    let replayed = vault
        .revoke_connection(connection_uid, &revoke_ctx)
        .await
        .expect("exact connection revoke replay should be a no-op");
    assert_eq!(replayed, 0);
    let conflict = vault
        .revoke_connection(
            connection_uid,
            &context(
                tenant_id,
                caller(owner),
                CredentialOperation::Revoke,
                "connection-revoke-all",
                "changed-connection-revoke-hash",
            ),
        )
        .await
        .expect_err("reused operation id with changed metadata must conflict");
    assert_eq!(conflict, CredentialError::IdempotencyConflict);

    let target_rows: Vec<(bool, bool)> = sqlx::query_as(
        "SELECT active, revoked FROM tenant_credential_versions \
         WHERE tenant_id = $1 AND connection_uid = $2 ORDER BY version, slot_name",
    )
    .bind(tenant_id.0)
    .bind(connection_uid)
    .fetch_all(database.raw_pool())
    .await
    .expect("read retained revoked connection versions");
    assert_eq!(target_rows, vec![(false, true); 3]);
    let other_row: (bool, bool) = sqlx::query_as(
        "SELECT active, revoked FROM tenant_credential_versions \
         WHERE tenant_id = $1 AND connection_uid = $2",
    )
    .bind(other_tenant.0)
    .bind(connection_uid)
    .fetch_one(database.raw_pool())
    .await
    .expect("read neighboring tenant version");
    assert_eq!(other_row, (true, false));
    let other_connection_row: (bool, bool) = sqlx::query_as(
        "SELECT active, revoked FROM tenant_credential_versions \
         WHERE tenant_id = $1 AND connection_uid = $2",
    )
    .bind(tenant_id.0)
    .bind(other_connection_uid)
    .fetch_one(database.raw_pool())
    .await
    .expect("read same-tenant neighboring connection version");
    assert_eq!(other_connection_row, (true, false));

    let audit: (Option<Uuid>, Option<String>, Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT credential_uid, kind, slot_name, version \
         FROM tenant_credential_operations \
         WHERE tenant_id = $1 AND operation_id = 'connection-revoke-all' \
           AND operation = 'revoke' AND connection_uid = $2",
    )
    .bind(tenant_id.0)
    .bind(connection_uid)
    .fetch_one(database.raw_pool())
    .await
    .expect("read connection-wide revoke audit");
    assert_eq!(audit, (None, None, None, None));
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM tenant_credential_operations \
         WHERE tenant_id = $1 AND operation_id = 'connection-revoke-all'",
    )
    .bind(tenant_id.0)
    .fetch_one(database.raw_pool())
    .await
    .expect("count connection-wide revoke audit rows");
    assert_eq!(audit_count, 1);
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

    sqlx::query("UPDATE tenant_credential_versions SET kind = 'oauth' WHERE credential_uid = $1")
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
