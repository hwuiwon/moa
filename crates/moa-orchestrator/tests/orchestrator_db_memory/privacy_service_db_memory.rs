//! Privacy service helper coverage.

use std::{collections::BTreeMap, io::Read, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier};
use moa_artifacts::{
    document::ArtifactDocument,
    registry::{ArtifactRegistry, NewArtifactDraft, NewArtifactFile, StoredArtifactRevision},
};
use moa_config::ComplianceConfig;
use moa_core::types::action_policy::ActionRuleScope;
use moa_core::types::agent::SYSTEM_DEFAULT_AGENT_REVISION_UID;
use moa_core::types::identifiers::UserId;
use moa_core::types::memory::RlsContext;
use moa_core::types::security::SensitivityClass;
use moa_core::{types::contact::ContactId, types::identifiers::TenantId};
use moa_crypto::{EncryptionContext, KeyManagementProvider, LocalKmsProvider};
use moa_lineage_audit::PiiVault;
use moa_memory_graph::{GraphStore, NodeLabel, NodeWriteIntent, PostgresGraphStore};
use moa_memory_pii::erasure::begin_app_scoped_tx;
use moa_memory_pii::learning_erasure::{
    ErasureDisposition, ErasureRecordKind, RecordDecision, record_decisions,
};
use moa_memory_vector::PgvectorStore;
use moa_orchestrator::services::dual_control::{self, DualControlError};
use moa_orchestrator::services::privacy::repository::{
    begin_privacy_export_snapshot, collect_privacy_export_data_sections,
};
use moa_orchestrator::services::privacy::{
    ApprovalClaims, ApprovalTokenVerifier, DUAL_CONTROL_OPERATION_ERASE, Ed25519ManifestSigner,
    PrivacyEraseContext, PrivacyExportContext, PrivacySubject, PrivacySubjectProvenance,
    ensure_jti_inserted, erase_operation_ref, execute_privacy_export, finalize_archive_to_bytes,
    run_privacy_erase, write_export_readme, write_manifest,
};
use moa_session::testing;
use moa_wire::privacy::{ContactErasureScope, PrivacyEraseStatus, PrivacyExportRequest};
use serde_json::json;
use sqlx::PgPool;
use tempfile::tempdir;
use uuid::Uuid;

fn test_kms() -> Arc<dyn KeyManagementProvider> {
    static KMS: std::sync::OnceLock<Arc<dyn KeyManagementProvider>> = std::sync::OnceLock::new();
    KMS.get_or_init(|| Arc::new(LocalKmsProvider::new()))
        .clone()
}

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7_u8; 32])
}

fn approval_token(claims: &ApprovalClaims, key: &SigningKey) -> String {
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"EdDSA","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("serialize claims"));
    let signed = format!("{header}.{payload}");
    let signature = key.sign(signed.as_bytes()).to_bytes();
    format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature))
}

fn valid_claims(subject: Uuid, tenant_id: Uuid) -> ApprovalClaims {
    valid_claims_for(subject, tenant_id, "export")
}

fn valid_claims_for(subject: Uuid, tenant_id: Uuid, op: &str) -> ApprovalClaims {
    valid_claims_for_user_id(&subject.to_string(), tenant_id, op)
}

fn valid_claims_for_user_id(subject_user_id: &str, tenant_id: Uuid, op: &str) -> ApprovalClaims {
    ApprovalClaims {
        sub: "ops-admin".to_string(),
        jti: Uuid::now_v7().to_string(),
        exp: moa_test_support::fixtures::pg_now().timestamp() + 300,
        op: op.to_string(),
        subject_user_id: subject_user_id.to_string(),
        tenant_id: TenantId::from(tenant_id),
        role: None,
        roles: vec!["platform_admin".to_string()],
    }
}

fn export_compliance_config() -> ComplianceConfig {
    ComplianceConfig {
        privacy_export_signing_key_hex: Some(hex::encode(signing_key().to_bytes())),
        privacy_export_signing_key_id: "privacy-export-test-key".to_string(),
        ..ComplianceConfig::default()
    }
}

fn export_request(
    tenant_id: Uuid,
    subject_user_id: &str,
    pgp_recipient: Option<String>,
) -> PrivacyExportRequest {
    PrivacyExportRequest {
        tenant_id: TenantId::from(tenant_id),
        subject_user_id: UserId::new(subject_user_id),
        reason: "GDPR Article 15 test request".to_string(),
        approval_token: "verified-before-executor".to_string(),
        pgp_recipient,
    }
}

fn basis_vector() -> Vec<f32> {
    let mut vector = vec![0.0; 1024];
    vector[0] = 1.0;
    vector
}

fn pii_vault_secret() -> Vec<u8> {
    b"privacy-erase-test-secret".to_vec()
}

fn contact_user_id(contact_id: Uuid) -> String {
    format!("contact:{contact_id}")
}

fn tenant_workspace() -> (Uuid, String) {
    let tenant_id = Uuid::now_v7();
    (tenant_id, tenant_id.to_string())
}

fn erase_test_graph(pool: &PgPool, tenant_id: Uuid, contact_id: Uuid) -> PostgresGraphStore {
    let scope = RlsContext::contact(TenantId::from(tenant_id), ContactId(contact_id));
    let vector = PgvectorStore::new_for_app_role(pool.clone(), scope.clone());
    PostgresGraphStore::scoped_for_app_role(pool.clone(), scope, test_kms())
        .with_vector_store(Arc::new(vector))
}

fn erase_test_intent(storage_partition_id: &str, user_id: &str, name: &str) -> NodeWriteIntent {
    let storage_partition_id = Some(storage_partition_id.to_string());
    NodeWriteIntent {
        barrier: None,
        uid: Uuid::now_v7(),
        label: NodeLabel::Fact,
        storage_partition_id,
        contact_id: Some(user_id.to_string()),
        data_subject_id: Uuid::parse_str(user_id.strip_prefix("contact:").unwrap_or(user_id))
            .expect("contact fixture UUID"),
        scope: "contact".to_string(),
        name: name.to_string(),
        properties: json!({ "name": name, "user_id": user_id, "source": "privacy_erase_test" }),
        pii_class: SensitivityClass::Phi,
        confidence: Some(0.95),
        valid_from: moa_test_support::fixtures::pg_now(),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        embedding_text: None,
        actor_id: user_id.to_string(),
        actor_kind: "contact".to_string(),
    }
}

async fn create_erase_test_node(
    pool: &PgPool,
    tenant_id: Uuid,
    contact_id: Uuid,
    storage_partition_id: &str,
    user_id: &str,
    name: &str,
) -> Uuid {
    seed_workspace_embedder_state(pool, tenant_id, storage_partition_id, user_id, "test-model")
        .await;
    let graph = erase_test_graph(pool, tenant_id, contact_id);
    let intent = erase_test_intent(storage_partition_id, user_id, name);
    let uid = intent.uid;
    graph
        .create_node(intent)
        .await
        .expect("create erase fixture");
    uid
}

/// Seeds an erase fixture whose node RETAINS its vector embedding.
///
/// `erase_test_intent` classifies nodes `Phi`, and the write path rejects any
/// vector embedding for `restricted`/`phi` content. Tests that assert embedding
/// erasure therefore use an unsealed attributable node with a real embedding.
async fn create_embedded_erase_test_node(
    pool: &PgPool,
    tenant_id: Uuid,
    contact_id: Uuid,
    storage_partition_id: &str,
    user_id: &str,
    name: &str,
) -> Uuid {
    seed_workspace_embedder_state(pool, tenant_id, storage_partition_id, user_id, "test-model")
        .await;
    let graph = erase_test_graph(pool, tenant_id, contact_id);
    let mut intent = erase_test_intent(storage_partition_id, user_id, name);
    intent.pii_class = SensitivityClass::None;
    intent.embedding = Some(basis_vector());
    intent.embedding_model = Some("test-model".to_string());
    intent.embedding_model_version = Some(1);
    let uid = intent.uid;
    graph
        .create_node(intent)
        .await
        .expect("create embedded erase fixture");
    uid
}

async fn seed_workspace_embedder_state(
    pool: &PgPool,
    tenant_id: Uuid,
    storage_partition_id: &str,
    subject_user_id: &str,
    model: &str,
) {
    let mut conn = begin_app_scoped_tx(pool, TenantId::from(tenant_id), subject_user_id)
        .await
        .expect("begin workspace embedder seed transaction");
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, $2, 1, 1024)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension
        "#,
    )
    .bind(storage_partition_id)
    .bind(model)
    .execute(conn.as_mut())
    .await
    .expect("seed workspace embedder state");
    conn.commit().await.expect("commit workspace embedder seed");
}

async fn seed_contact(
    pool: &PgPool,
    tenant_id: Uuid,
    storage_partition_id: &str,
    contact_id: Uuid,
    state: &str,
) {
    sqlx::query(
        r#"
        INSERT INTO contacts (id, tenant_id, storage_partition_id, contact_id, state)
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(contact_id)
    .bind(tenant_id)
    .bind(storage_partition_id)
    .bind(contact_id)
    .bind(state)
    .execute(pool)
    .await
    .expect("seed contact");
}

async fn seed_merged_contact(
    pool: &PgPool,
    tenant_id: Uuid,
    storage_partition_id: &str,
    merged_contact_id: Uuid,
    canonical_contact_id: Uuid,
) {
    sqlx::query(
        r#"
        INSERT INTO contacts (
            id, tenant_id, storage_partition_id, contact_id, state, canonical_contact_id, merged_at
        )
        VALUES ($1, $2, $3, $4, 'merged', $5, NOW())
        "#,
    )
    .bind(merged_contact_id)
    .bind(tenant_id)
    .bind(storage_partition_id)
    .bind(merged_contact_id)
    .bind(canonical_contact_id)
    .execute(pool)
    .await
    .expect("seed merged contact");
}

async fn seed_merged_contacts(
    pool: &PgPool,
    tenant_id: Uuid,
    storage_partition_id: &str,
    canonical_contact_id: Uuid,
    merged_contact_ids: &[Uuid],
) {
    sqlx::query(
        r#"
        INSERT INTO contacts (
            id, tenant_id, storage_partition_id, contact_id, state,
            canonical_contact_id, merged_at
        )
        SELECT linked_id, $1, $2, linked_id, 'merged', $3, now()
        FROM unnest($4::uuid[]) AS linked(linked_id)
        "#,
    )
    .bind(tenant_id)
    .bind(storage_partition_id)
    .bind(canonical_contact_id)
    .bind(merged_contact_ids)
    .execute(pool)
    .await
    .expect("seed merged contact set");
}

async fn node_count(pool: &PgPool, uid: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.node_index WHERE uid = $1")
        .bind(uid)
        .fetch_one(pool)
        .await
        .expect("count node rows")
}

async fn embedding_count(pool: &PgPool, uid: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.embeddings WHERE uid = $1")
        .bind(uid)
        .fetch_one(pool)
        .await
        .expect("count embedding rows")
}

async fn erase_changelog_count(pool: &PgPool, storage_partition_id: &str, subject: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog \
         WHERE storage_partition_id = $1 AND op = 'erase' AND target_uid = $2",
    )
    .bind(storage_partition_id)
    .bind(subject)
    .fetch_one(pool)
    .await
    .expect("count erase changelog rows")
}

async fn seed_pii_vault_subject(pool: &PgPool, storage_partition_id: &str, subject_user_id: &str) {
    let vault = PiiVault::new_dev(pii_vault_secret());
    let subject_pseudonym = vault
        .subject_pseudonym(subject_user_id)
        .expect("subject pseudonym should compute");
    sqlx::query(
        r#"
        INSERT INTO pii_vault.subject_keys (
            subject_pseudonym, storage_partition_id, hmac_key_handle
        )
        VALUES ($1, $2, 'test-key')
        "#,
    )
    .bind(subject_pseudonym)
    .bind(storage_partition_id)
    .execute(pool)
    .await
    .expect("seed PII vault subject");
}

async fn erased_pii_vault_subject_count(
    pool: &PgPool,
    storage_partition_id: &str,
    subject_user_id: &str,
) -> i64 {
    let vault = PiiVault::new_dev(pii_vault_secret());
    let subject_pseudonym = vault
        .subject_pseudonym(subject_user_id)
        .expect("subject pseudonym should compute");
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM pii_vault.subject_keys
        WHERE storage_partition_id = $1
          AND subject_pseudonym = $2
          AND erased_at IS NOT NULL
        "#,
    )
    .bind(storage_partition_id)
    .bind(subject_pseudonym)
    .fetch_one(pool)
    .await
    .expect("count erased PII vault subject")
}

async fn total_erase_changelog_count(pool: &PgPool, storage_partition_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog WHERE storage_partition_id = $1 AND op = 'erase'",
    )
    .bind(storage_partition_id)
    .fetch_one(pool)
    .await
    .expect("count all erase changelog rows")
}

#[test]
fn approval_token_verifies_subject_op_tenant_and_signature() {
    // Pins: server-side approval verifier binds token proof to op, subject, tenant, and signature.
    let subject = Uuid::now_v7();
    let tenant_id = Uuid::now_v7();
    let key = signing_key();
    let verifier = ApprovalTokenVerifier {
        verifying_key: key.verifying_key(),
    };
    let claims = valid_claims(subject, tenant_id);
    let token = approval_token(&claims, &key);

    let verified = verifier
        .verify(
            &token,
            "export",
            &subject.to_string(),
            TenantId::from(tenant_id),
        )
        .expect("verify token");

    assert_eq!(verified.sub, "ops-admin");
    assert_eq!(verified.subject_user_id, subject.to_string());
    assert_eq!(verified.tenant_id, TenantId::from(tenant_id));
}

#[test]
fn approval_token_requires_platform_admin_role() {
    // Pins: signed privacy approval tokens must carry platform_admin.
    let subject = Uuid::now_v7();
    let tenant_id = Uuid::now_v7();
    let key = signing_key();
    let verifier = ApprovalTokenVerifier {
        verifying_key: key.verifying_key(),
    };
    let mut claims = valid_claims(subject, tenant_id);
    claims.roles.clear();
    let token = approval_token(&claims, &key);

    let error = verifier
        .verify(
            &token,
            "export",
            &subject.to_string(),
            TenantId::from(tenant_id),
        )
        .expect_err("missing platform_admin role should fail");

    assert!(format!("{error:?}").contains("platform_admin"));
}

#[test]
fn approval_jti_replay_blocked() {
    // Pins: server-side JTI replay helper rejects duplicate approval-token consumption.
    ensure_jti_inserted(Some("jti-1")).expect("first insert accepted");
    let error = ensure_jti_inserted(None).expect_err("replay should fail");
    assert!(format!("{error:?}").contains("replayed"));
}

#[tokio::test]
async fn privacy_erase_dry_run() {
    // Pins: dry-run erase enumerates candidates without hard-purging graph rows.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    let uid = create_erase_test_node(
        store.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject.to_string(),
        "dry run fact",
    )
    .await;
    let before_changelog = total_erase_changelog_count(store.pool(), &storage_partition_id).await;
    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "dry run".to_string(),
        dry_run: true,
        contact_erasure_scope: None,
        claims: valid_claims_for(subject, tenant_id, "erase"),
        pii_vault_secret: None,
        require_dual_control: false,
    };

    let response = run_privacy_erase(ctx).await.expect("run dry erase");

    assert!(response.dry_run);
    assert_eq!(response.candidate_count, 1);
    assert_eq!(response.erased_count, 0);
    assert_eq!(response.sample[0]["uid"], uid.to_string());
    assert_eq!(node_count(store.pool(), uid).await, 1);
    assert_eq!(
        total_erase_changelog_count(store.pool(), &storage_partition_id).await,
        before_changelog
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_basic() {
    // Pins: privacy erase hard-purges graph data and marks the PII vault subject key erased.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    let uid = create_embedded_erase_test_node(
        store.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject.to_string(),
        "basic erasure fact",
    )
    .await;
    seed_pii_vault_subject(store.pool(), &storage_partition_id, &subject.to_string()).await;
    assert_eq!(embedding_count(store.pool(), uid).await, 1);
    let kms = test_kms();
    let encryption_context =
        EncryptionContext::new(tenant_id, subject, "privacy-shred-proof", "restricted");
    let ciphertext =
        moa_crypto::encrypt(kms.as_ref(), b"must become unreadable", &encryption_context)
            .await
            .expect("seal privacy-shred proof");
    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: kms.clone(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "GDPR Art.17 request".to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims: valid_claims_for(subject, tenant_id, "erase"),
        pii_vault_secret: Some(pii_vault_secret()),
        require_dual_control: false,
    };

    let response = run_privacy_erase(ctx).await.expect("run erasure");

    assert_eq!(response.erased_count, 1);
    assert_eq!(response.pii_vault_erased, 1);
    assert_eq!(node_count(store.pool(), uid).await, 0);
    assert_eq!(embedding_count(store.pool(), uid).await, 0);
    assert!(matches!(
        moa_crypto::decrypt(kms.as_ref(), &ciphertext, &encryption_context).await,
        Err(moa_crypto::Error::CryptoShredded(_))
    ));
    assert_eq!(
        erased_pii_vault_subject_count(store.pool(), &storage_partition_id, &subject.to_string())
            .await,
        1
    );
    assert_eq!(
        total_erase_changelog_count(store.pool(), &storage_partition_id).await,
        1,
        "erasure retains one redacted subject-level audit record"
    );
    assert_eq!(
        erase_changelog_count(store.pool(), &storage_partition_id, subject).await,
        1
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_blocked_by_subject_legal_hold_db_memory() {
    // Pins: an active legal hold on a subject blocks erasure entirely (fail
    // closed) — no graph node is purged and no erase changelog row is written —
    // and releasing the hold lets the same subject be erased. Mutation check:
    // forcing active_hold_for to return false makes this assert Completed with a
    // purged node instead of BlockedByLegalHold, so the test fails.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    let uid = create_erase_test_node(
        store.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject.to_string(),
        "held erasure fact",
    )
    .await;

    let hold = moa_memory_pii::legal_hold::place_hold(
        store.pool(),
        TenantId::from(tenant_id),
        Some(subject),
        "Acme v. MOA litigation hold",
        "ops-admin",
    )
    .await
    .expect("place legal hold");

    let held_ctx = || PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "GDPR Art.17 request".to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims: valid_claims_for(subject, tenant_id, "erase"),
        pii_vault_secret: None,
        require_dual_control: false,
    };

    let blocked = run_privacy_erase(held_ctx()).await.expect("run held erase");
    assert!(matches!(
        blocked.status,
        PrivacyEraseStatus::BlockedByLegalHold
    ));
    assert_eq!(blocked.erased_count, 0);
    assert_eq!(blocked.candidate_count, 0);
    // The subject's node survives and nothing was purged under the hold.
    assert_eq!(node_count(store.pool(), uid).await, 1);
    assert_eq!(
        total_erase_changelog_count(store.pool(), &storage_partition_id).await,
        0
    );

    // Releasing the hold lets the same subject be erased.
    let released = moa_memory_pii::legal_hold::release_hold(
        store.pool(),
        TenantId::from(tenant_id),
        hold.id,
        "ops-admin",
    )
    .await
    .expect("release legal hold");
    assert!(released, "active hold must release");

    let response = run_privacy_erase(held_ctx())
        .await
        .expect("run erase after release");
    assert!(matches!(response.status, PrivacyEraseStatus::Completed));
    assert_eq!(response.erased_count, 1);
    assert_eq!(node_count(store.pool(), uid).await, 0);

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_blocked_by_tenant_wide_legal_hold_db_memory() {
    // Pins: a tenant-wide legal hold (subject_id NULL) blocks erasure of any
    // subject in the tenant, not just one named subject.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    let uid = create_erase_test_node(
        store.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject.to_string(),
        "tenant-wide held fact",
    )
    .await;

    moa_memory_pii::legal_hold::place_hold(
        store.pool(),
        TenantId::from(tenant_id),
        None,
        "tenant-wide preservation order",
        "ops-admin",
    )
    .await
    .expect("place tenant-wide hold");

    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "GDPR Art.17 request".to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims: valid_claims_for(subject, tenant_id, "erase"),
        pii_vault_secret: None,
        require_dual_control: false,
    };

    let blocked = run_privacy_erase(ctx).await.expect("run held erase");
    assert!(matches!(
        blocked.status,
        PrivacyEraseStatus::BlockedByLegalHold
    ));
    assert_eq!(node_count(store.pool(), uid).await, 1);

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_idempotent() {
    // Pins: a second erase for an already-purged subject does not create extra graph erasure rows.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    create_erase_test_node(
        store.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject.to_string(),
        "idempotent erasure fact",
    )
    .await;
    let first = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "first erase".to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims: valid_claims_for(subject, tenant_id, "erase"),
        pii_vault_secret: None,
        require_dual_control: false,
    };
    run_privacy_erase(first).await.expect("first erasure");
    let after_first = total_erase_changelog_count(store.pool(), &storage_partition_id).await;
    let second = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "second erase".to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims: valid_claims_for(subject, tenant_id, "erase"),
        pii_vault_secret: None,
        require_dual_control: false,
    };

    let response = run_privacy_erase(second).await.expect("second erasure");

    assert_eq!(response.candidate_count, 0);
    assert_eq!(response.erased_count, 0);
    assert_eq!(
        total_erase_changelog_count(store.pool(), &storage_partition_id).await,
        after_first
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_same_request_replay_resumes_db_memory() {
    // Pins: replaying the SAME erase request (identical approval JTI and request
    // parameters) resumes the durable job idempotently and returns the persisted
    // result, rather than failing as a spent-token replay and stranding the
    // erasure. This is the Restate re-execution path.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    let uid = create_erase_test_node(
        store.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject.to_string(),
        "resumable erasure fact",
    )
    .await;
    // One signed approval token (one JTI) reused for the identical request.
    let claims = valid_claims_for(subject, tenant_id, "erase");
    let make_ctx = || PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "gdpr erasure request".to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims: claims.clone(),
        pii_vault_secret: None,
        require_dual_control: false,
    };

    let first = run_privacy_erase(make_ctx()).await.expect("first erase");
    assert_eq!(first.candidate_count, 1);
    assert_eq!(first.erased_count, 1);
    assert_eq!(node_count(store.pool(), uid).await, 0);

    // Simulate a crash after the durable erasure job completed but before its
    // separate destruction-fence completion commit was journaled.
    sqlx::query(
        "UPDATE moa.destruction_operation_fence SET status = 'in_progress', committed_at = NULL WHERE tenant_id = $1 AND subject_id = $2",
    )
    .bind(tenant_id)
    .bind(subject)
    .execute(store.pool())
    .await
    .expect("reopen destruction fence to simulate commit gap");

    // Identical request replays the same JTI: resume, do not reject.
    let replay = run_privacy_erase(make_ctx())
        .await
        .expect("identical replay resumes without error");
    assert!(matches!(replay.status, PrivacyEraseStatus::Completed));
    assert_eq!(replay.candidate_count, 1);
    assert_eq!(replay.erased_count, 1);
    let fence_status: String = sqlx::query_scalar(
        "SELECT status FROM moa.destruction_operation_fence WHERE tenant_id = $1 AND subject_id = $2",
    )
    .bind(tenant_id)
    .bind(subject)
    .fetch_one(store.pool())
    .await
    .expect("load resumed destruction fence");
    assert_eq!(fence_status, "committed");

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_deletes_digest_and_lineage_db_memory() {
    // Pins: the erase operation closes the digest and retrieval-lineage stores,
    // which graph-node purges never touch, so erased memory cannot survive in a
    // standing digest or as attributable retrieval provenance.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    create_erase_test_node(
        store.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject.to_string(),
        "digest-and-lineage erasure fact",
    )
    .await;
    seed_digest_and_lineage(store.pool(), tenant_id, subject, &storage_partition_id).await;

    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "gdpr erasure request".to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims: valid_claims_for(subject, tenant_id, "erase"),
        pii_vault_secret: None,
        require_dual_control: false,
    };

    let response = run_privacy_erase(ctx).await.expect("run erasure");
    assert_eq!(response.digest_deleted, 1);
    assert_eq!(response.lineage_deleted, 1);
    assert_eq!(
        digest_row_count(store.pool(), &storage_partition_id).await,
        0
    );
    assert_eq!(
        lineage_row_count(store.pool(), &storage_partition_id).await,
        0
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_full_same_connection_closure_finishes_with_exact_residue_db_memory() {
    // Pins: the complete subject-erasure closure runs under short timeouts
    // while every protected mutation reuses its exclusive guard connection.
    // Learning, vault, graph/vector, digest, and retrieval-lineage residue all
    // disappear, and the applied learning disposition remains as exact audit.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    let subject_user_id = subject.to_string();
    let graph_uid = create_embedded_erase_test_node(
        store.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject_user_id,
        "complete same-connection erasure fact",
    )
    .await;
    seed_pii_vault_subject(store.pool(), &storage_partition_id, &subject_user_id).await;
    seed_digest_and_lineage(store.pool(), tenant_id, subject, &storage_partition_id).await;
    let experience_id = seed_learning_experience(
        store.pool(),
        tenant_id,
        &storage_partition_id,
        &subject_user_id,
    )
    .await;
    let claims = valid_claims_for(subject, tenant_id, "erase");
    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject_user_id.clone(),
        reason: "complete GDPR Art.17 request".to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims,
        pii_vault_secret: Some(pii_vault_secret()),
        require_dual_control: false,
    };

    let response = tokio::time::timeout(Duration::from_secs(10), run_privacy_erase(ctx))
        .await
        .expect("full privacy erase must not wait on a second database PID")
        .expect("run full same-connection erasure");

    assert!(matches!(response.status, PrivacyEraseStatus::Completed));
    assert_eq!(response.candidate_count, 1);
    assert_eq!(response.erased_count, 1);
    assert_eq!(response.pii_vault_erased, 1);
    assert_eq!(response.digest_deleted, 1);
    assert_eq!(response.lineage_deleted, 1);
    let residue: (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT count(*) FROM experience_records WHERE id = $1), \
            (SELECT count(*) FROM moa.privacy_erasure_record_decision \
             WHERE tenant_id = $2 AND subject_user_id = $3 \
               AND record_kind = 'experience_record' AND record_id = $1::text \
               AND disposition = 'erased' AND applied), \
            (SELECT count(*) FROM moa.node_index WHERE uid = $4), \
            (SELECT count(*) FROM moa.embeddings WHERE uid = $4), \
            (SELECT count(*) FROM pii_vault.subject_keys \
             WHERE storage_partition_id = $5 AND erased_at IS NOT NULL)",
    )
    .bind(experience_id)
    .bind(tenant_id)
    .bind(&subject_user_id)
    .bind(graph_uid)
    .bind(&storage_partition_id)
    .fetch_one(store.pool())
    .await
    .expect("read exact full-erasure residue");
    assert_eq!(residue, (0, 1, 0, 0, 1));
    assert_eq!(
        digest_row_count(store.pool(), &storage_partition_id).await,
        0
    );
    assert_eq!(
        lineage_row_count(store.pool(), &storage_partition_id).await,
        0
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_vault_sql_failure_rolls_back_stage_and_retries_same_progress_db_memory() {
    // Pins: an SQL error after the vault UPDATE still rolls that whole guard
    // transaction back, leaves durable progress at `vault`, and permits the
    // identical job replay to retry that stage without partial residue.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    let subject_user_id = subject.to_string();
    let graph_uid = create_erase_test_node(
        store.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject_user_id,
        "vault rollback erasure fact",
    )
    .await;
    seed_pii_vault_subject(store.pool(), &storage_partition_id, &subject_user_id).await;
    let claims = valid_claims_for(subject, tenant_id, "erase");
    let make_ctx = || PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject_user_id.clone(),
        reason: "vault rollback GDPR request".to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims: claims.clone(),
        pii_vault_secret: Some(pii_vault_secret()),
        require_dual_control: false,
    };
    let suffix = subject.simple().to_string();
    let function_name = format!("fail_privacy_vault_{suffix}");
    let trigger_name = format!("fail_privacy_vault_update_{suffix}");
    let mut injector = store
        .pool()
        .acquire()
        .await
        .expect("acquire failure-injection connection");
    sqlx::query(&format!(
        "CREATE FUNCTION pg_temp.{function_name}() RETURNS trigger \
         LANGUAGE plpgsql AS $$ \
         BEGIN \
           IF NEW.storage_partition_id = '{storage_partition_id}' THEN \
             RAISE EXCEPTION 'injected vault stage failure'; \
           END IF; \
           RETURN NEW; \
         END $$"
    ))
    .execute(injector.as_mut())
    .await
    .expect("create scoped vault failure function");
    sqlx::query(&format!(
        "CREATE TRIGGER {trigger_name} AFTER UPDATE ON pii_vault.subject_keys \
         FOR EACH ROW EXECUTE FUNCTION pg_temp.{function_name}()"
    ))
    .execute(injector.as_mut())
    .await
    .expect("install scoped vault failure trigger");

    let error = tokio::time::timeout(Duration::from_secs(10), run_privacy_erase(make_ctx()))
        .await
        .expect("failing vault stage must return rather than deadlock")
        .expect_err("injected vault SQL error must fail the stage");
    let rendered_error = format!("{error:?}");
    assert!(
        rendered_error.contains("injected vault stage failure"),
        "unexpected injected-stage error: {rendered_error}"
    );
    let failed_state: (String, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT stage, pii_vault_erased, graph_erased, \
            (SELECT count(*) FROM pii_vault.subject_keys \
             WHERE storage_partition_id = $2 AND erased_at IS NOT NULL), \
            (SELECT count(*) FROM moa.node_index WHERE uid = $3) \
         FROM moa.erasure_jobs WHERE jti = $1",
    )
    .bind(&claims.jti)
    .bind(&storage_partition_id)
    .bind(graph_uid)
    .fetch_one(store.pool())
    .await
    .expect("read failed-stage durable progress and residue");
    assert_eq!(failed_state, ("vault".to_string(), 0, 0, 0, 1));

    sqlx::query(&format!(
        "DROP TRIGGER {trigger_name} ON pii_vault.subject_keys"
    ))
    .execute(injector.as_mut())
    .await
    .expect("remove scoped vault failure trigger");
    let response = tokio::time::timeout(Duration::from_secs(10), run_privacy_erase(make_ctx()))
        .await
        .expect("vault-stage replay must not deadlock")
        .expect("vault-stage replay succeeds after transient SQL failure");
    assert!(matches!(response.status, PrivacyEraseStatus::Completed));
    assert_eq!(response.pii_vault_erased, 1);
    assert_eq!(response.erased_count, 1);
    assert_eq!(node_count(store.pool(), graph_uid).await, 0);

    drop(injector);
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

async fn seed_learning_experience(
    pool: &PgPool,
    tenant_id: Uuid,
    storage_partition_id: &str,
    subject_user_id: &str,
) -> Uuid {
    let session_id = Uuid::now_v7();
    let segment_id = Uuid::now_v7();
    let experience_id = Uuid::now_v7();
    let mut tx = pool.begin().await.expect("begin learning-erasure fixture");
    sqlx::query(
        "INSERT INTO sessions \
            (id, storage_partition_id, user_id, tenant_id, model, status) \
         VALUES ($1, $2, $3, $4, 'test-model', 'completed')",
    )
    .bind(session_id)
    .bind(storage_partition_id)
    .bind(subject_user_id)
    .bind(tenant_id)
    .execute(tx.as_mut())
    .await
    .expect("seed learning-erasure session");
    sqlx::query(
        "INSERT INTO session_agent_context \
            (session_id, storage_partition_id, user_id, tenant_id, \
             agent_definition_ref, agent_revision_uid, policy_hash, display_name, policy_snapshot) \
         VALUES ($1, $2, $3, $4, 'agent://privacy-fixture', $5, \
                 'privacy-fixture-hash', 'Privacy fixture', '{}'::JSONB)",
    )
    .bind(session_id)
    .bind(storage_partition_id)
    .bind(subject_user_id)
    .bind(tenant_id)
    .bind(SYSTEM_DEFAULT_AGENT_REVISION_UID)
    .execute(tx.as_mut())
    .await
    .expect("seed learning-erasure session agent context");
    sqlx::query(
        "INSERT INTO task_segments \
            (id, session_id, storage_partition_id, user_id, tenant_id, segment_index, \
             started_at, ended_at, outcome, tools_used, skills_activated, turn_count, token_cost) \
         VALUES ($1, $2, $3, $4, $3, 0, now(), now(), 'resolved', '{}', '{}', 1, 0)",
    )
    .bind(segment_id)
    .bind(session_id)
    .bind(storage_partition_id)
    .bind(subject_user_id)
    .execute(tx.as_mut())
    .await
    .expect("seed learning-erasure task segment");
    sqlx::query(
        "INSERT INTO experience_records \
            (id, segment_id, session_id, storage_partition_id, user_id, tenant_id, \
             task_summary, task_fingerprint, task_fingerprint_payload, task_facets, outcome, \
             confidence, assessment_policy_version, extraction_policy_version) \
         VALUES ($1, $2, $3, $4, $5, $4, 'redacted privacy fixture', $6, \
                 '{}'::JSONB, '{}'::JSONB, 'resolved', 0.9, 'assessment-v1', 'extract-v1')",
    )
    .bind(experience_id)
    .bind(segment_id)
    .bind(session_id)
    .bind(storage_partition_id)
    .bind(subject_user_id)
    .bind(format!("privacy-erasure-{experience_id}"))
    .execute(tx.as_mut())
    .await
    .expect("seed subject learning experience");
    tx.commit().await.expect("commit learning-erasure fixture");
    experience_id
}

async fn create_privacy_skill_draft(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    name: &str,
    description: &str,
    body: &[u8],
) -> StoredArtifactRevision {
    let document: ArtifactDocument = serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "skill",
        "metadata": { "name": name, "description": description },
        "definition": {
            "type": "skill",
            "spec": { "instructions": { "path": "SKILL.md" } }
        }
    }))
    .expect("valid privacy skill document");
    let source = document.to_yaml().expect("serialize privacy skill");
    registry
        .create_draft(
            scope,
            NewArtifactDraft {
                document: &document,
                source_format: "yaml",
                source_text: source.as_bytes(),
                files: &[NewArtifactFile::new("SKILL.md", body.to_vec())],
            },
        )
        .await
        .expect("create privacy skill revision")
}

async fn seed_digest_and_lineage(
    pool: &PgPool,
    tenant_id: Uuid,
    subject: Uuid,
    storage_partition_id: &str,
) {
    let mut conn = begin_app_scoped_tx(pool, TenantId::from(tenant_id), &subject.to_string())
        .await
        .expect("begin contact-scoped digest/lineage seed");
    sqlx::query(
        r#"
        INSERT INTO moa.memory_digests
            (storage_partition_id, user_id, content, version, updated_at)
        VALUES ($1, $2, $3, 1, now())
        "#,
    )
    .bind(storage_partition_id)
    .bind(subject.to_string())
    .bind("What I know about this contact:\n- prefers dark mode\n")
    .execute(conn.as_mut())
    .await
    .expect("seed memory digest row");
    sqlx::query(
        r#"
        INSERT INTO moa.retrieval_lineage
            (storage_partition_id, user_id, session_id, turn_seq, uid, rank, retrieved_at)
        VALUES ($1, $2, $3, 1, $4, 1, now())
        "#,
    )
    .bind(storage_partition_id)
    .bind(subject.to_string())
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .execute(conn.as_mut())
    .await
    .expect("seed retrieval lineage row");
    conn.commit().await.expect("commit digest/lineage seed");
}

async fn digest_row_count(pool: &PgPool, storage_partition_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.memory_digests WHERE storage_partition_id = $1",
    )
    .bind(storage_partition_id)
    .fetch_one(pool)
    .await
    .expect("count digest rows")
}

async fn lineage_row_count(pool: &PgPool, storage_partition_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.retrieval_lineage WHERE storage_partition_id = $1",
    )
    .bind(storage_partition_id)
    .fetch_one(pool)
    .await
    .expect("count lineage rows")
}

async fn privacy_export_audit_count(pool: &PgPool, tenant_id: Uuid, subject: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog \
         WHERE tenant_id = $1 AND op = 'export' AND target_kind = 'user' AND target_uid = $2",
    )
    .bind(tenant_id)
    .bind(subject)
    .fetch_one(pool)
    .await
    .expect("count privacy export audit rows")
}

#[tokio::test]
async fn approval_jti_replay_blocked_through_erase_db_memory() {
    // Pins: reusing one approval JTI for a DIFFERENT erase request (a different
    // request fingerprint) is rejected, so a durable, resumable erasure job never
    // lets an approval token become generally reusable across requests.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    create_erase_test_node(
        store.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject.to_string(),
        "replay-guarded erasure fact",
    )
    .await;
    // A single signed approval token (one JTI) reused across two erase invocations.
    let claims = valid_claims_for(subject, tenant_id, "erase");
    let first = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "first erase consumes the token".to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims: claims.clone(),
        pii_vault_secret: None,
        require_dual_control: false,
    };
    let first_response = run_privacy_erase(first)
        .await
        .expect("first erase succeeds");
    assert_eq!(first_response.candidate_count, 1);

    let replay = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "replayed token must be rejected".to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims,
        pii_vault_secret: None,
        require_dual_control: false,
    };

    let error = run_privacy_erase(replay)
        .await
        .expect_err("reusing a consumed approval JTI must be rejected");
    let rendered = format!("{error:?}");
    assert!(
        rendered.contains("replayed"),
        "replayed approval token should be rejected, got: {rendered}"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_cross_workspace_is_noop_for_graph_data() {
    // Pins: erase candidate enumeration stays scoped to the requested workspace.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_a, workspace_a) = tenant_workspace();
    let (tenant_b, workspace_b) = tenant_workspace();
    let subject = Uuid::now_v7();
    let uid_b = create_erase_test_node(
        store.pool(),
        tenant_b,
        subject,
        &workspace_b,
        &subject.to_string(),
        "other workspace fact",
    )
    .await;
    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_a),
        storage_partition_id: workspace_a.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "wrong workspace erase".to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims: valid_claims_for(subject, tenant_a, "erase"),
        pii_vault_secret: None,
        require_dual_control: false,
    };

    let response = run_privacy_erase(ctx)
        .await
        .expect("wrong workspace erasure is idempotent");

    assert_eq!(response.erased_count, 0);
    assert_eq!(node_count(store.pool(), uid_b).await, 1);
    assert_eq!(
        total_erase_changelog_count(store.pool(), &workspace_a).await,
        1,
        "the target tenant retains one redacted audit record for the erase attempt"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_contact_requires_explicit_scope() {
    // Pins: destructive contact erasure requires an explicit contact erasure boundary.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let contact_id = Uuid::now_v7();
    seed_contact(
        store.pool(),
        tenant_id,
        &storage_partition_id,
        contact_id,
        "unverified",
    )
    .await;
    create_erase_test_node(
        store.pool(),
        tenant_id,
        contact_id,
        &storage_partition_id,
        &contact_user_id(contact_id),
        "contact scope fact",
    )
    .await;
    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: contact_id,
        subject_user_id: contact_id.to_string(),
        reason: "contact erase".to_string(),
        dry_run: true,
        contact_erasure_scope: None,
        claims: valid_claims_for(contact_id, tenant_id, "erase"),
        pii_vault_secret: None,
        require_dual_control: false,
    };

    let error = run_privacy_erase(ctx)
        .await
        .expect_err("contact erasure without scope should fail");

    assert!(format!("{error:?}").contains("contact_erasure_scope is required"));

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_unverified_contact_only() {
    // Pins: specified-contact erasure deletes only the requested contact subject.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let contact_id = Uuid::now_v7();
    let other_contact_id = Uuid::now_v7();
    seed_contact(
        store.pool(),
        tenant_id,
        &storage_partition_id,
        contact_id,
        "unverified",
    )
    .await;
    seed_contact(
        store.pool(),
        tenant_id,
        &storage_partition_id,
        other_contact_id,
        "unverified",
    )
    .await;
    let contact_uid = create_erase_test_node(
        store.pool(),
        tenant_id,
        contact_id,
        &storage_partition_id,
        &contact_user_id(contact_id),
        "contact-only fact",
    )
    .await;
    let other_uid = create_erase_test_node(
        store.pool(),
        tenant_id,
        other_contact_id,
        &storage_partition_id,
        &contact_user_id(other_contact_id),
        "other contact fact",
    )
    .await;
    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: contact_id,
        subject_user_id: contact_id.to_string(),
        reason: "contact erase".to_string(),
        dry_run: false,
        contact_erasure_scope: Some(ContactErasureScope::SpecifiedContact),
        claims: valid_claims_for(contact_id, tenant_id, "erase"),
        pii_vault_secret: None,
        require_dual_control: false,
    };

    let response = run_privacy_erase(ctx).await.expect("erase contact");

    assert_eq!(response.candidate_count, 1);
    assert_eq!(response.erased_count, 1);
    assert_eq!(node_count(store.pool(), contact_uid).await, 0);
    assert_eq!(node_count(store.pool(), other_uid).await, 1);

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_verified_contact_with_linked_unverified_contacts() {
    // Pins: verified contact erasure includes linked unverified contacts only when explicitly requested.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let contact_id = Uuid::now_v7();
    let linked_contact_id = Uuid::now_v7();
    seed_contact(
        store.pool(),
        tenant_id,
        &storage_partition_id,
        contact_id,
        "verified",
    )
    .await;
    seed_merged_contact(
        store.pool(),
        tenant_id,
        &storage_partition_id,
        linked_contact_id,
        contact_id,
    )
    .await;
    let contact_uid = create_erase_test_node(
        store.pool(),
        tenant_id,
        contact_id,
        &storage_partition_id,
        &contact_user_id(contact_id),
        "verified contact fact",
    )
    .await;
    let linked_uid = create_erase_test_node(
        store.pool(),
        tenant_id,
        linked_contact_id,
        &storage_partition_id,
        &contact_user_id(linked_contact_id),
        "linked contact fact",
    )
    .await;
    let subject_user_id = contact_user_id(contact_id);
    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: contact_id,
        subject_user_id: subject_user_id.clone(),
        reason: "verified contact erase".to_string(),
        dry_run: false,
        contact_erasure_scope: Some(ContactErasureScope::SpecifiedAndLinkedContacts),
        claims: valid_claims_for_user_id(&subject_user_id, tenant_id, "erase"),
        pii_vault_secret: None,
        require_dual_control: false,
    };

    let response = run_privacy_erase(ctx)
        .await
        .expect("erase linked contact subjects");

    assert_eq!(response.candidate_count, 2);
    assert_eq!(response.erased_count, 2);
    assert_eq!(node_count(store.pool(), contact_uid).await, 0);
    assert_eq!(node_count(store.pool(), linked_uid).await, 0);
    assert!(
        response
            .sample
            .iter()
            .any(|entry| entry["privacy_subject_provenance"] == "linked_contact")
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_export_contact_data_sections_label_linked_provenance() {
    // Pins: contact subject-access export labels rows from linked contact memory.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let contact_id = Uuid::now_v7();
    let linked_contact_id = Uuid::now_v7();
    let contact_uid = create_embedded_erase_test_node(
        store.pool(),
        tenant_id,
        contact_id,
        &storage_partition_id,
        &contact_user_id(contact_id),
        "export primary fact",
    )
    .await;
    let linked_uid = create_erase_test_node(
        store.pool(),
        tenant_id,
        linked_contact_id,
        &storage_partition_id,
        &contact_user_id(linked_contact_id),
        "export linked fact",
    )
    .await;
    let subject_user_id = contact_user_id(contact_id);
    let decoy_contact_id = Uuid::now_v7();
    let decoy_uid = create_embedded_erase_test_node(
        store.pool(),
        tenant_id,
        decoy_contact_id,
        &storage_partition_id,
        &contact_user_id(decoy_contact_id),
        &format!("substring-only coincidence {subject_user_id}"),
    )
    .await;
    sqlx::query(
        r#"
        INSERT INTO moa.edge_index
            (uid, label, start_uid, end_uid, tenant_id, storage_partition_id,
             user_id, contact_id, properties)
        VALUES ($1, 'RELATES_TO', $2, $3, $4, $5, NULL, NULL,
                '{"typed_endpoint": true}'::jsonb)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(contact_uid)
    .bind(linked_uid)
    .bind(tenant_id)
    .bind(&storage_partition_id)
    .execute(store.pool())
    .await
    .expect("seed subject-owned endpoint edge");
    let export_dir = tempdir().expect("tempdir");
    let ctx = PrivacyExportContext {
        audit_pool: store.pool().clone(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition: Some(storage_partition_id.clone()),
        subject_user: contact_id,
        subject_user_id: subject_user_id.clone(),
        subjects: vec![
            PrivacySubject::primary(subject_user_id.clone(), contact_id),
            PrivacySubject {
                user_id: contact_user_id(linked_contact_id),
                target_uid: linked_contact_id,
                provenance: PrivacySubjectProvenance::LinkedContact,
            },
        ],
        reason: "GDPR Art.15 contact request".to_string(),
        claims: valid_claims_for_user_id(&subject_user_id, tenant_id, "export"),
    };

    let mut snapshot = begin_privacy_export_snapshot(store.pool())
        .await
        .expect("begin privacy export snapshot");
    let snapshot_settings: (String, String, String, String, String) = sqlx::query_as(
        "SELECT current_user::text, current_setting('transaction_isolation'), \
                current_setting('transaction_read_only'), \
                current_setting('statement_timeout'), \
                current_setting('idle_in_transaction_session_timeout')",
    )
    .fetch_one(snapshot.as_mut())
    .await
    .expect("read export snapshot settings");
    assert_eq!(
        snapshot_settings,
        (
            "moa_auditor".to_string(),
            "repeatable read".to_string(),
            "on".to_string(),
            "30s".to_string(),
            "30s".to_string(),
        )
    );
    let counts = collect_privacy_export_data_sections(&ctx, snapshot.as_mut(), export_dir.path())
        .await
        .expect("collect export sections");
    snapshot.commit().await.expect("commit export snapshot");
    let facts = tokio::fs::read_to_string(export_dir.path().join("facts.jsonl"))
        .await
        .expect("read facts export");
    let rows = facts
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse fact row"))
        .collect::<Vec<_>>();
    let relationships = tokio::fs::read_to_string(export_dir.path().join("relationships.jsonl"))
        .await
        .expect("read relationships export");
    let embeddings = tokio::fs::read_to_string(export_dir.path().join("embeddings.jsonl"))
        .await
        .expect("read embeddings export");
    let changelog = tokio::fs::read_to_string(export_dir.path().join("changelog.jsonl"))
        .await
        .expect("read changelog export");

    assert_eq!(counts["facts"], 2);
    assert_eq!(counts["relationships"], 1);
    assert_eq!(counts["embeddings"], 1);
    assert_eq!(counts["changelog"], 2);
    assert_eq!(relationships.lines().count(), 1);
    assert_eq!(embeddings.lines().count(), 1);
    assert_eq!(changelog.lines().count(), 2);
    assert!(embeddings.contains(&contact_uid.to_string()));
    assert!(!embeddings.contains(&decoy_uid.to_string()));
    assert!(!facts.contains("substring-only coincidence"));
    assert!(!changelog.contains("substring-only coincidence"));
    assert!(
        rows.iter()
            .any(|row| row["privacy_subject_provenance"] == "primary")
    );
    assert!(
        rows.iter()
            .any(|row| row["privacy_subject_provenance"] == "linked_contact")
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_export_allows_exactly_1000_snapshot_subjects_db_memory() {
    // Pins: the primary contact plus 999 verified links is accepted as the exact
    // subject-expansion ceiling, and the manifest records every resolved subject
    // in exact primary-then-linked order with exact provenance.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    seed_contact(
        store.pool(),
        tenant_id,
        &storage_partition_id,
        subject,
        "verified",
    )
    .await;
    let linked = (0..999).map(|_| Uuid::now_v7()).collect::<Vec<_>>();
    seed_merged_contacts(
        store.pool(),
        tenant_id,
        &storage_partition_id,
        subject,
        &linked,
    )
    .await;
    let subject_user_id = contact_user_id(subject);
    let request = export_request(tenant_id, &subject_user_id, None);
    let claims = valid_claims_for_user_id(&subject_user_id, tenant_id, "export");

    let response = execute_privacy_export(
        store.pool().clone(),
        store.pool().clone(),
        TenantId::from(tenant_id),
        request,
        claims,
        export_compliance_config(),
    )
    .await
    .expect("1000 resolved subjects must export");

    let mut ordered_linked = linked.clone();
    ordered_linked.sort_unstable();
    let expected_subjects = std::iter::once(json!({
        "user_id": subject_user_id,
        "provenance": "primary",
    }))
    .chain(ordered_linked.into_iter().map(|contact_id| {
        json!({
            "user_id": contact_user_id(contact_id),
            "provenance": "linked_contact",
        })
    }))
    .collect::<Vec<_>>();
    assert_eq!(response.manifest["subjects"], json!(expected_subjects));
    assert_eq!(
        privacy_export_audit_count(store.pool(), tenant_id, subject).await,
        1
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_export_rejects_1001_snapshot_subjects_before_sections_db_memory() {
    // Pins: primary plus 1000 linked contacts fails at the bounded snapshot
    // relation before section reads and never writes a success audit.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    seed_contact(
        store.pool(),
        tenant_id,
        &storage_partition_id,
        subject,
        "verified",
    )
    .await;
    let linked = (0..1_000).map(|_| Uuid::now_v7()).collect::<Vec<_>>();
    seed_merged_contacts(
        store.pool(),
        tenant_id,
        &storage_partition_id,
        subject,
        &linked,
    )
    .await;
    let subject_user_id = contact_user_id(subject);
    let request = export_request(tenant_id, &subject_user_id, None);
    let claims = valid_claims_for_user_id(&subject_user_id, tenant_id, "export");

    let error = execute_privacy_export(
        store.pool().clone(),
        store.pool().clone(),
        TenantId::from(tenant_id),
        request,
        claims,
        export_compliance_config(),
    )
    .await
    .expect_err("1001 resolved subjects must fail closed");
    assert!(
        format!("{error:?}").contains("exceeds the 1000-subject limit"),
        "unexpected subject-cap error: {error:?}"
    );
    assert_eq!(
        privacy_export_audit_count(store.pool(), tenant_id, subject).await,
        0
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_export_uses_background_reads_and_foreground_writes_across_databases_db_memory() {
    // Pins: export data exists only in the background database, while approval
    // consumption and the terminal success audit exist only in the foreground.
    let (foreground, foreground_url, foreground_schema) = testing::create_isolated_test_store()
        .await
        .expect("create foreground test store");
    let (background, background_url, background_schema) = testing::create_isolated_test_store()
        .await
        .expect("create background test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    create_erase_test_node(
        background.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject.to_string(),
        "background-only export fact",
    )
    .await;
    let request = export_request(tenant_id, &subject.to_string(), None);
    let claims = valid_claims(subject, tenant_id);
    let approval_jti = claims.jti.clone();

    let response = execute_privacy_export(
        foreground.pool().clone(),
        background.pool().clone(),
        TenantId::from(tenant_id),
        request,
        claims,
        export_compliance_config(),
    )
    .await
    .expect("cross-database export succeeds");
    assert_eq!(response.counts.get("facts"), Some(&1));
    let foreground_jti: i64 =
        sqlx::query_scalar("SELECT count(*) FROM moa.audit_jti_used WHERE jti = $1")
            .bind(&approval_jti)
            .fetch_one(foreground.pool())
            .await
            .expect("count foreground approval JTI");
    let background_jti: i64 =
        sqlx::query_scalar("SELECT count(*) FROM moa.audit_jti_used WHERE jti = $1")
            .bind(&approval_jti)
            .fetch_one(background.pool())
            .await
            .expect("count background approval JTI");
    assert_eq!((foreground_jti, background_jti), (1, 0));
    assert_eq!(
        privacy_export_audit_count(foreground.pool(), tenant_id, subject).await,
        1
    );
    assert_eq!(
        privacy_export_audit_count(background.pool(), tenant_id, subject).await,
        0
    );

    drop(foreground);
    drop(background);
    testing::cleanup_test_schema(&foreground_url, &foreground_schema)
        .await
        .expect("drop foreground schema");
    testing::cleanup_test_schema(&background_url, &background_schema)
        .await
        .expect("drop background schema");
}

#[tokio::test]
async fn privacy_export_post_snapshot_failure_writes_no_success_audit_db_memory() {
    // Pins: archive/encryption failures happen after the snapshot commit, and a
    // failed export never records the terminal success audit.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    create_erase_test_node(
        store.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject.to_string(),
        "post-snapshot failure fact",
    )
    .await;
    let request = export_request(
        tenant_id,
        &subject.to_string(),
        Some("not-an-armored-pgp-recipient".to_string()),
    );
    let claims = valid_claims(subject, tenant_id);

    let error = execute_privacy_export(
        store.pool().clone(),
        store.pool().clone(),
        TenantId::from(tenant_id),
        request,
        claims,
        export_compliance_config(),
    )
    .await
    .expect_err("invalid recipient must fail after snapshot collection");
    assert!(
        format!("{error:?}").contains("gpg encryption failed"),
        "unexpected post-snapshot error: {error:?}"
    );
    assert_eq!(
        privacy_export_audit_count(store.pool(), tenant_id, subject).await,
        0
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_export_archived_skills_follow_recursive_typed_provenance_not_substrings_db_memory()
{
    // Pins: artifact lifecycle state controls serving, not subject-access
    // visibility. An archived revision remains exportable through normalized
    // contribution provenance even when none of its retained bytes contains the
    // privacy subject identifier.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let tenant_id = TenantId::from(tenant_id);
    let subject = Uuid::now_v7();
    let subject_user_id = subject.to_string();
    let scope = ActionRuleScope::Tenant { tenant_id };
    let registry = ArtifactRegistry::new(store.pool().clone());
    let bridge = create_privacy_skill_draft(
        &registry,
        &scope,
        &format!("privacy-bridge-{}", Uuid::now_v7()),
        "typed bridge revision",
        b"# Bridge\nTyped provenance only.\n",
    )
    .await;
    let target = create_privacy_skill_draft(
        &registry,
        &scope,
        &format!("privacy-target-{}", Uuid::now_v7()),
        "typed target revision",
        b"# Target\nTyped provenance only.\n",
    )
    .await;
    let decoy_body = format!("# Decoy\nUnrelated text happens to contain {subject_user_id}.\n");
    let decoy = create_privacy_skill_draft(
        &registry,
        &scope,
        &format!("privacy-decoy-{}", Uuid::now_v7()),
        &format!("substring-only coincidence {subject_user_id}"),
        decoy_body.as_bytes(),
    )
    .await;
    let revision_uids = [bridge.revision_uid, target.revision_uid, decoy.revision_uid];
    sqlx::query(
        "UPDATE moa.artifact_revision SET status = 'archived' WHERE revision_uid = ANY($1)",
    )
    .bind(revision_uids)
    .execute(store.pool())
    .await
    .expect("archive retained and decoy revisions");

    let experience_id = seed_learning_experience(
        store.pool(),
        tenant_id.0,
        &storage_partition_id,
        &subject_user_id,
    )
    .await;
    let seed_candidate = Uuid::now_v7();
    let promoted_candidate = Uuid::now_v7();
    let revision_candidate = Uuid::now_v7();
    let candidate_ids = [seed_candidate, promoted_candidate, revision_candidate];
    let mut provenance_tx = store
        .pool()
        .begin()
        .await
        .expect("begin archived-skill provenance fixture");
    sqlx::query(
        r#"
        INSERT INTO learning_candidates
            (id, tenant_id, storage_partition_id, user_id, candidate_type,
             proposal_kind, status, payload, risk_class)
        SELECT candidate_id, $2, $2, $3, 'skill', 'skill_draft',
               'proposed', '{}'::jsonb, 'low'
        FROM unnest($1::uuid[]) AS candidate(candidate_id)
        "#,
    )
    .bind(candidate_ids)
    .bind(&storage_partition_id)
    .bind(&subject_user_id)
    .execute(provenance_tx.as_mut())
    .await
    .expect("seed recursive learning candidates");
    sqlx::query(
        r#"
        INSERT INTO learning_candidate_source
            (id, candidate_id, tenant_id, storage_partition_id, user_id, source_kind,
             experience_id, promotion_candidate_id, artifact_revision_uid)
        VALUES
            ($1, $2, $9, $9, $10, 'experience', $3, NULL, NULL),
            ($4, $5, $9, $9, $10, 'promotion_candidate', NULL, $2, NULL),
            ($6, $7, $9, $9, $10, 'artifact_revision', NULL, NULL, $8)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(seed_candidate)
    .bind(experience_id)
    .bind(Uuid::now_v7())
    .bind(promoted_candidate)
    .bind(Uuid::now_v7())
    .bind(revision_candidate)
    .bind(bridge.revision_uid)
    .bind(&storage_partition_id)
    .bind(&subject_user_id)
    .execute(provenance_tx.as_mut())
    .await
    .expect("seed promotion and revision reachability");
    sqlx::query(
        r#"
        INSERT INTO moa.artifact_revision_contribution
            (contribution_uid, storage_partition_id, user_id, revision_uid,
             candidate_id, tenant_id, contribution_kind)
        VALUES
            ($1, $2, $3, $4, $5, $2, 'generated_definition'),
            ($6, $2, $3, $7, $8, $2, 'generated_definition')
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(&storage_partition_id)
    .bind(&subject_user_id)
    .bind(bridge.revision_uid)
    .bind(promoted_candidate)
    .bind(Uuid::now_v7())
    .bind(target.revision_uid)
    .bind(revision_candidate)
    .execute(provenance_tx.as_mut())
    .await
    .expect("seed recursive revision contributions");
    provenance_tx
        .commit()
        .await
        .expect("commit archived-skill provenance fixture");

    let export_dir = tempdir().expect("tempdir");
    let ctx = PrivacyExportContext {
        audit_pool: store.pool().clone(),
        tenant_id,
        storage_partition: Some(storage_partition_id),
        subject_user: subject,
        subject_user_id: subject_user_id.clone(),
        subjects: vec![PrivacySubject::primary(subject_user_id.clone(), subject)],
        reason: "subject access request".to_string(),
        claims: valid_claims_for_user_id(&subject_user_id, tenant_id.0, "export"),
    };
    let mut snapshot = begin_privacy_export_snapshot(store.pool())
        .await
        .expect("begin privacy export snapshot");
    let counts = collect_privacy_export_data_sections(&ctx, snapshot.as_mut(), export_dir.path())
        .await
        .expect("collect archived skill export");
    snapshot.commit().await.expect("commit export snapshot");
    let rows = tokio::fs::read_to_string(export_dir.path().join("skills.jsonl"))
        .await
        .expect("read skill export")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse skill row"))
        .collect::<Vec<_>>();

    assert_eq!(counts["skills"], 2);
    assert_eq!(counts["artifact_revision_contributions"], 2);
    assert_eq!(rows.len(), 2);
    let exported_revisions = rows
        .iter()
        .map(|row| {
            Uuid::parse_str(row["revision_uid"].as_str().expect("exported revision uid"))
                .expect("revision uid is UUID")
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        exported_revisions,
        std::collections::BTreeSet::from([bridge.revision_uid, target.revision_uid])
    );
    assert!(!exported_revisions.contains(&decoy.revision_uid));
    assert!(rows.iter().all(|row| row["status"] == "archived"));
    assert!(
        rows.iter()
            .all(|row| row["files"].as_array().map(Vec::len) == Some(1))
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_export_erasure_decisions_are_subject_scoped_and_attempt_complete_db_memory() {
    // Pins: one subject's export cannot reveal another subject's erasure
    // decisions, and an earlier dry run cannot mask a later applied attempt for
    // the same record.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let tenant_id = TenantId::from(tenant_id);
    let subject = Uuid::now_v7();
    let other_subject = Uuid::now_v7();
    let subject_user_id = subject.to_string();
    let other_subject_user_id = other_subject.to_string();
    let record_id = Uuid::now_v7().to_string();

    let planned = RecordDecision {
        kind: ErasureRecordKind::ExperienceRecord,
        record_id: record_id.clone(),
        disposition: ErasureDisposition::Erased,
        applied: false,
        reason: Some("dry run".to_string()),
    };
    let applied = RecordDecision {
        applied: true,
        reason: Some("applied erase".to_string()),
        ..planned.clone()
    };
    assert_eq!(
        record_decisions(
            store.pool(),
            tenant_id,
            &subject_user_id,
            "approval-jti:dry_run",
            std::slice::from_ref(&planned),
        )
        .await
        .expect("record subject dry-run decision"),
        1
    );
    assert_eq!(
        record_decisions(
            store.pool(),
            tenant_id,
            &subject_user_id,
            "approval-jti:dry_run",
            std::slice::from_ref(&planned),
        )
        .await
        .expect("replay subject dry-run decision"),
        0
    );
    assert_eq!(
        record_decisions(
            store.pool(),
            tenant_id,
            &subject_user_id,
            "approval-jti:applied",
            std::slice::from_ref(&applied),
        )
        .await
        .expect("record later applied decision"),
        1
    );
    record_decisions(
        store.pool(),
        tenant_id,
        &other_subject_user_id,
        "attempt-other-subject",
        &[applied],
    )
    .await
    .expect("record other subject decision");

    let export_dir = tempdir().expect("tempdir");
    let ctx = PrivacyExportContext {
        audit_pool: store.pool().clone(),
        tenant_id,
        storage_partition: Some(storage_partition_id),
        subject_user: subject,
        subject_user_id: subject_user_id.clone(),
        subjects: vec![PrivacySubject::primary(subject_user_id.clone(), subject)],
        reason: "subject access request".to_string(),
        claims: valid_claims_for_user_id(&subject_user_id, tenant_id.0, "export"),
    };
    let mut snapshot = begin_privacy_export_snapshot(store.pool())
        .await
        .expect("begin privacy export snapshot");
    let counts = collect_privacy_export_data_sections(&ctx, snapshot.as_mut(), export_dir.path())
        .await
        .expect("collect subject-scoped export");
    snapshot.commit().await.expect("commit export snapshot");
    let rows = tokio::fs::read_to_string(export_dir.path().join("erasure_decisions.jsonl"))
        .await
        .expect("read erasure decisions")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse decision row"))
        .collect::<Vec<_>>();

    assert_eq!(counts["erasure_decisions"], 2);
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|row| row["subject_user_id"] == subject_user_id)
    );
    assert_eq!(
        rows.iter()
            .map(|row| row["attempt_id"].as_str().expect("attempt id"))
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["approval-jti:applied", "approval-jti:dry_run"])
    );
    assert_eq!(
        rows.iter()
            .map(|row| row["applied"].as_bool().expect("applied flag"))
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([false, true])
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_unknown_op_rejected() {
    // Pins: privacy erasure does not add unsupported crypto-shredding op variants.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, _storage_partition_id) = tenant_workspace();
    let mut tx = begin_app_scoped_tx(
        store.pool(),
        TenantId::from(tenant_id),
        &Uuid::now_v7().to_string(),
    )
    .await
    .expect("begin app tx");

    let error = sqlx::query(
        r#"
        INSERT INTO moa.graph_changelog
            (storage_partition_id, actor_id, actor_kind, op, target_kind, target_label,
             target_uid, payload, pii_class)
        VALUES ($1, 'ops-admin', 'admin', 'deferred_encryption', 'contact', 'Contact',
                $2, '{}'::jsonb, 'phi')
        "#,
    )
    .bind(tenant_id.to_string())
    .bind(Uuid::now_v7())
    .execute(tx.as_mut())
    .await
    .expect_err("unknown changelog op must be rejected");
    assert!(error.to_string().contains("graph_changelog_op_check"));
    tx.rollback().await.expect("rollback failed insert");

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_export_archive_round_trip() {
    // Pins: server-side privacy export signs a manifest and returns a readable archive artifact.
    let subject = Uuid::now_v7();
    let dir = tempdir().expect("tempdir");
    let export_dir = dir.path().join("export");
    tokio::fs::create_dir_all(&export_dir)
        .await
        .expect("create export dir");
    tokio::fs::write(export_dir.join("facts.jsonl"), "{}\n")
        .await
        .expect("write facts");
    tokio::fs::write(export_dir.join("entities.jsonl"), "")
        .await
        .expect("write entities");
    tokio::fs::write(export_dir.join("relationships.jsonl"), "")
        .await
        .expect("write relationships");
    tokio::fs::write(export_dir.join("embeddings.jsonl"), "")
        .await
        .expect("write embeddings");
    tokio::fs::write(export_dir.join("skills.jsonl"), "")
        .await
        .expect("write skills");
    tokio::fs::write(export_dir.join("changelog.jsonl"), "")
        .await
        .expect("write changelog");

    let (tenant_id, storage_partition_id) = tenant_workspace();
    let claims = valid_claims_for(subject, tenant_id, "export");
    let ctx = PrivacyExportContext {
        audit_pool: PgPool::connect_lazy("postgres://unused").expect("lazy pool"),
        tenant_id: TenantId::from(tenant_id),
        storage_partition: Some(storage_partition_id),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        subjects: vec![PrivacySubject::primary(subject.to_string(), subject)],
        reason: "GDPR Art.15 request".to_string(),
        claims,
    };
    let counts = BTreeMap::from([
        ("facts", 1),
        ("entities", 0),
        ("relationships", 0),
        ("embeddings", 0),
        ("skills", 0),
        ("changelog", 0),
    ]);
    write_export_readme(&ctx, &counts, &export_dir)
        .await
        .expect("write readme");
    let signer = Ed25519ManifestSigner {
        key_id: "test-key".to_string(),
        signing_key: signing_key(),
    };
    let manifest_json = write_manifest(&export_dir, &signer, &ctx, &counts)
        .await
        .expect("write manifest");

    let manifest = tokio::fs::read(export_dir.join("manifest.json"))
        .await
        .expect("read manifest");
    let signature = tokio::fs::read(export_dir.join("manifest.sig"))
        .await
        .expect("read sig");
    let signature: [u8; 64] = signature
        .as_slice()
        .try_into()
        .expect("ed25519 signature bytes");
    signer
        .signing_key
        .verifying_key()
        .verify(&manifest, &Signature::from_bytes(&signature))
        .expect("signature verifies");
    assert_eq!(manifest_json["encryption"], "none");
    assert!(
        manifest_json["files"]
            .as_array()
            .expect("files")
            .iter()
            .any(|entry| entry["name"] == "facts.jsonl" && entry["sha256"].as_str().is_some())
    );

    let archive_bytes = finalize_archive_to_bytes(&export_dir, None)
        .await
        .expect("finalize archive");
    let decoder = flate2::read::GzDecoder::new(&archive_bytes[..]);
    let mut archive = tar::Archive::new(decoder);
    let mut names = Vec::new();
    for entry in archive.entries().expect("entries") {
        let mut entry = entry.expect("entry");
        let path = entry.path().expect("entry path").display().to_string();
        let mut sink = Vec::new();
        entry.read_to_end(&mut sink).expect("read entry");
        names.push(path);
    }
    assert!(names.iter().any(|name| name == "export/manifest.json"));
    assert!(names.iter().any(|name| name == "export/manifest.sig"));
    assert!(names.iter().any(|name| name == "export/facts.jsonl"));
}

async fn dual_control_request_count(pool: &PgPool, tenant_id: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.dual_control_request WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await
    .expect("count dual-control requests")
}

#[tokio::test]
async fn privacy_erase_executes_after_distinct_dual_control_approval_db_memory() {
    // Pins: with the dual-control policy ON, an erasure executes only after a
    // SECOND, DISTINCT tenant admin approves the specific request. request(admin A)
    // -> approve(admin B, B != A) -> erase purges the subject's graph node and
    // consumes the approval. Mutation check: dropping the consume step in
    // ensure_erase_dual_control would let the erase run without an approval, which
    // the fail-closed test below independently pins.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    let uid = create_erase_test_node(
        store.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject.to_string(),
        "dual-control erasure fact",
    )
    .await;
    let reason = "GDPR Art.17 request";
    let operation_ref = erase_operation_ref(
        TenantId::from(tenant_id),
        &subject.to_string(),
        None,
        reason,
    );
    let admin_a = Uuid::now_v7().to_string();
    let admin_b = Uuid::now_v7().to_string();

    let request_id = dual_control::request(
        store.pool(),
        TenantId::from(tenant_id),
        DUAL_CONTROL_OPERATION_ERASE,
        &operation_ref,
        &admin_a,
    )
    .await
    .expect("first admin raises dual-control request");
    dual_control::approve(
        store.pool(),
        TenantId::from(tenant_id),
        request_id,
        &admin_b,
    )
    .await
    .expect("distinct second admin approves");

    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: reason.to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims: valid_claims_for(subject, tenant_id, "erase"),
        pii_vault_secret: None,
        require_dual_control: true,
    };

    let response = run_privacy_erase(ctx)
        .await
        .expect("erase executes after distinct approval");
    assert!(matches!(response.status, PrivacyEraseStatus::Completed));
    assert_eq!(response.erased_count, 1);
    assert_eq!(node_count(store.pool(), uid).await, 0);

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn dual_control_rejects_self_approval_segregation_of_duties_db_memory() {
    // Pins: segregation of duties. A dual-control request cannot be approved by the
    // same operator that raised it. approve() rejects it with SelfApproval, leaves
    // the request pending, and a DISTINCT admin can still approve it afterward.
    // Mutation check: removing the `approver == requested_by` guard in approve()
    // makes this return Ok (or a Storage error from the DB backstop constraint)
    // instead of SelfApproval, so the assertion fails.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, _storage_partition_id) = tenant_workspace();
    let admin_a = Uuid::now_v7().to_string();

    let request_id = dual_control::request(
        store.pool(),
        TenantId::from(tenant_id),
        DUAL_CONTROL_OPERATION_ERASE,
        "operation-ref-sod",
        &admin_a,
    )
    .await
    .expect("first admin raises dual-control request");

    let error = dual_control::approve(
        store.pool(),
        TenantId::from(tenant_id),
        request_id,
        &admin_a,
    )
    .await
    .expect_err("a request must not be approvable by its own requester");
    assert!(
        matches!(error, DualControlError::SelfApproval),
        "expected SelfApproval, got {error:?}"
    );

    // The rejected self-approval left the request pending, so a distinct admin can
    // still approve it.
    let admin_b = Uuid::now_v7().to_string();
    dual_control::approve(
        store.pool(),
        TenantId::from(tenant_id),
        request_id,
        &admin_b,
    )
    .await
    .expect("a distinct admin can approve the still-pending request");

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_fails_closed_without_dual_control_approval_db_memory() {
    // Pins: with the policy ON and no distinct approval available, erasure is
    // refused (fail closed, 403) before any destructive work — the subject's node
    // survives and no erase changelog row is written.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    let uid = create_erase_test_node(
        store.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject.to_string(),
        "unapproved erasure fact",
    )
    .await;

    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "GDPR Art.17 request".to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims: valid_claims_for(subject, tenant_id, "erase"),
        pii_vault_secret: None,
        require_dual_control: true,
    };

    let error = run_privacy_erase(ctx)
        .await
        .expect_err("erase must be refused when dual control is required but unapproved");
    let rendered = format!("{error:?}");
    assert!(
        rendered.contains("dual-control approval"),
        "expected a dual-control fail-closed refusal, got: {rendered}"
    );

    // Fail closed: nothing was purged.
    assert_eq!(node_count(store.pool(), uid).await, 1);
    assert_eq!(
        total_erase_changelog_count(store.pool(), &storage_partition_id).await,
        0
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_policy_off_needs_no_dual_control_db_memory() {
    // Pins: with the policy OFF (the default), erasure keeps its single-admin
    // behavior — it executes with no dual-control request or approval present at
    // all, so enabling the feature does not regress existing erasure.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let (tenant_id, storage_partition_id) = tenant_workspace();
    let subject = Uuid::now_v7();
    let uid = create_erase_test_node(
        store.pool(),
        tenant_id,
        subject,
        &storage_partition_id,
        &subject.to_string(),
        "policy-off erasure fact",
    )
    .await;

    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        kms: test_kms(),
        tenant_id: TenantId::from(tenant_id),
        storage_partition_id: storage_partition_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "GDPR Art.17 request".to_string(),
        dry_run: false,
        contact_erasure_scope: None,
        claims: valid_claims_for(subject, tenant_id, "erase"),
        pii_vault_secret: None,
        require_dual_control: false,
    };

    let response = run_privacy_erase(ctx)
        .await
        .expect("erase executes without dual control when the policy is off");
    assert!(matches!(response.status, PrivacyEraseStatus::Completed));
    assert_eq!(response.erased_count, 1);
    assert_eq!(node_count(store.pool(), uid).await, 0);
    assert_eq!(
        dual_control_request_count(store.pool(), tenant_id).await,
        0,
        "policy-off erasure must not require any dual-control row"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_dual_control_digest_and_cross_pool_consumption_are_race_safe_db_memory() {
    // Pins: the stored operation reference contains no raw request fields;
    // same-consumer retries both succeed after serialization, while two distinct
    // consumers racing one approval produce exactly one success.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let tenant_id = TenantId::new();
    let subject = Uuid::now_v7();
    let raw_ref =
        format!("tenant={tenant_id}|subject={subject}|reason=highly-sensitive-legal-reason");
    let requester = Uuid::now_v7().to_string();
    let approver = Uuid::now_v7().to_string();
    let request_id = dual_control::request(
        store.pool(),
        tenant_id,
        DUAL_CONTROL_OPERATION_ERASE,
        &raw_ref,
        &requester,
    )
    .await
    .expect("request approval");
    dual_control::approve(store.pool(), tenant_id, request_id, &approver)
        .await
        .expect("approve request");
    let stored: String =
        sqlx::query_scalar("SELECT operation_ref FROM moa.dual_control_request WHERE id = $1")
            .bind(request_id)
            .fetch_one(store.pool())
            .await
            .expect("load stored operation digest");
    assert!(stored.starts_with("v1:blake3:"));
    assert_eq!(stored.len(), "v1:blake3:".len() + 64);
    assert!(!stored.contains(&tenant_id.to_string()));
    assert!(!stored.contains(&subject.to_string()));
    assert!(!stored.contains("highly-sensitive-legal-reason"));

    let pool_a = PgPool::connect(&database_url)
        .await
        .expect("connect consumer pool A");
    let pool_b = PgPool::connect(&database_url)
        .await
        .expect("connect consumer pool B");
    let (same_a, same_b) = tokio::join!(
        dual_control::consume_approval_for(
            &pool_a,
            tenant_id,
            DUAL_CONTROL_OPERATION_ERASE,
            &raw_ref,
            "same-consumer"
        ),
        dual_control::consume_approval_for(
            &pool_b,
            tenant_id,
            DUAL_CONTROL_OPERATION_ERASE,
            &raw_ref,
            "same-consumer"
        )
    );
    assert!(same_a.is_ok(), "first same-consumer result: {same_a:?}");
    assert!(same_b.is_ok(), "second same-consumer result: {same_b:?}");

    let other_ref = format!("{raw_ref}|operation=two");
    let other_request = dual_control::request(
        store.pool(),
        tenant_id,
        DUAL_CONTROL_OPERATION_ERASE,
        &other_ref,
        &requester,
    )
    .await
    .expect("request competing approval");
    dual_control::approve(store.pool(), tenant_id, other_request, &approver)
        .await
        .expect("approve competing request");
    let (different_a, different_b) = tokio::join!(
        dual_control::consume_approval_for(
            &pool_a,
            tenant_id,
            DUAL_CONTROL_OPERATION_ERASE,
            &other_ref,
            "consumer-a"
        ),
        dual_control::consume_approval_for(
            &pool_b,
            tenant_id,
            DUAL_CONTROL_OPERATION_ERASE,
            &other_ref,
            "consumer-b"
        )
    );
    assert_eq!(
        [different_a.is_ok(), different_b.is_ok()]
            .into_iter()
            .filter(|success| *success)
            .count(),
        1,
        "exactly one different consumer may win"
    );
    for error in [different_a, different_b]
        .into_iter()
        .filter_map(Result::err)
    {
        assert!(matches!(error, DualControlError::NoValidApproval));
    }

    pool_a.close().await;
    pool_b.close().await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
