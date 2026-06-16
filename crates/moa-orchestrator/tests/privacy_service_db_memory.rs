//! Privacy service helper coverage.

use std::{collections::BTreeMap, io::Read, sync::Arc};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier};
use moa_core::{ScopeContext, UserId, WorkspaceId};
use moa_lineage_audit::PiiVault;
use moa_memory_graph::{AgeGraphStore, GraphStore, NodeLabel, NodeWriteIntent, PiiClass};
use moa_memory_vector::PgvectorStore;
use moa_orchestrator::services::privacy::{
    ApprovalClaims, ApprovalTokenVerifier, Ed25519ManifestSigner, PrivacyEraseContext,
    PrivacyExportContext, begin_app_scoped_tx, ensure_jti_inserted, execute_privacy_erase,
    finalize_archive_to_bytes, write_export_readme, write_manifest,
};
use moa_session::testing;
use serde_json::json;
use sqlx::PgPool;
use tempfile::tempdir;
use tokio::sync::Mutex;
use uuid::Uuid;

static PRIVACY_ERASE_TEST_LOCK: Mutex<()> = Mutex::const_new(());

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

fn valid_claims(subject: Uuid) -> ApprovalClaims {
    valid_claims_for(subject, "workspace-a", "export")
}

fn valid_claims_for(subject: Uuid, workspace: &str, op: &str) -> ApprovalClaims {
    ApprovalClaims {
        sub: "ops-admin".to_string(),
        jti: Uuid::now_v7().to_string(),
        exp: Utc::now().timestamp() + 300,
        op: op.to_string(),
        subject_user_id: subject.to_string(),
        workspace_id: Some(workspace.to_string()),
        role: None,
        roles: vec!["platform_admin".to_string()],
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

fn erase_test_graph(pool: &PgPool, workspace_id: &str, user_id: &str) -> AgeGraphStore {
    let scope = ScopeContext::user(WorkspaceId::new(workspace_id), UserId::new(user_id));
    let vector = PgvectorStore::new_for_app_role(pool.clone(), scope.clone());
    AgeGraphStore::scoped_for_app_role(pool.clone(), scope).with_vector_store(Arc::new(vector))
}

fn erase_test_intent(workspace_id: &str, user_id: &str, name: &str) -> NodeWriteIntent {
    NodeWriteIntent {
        uid: Uuid::now_v7(),
        label: NodeLabel::Fact,
        workspace_id: Some(workspace_id.to_string()),
        user_id: Some(user_id.to_string()),
        scope: "user".to_string(),
        name: name.to_string(),
        properties: json!({ "name": name, "user_id": user_id, "source": "privacy_erase_test" }),
        pii_class: PiiClass::Phi,
        confidence: Some(0.95),
        valid_from: Utc::now(),
        embedding: Some(basis_vector()),
        embedding_model: Some("test-model".to_string()),
        embedding_model_version: Some(1),
        actor_id: user_id.to_string(),
        actor_kind: "user".to_string(),
    }
}

async fn create_erase_test_node(
    pool: &PgPool,
    workspace_id: &str,
    user_id: &str,
    name: &str,
) -> Uuid {
    let graph = erase_test_graph(pool, workspace_id, user_id);
    let intent = erase_test_intent(workspace_id, user_id, name);
    let uid = intent.uid;
    graph
        .create_node(intent)
        .await
        .expect("create erase fixture");
    uid
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

async fn erase_changelog_count(pool: &PgPool, workspace_id: &str, subject: Uuid) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog \
         WHERE workspace_id = $1 AND op = 'erase' AND target_uid = $2",
    )
    .bind(workspace_id)
    .bind(subject)
    .fetch_one(pool)
    .await
    .expect("count erase changelog rows")
}

async fn seed_pii_vault_subject(pool: &PgPool, workspace_id: &str, subject_user_id: &str) {
    let vault = PiiVault::new_dev(pii_vault_secret());
    let subject_pseudonym = vault
        .subject_pseudonym(subject_user_id)
        .expect("subject pseudonym should compute");
    sqlx::query(
        r#"
        INSERT INTO pii_vault.subject_keys (
            subject_pseudonym, workspace_id, hmac_key_handle
        )
        VALUES ($1, $2, 'test-key')
        "#,
    )
    .bind(subject_pseudonym)
    .bind(workspace_id)
    .execute(pool)
    .await
    .expect("seed PII vault subject");
}

async fn erased_pii_vault_subject_count(
    pool: &PgPool,
    workspace_id: &str,
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
        WHERE workspace_id = $1
          AND subject_pseudonym = $2
          AND erased_at IS NOT NULL
        "#,
    )
    .bind(workspace_id)
    .bind(subject_pseudonym)
    .fetch_one(pool)
    .await
    .expect("count erased PII vault subject")
}

async fn total_erase_changelog_count(pool: &PgPool, workspace_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog WHERE workspace_id = $1 AND op = 'erase'",
    )
    .bind(workspace_id)
    .fetch_one(pool)
    .await
    .expect("count all erase changelog rows")
}

#[test]
fn approval_token_verifies_subject_op_workspace_and_signature() {
    // Pins: server-side approval verifier binds token proof to op, subject, workspace, and signature.
    let subject = Uuid::now_v7();
    let key = signing_key();
    let verifier = ApprovalTokenVerifier {
        verifying_key: key.verifying_key(),
    };
    let claims = valid_claims(subject);
    let token = approval_token(&claims, &key);

    let verified = verifier
        .verify(&token, "export", &subject.to_string(), Some("workspace-a"))
        .expect("verify token");

    assert_eq!(verified.sub, "ops-admin");
    assert_eq!(verified.subject_user_id, subject.to_string());
}

#[test]
fn approval_token_requires_platform_admin_role() {
    // Pins: signed privacy approval tokens must carry platform_admin.
    let subject = Uuid::now_v7();
    let key = signing_key();
    let verifier = ApprovalTokenVerifier {
        verifying_key: key.verifying_key(),
    };
    let mut claims = valid_claims(subject);
    claims.roles.clear();
    let token = approval_token(&claims, &key);

    let error = verifier
        .verify(&token, "export", &subject.to_string(), Some("workspace-a"))
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
    let _guard = PRIVACY_ERASE_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let workspace_id = format!("privacy-erase-dry-{}", Uuid::now_v7().simple());
    let subject = Uuid::now_v7();
    let uid = create_erase_test_node(
        store.pool(),
        &workspace_id,
        &subject.to_string(),
        "dry run fact",
    )
    .await;
    let before_changelog = total_erase_changelog_count(store.pool(), &workspace_id).await;
    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        workspace_id: workspace_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "dry run".to_string(),
        dry_run: true,
        claims: valid_claims_for(subject, &workspace_id, "erase"),
        pii_vault_secret: None,
    };

    let response = execute_privacy_erase(ctx).await.expect("run dry erase");

    assert!(response.dry_run);
    assert_eq!(response.candidate_count, 1);
    assert_eq!(response.erased_count, 0);
    assert_eq!(response.sample[0]["uid"], uid.to_string());
    assert_eq!(node_count(store.pool(), uid).await, 1);
    assert_eq!(
        total_erase_changelog_count(store.pool(), &workspace_id).await,
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
    let _guard = PRIVACY_ERASE_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let workspace_id = format!("privacy-erase-basic-{}", Uuid::now_v7().simple());
    let subject = Uuid::now_v7();
    let uid = create_erase_test_node(
        store.pool(),
        &workspace_id,
        &subject.to_string(),
        "basic erasure fact",
    )
    .await;
    seed_pii_vault_subject(store.pool(), &workspace_id, &subject.to_string()).await;
    assert_eq!(embedding_count(store.pool(), uid).await, 1);
    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        workspace_id: workspace_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "GDPR Art.17 request".to_string(),
        dry_run: false,
        claims: valid_claims_for(subject, &workspace_id, "erase"),
        pii_vault_secret: Some(pii_vault_secret()),
    };

    let response = execute_privacy_erase(ctx).await.expect("run erasure");

    assert_eq!(response.erased_count, 1);
    assert_eq!(response.pii_vault_erased, 1);
    assert_eq!(node_count(store.pool(), uid).await, 0);
    assert_eq!(embedding_count(store.pool(), uid).await, 0);
    assert_eq!(
        erased_pii_vault_subject_count(store.pool(), &workspace_id, &subject.to_string()).await,
        1
    );
    assert_eq!(
        erase_changelog_count(store.pool(), &workspace_id, uid).await,
        1
    );
    assert_eq!(
        erase_changelog_count(store.pool(), &workspace_id, subject).await,
        1
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_idempotent() {
    // Pins: a second erase for an already-purged subject does not create extra graph erasure rows.
    let _guard = PRIVACY_ERASE_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let workspace_id = format!("privacy-erase-idem-{}", Uuid::now_v7().simple());
    let subject = Uuid::now_v7();
    create_erase_test_node(
        store.pool(),
        &workspace_id,
        &subject.to_string(),
        "idempotent erasure fact",
    )
    .await;
    let first = PrivacyEraseContext {
        pool: store.pool().clone(),
        workspace_id: workspace_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "first erase".to_string(),
        dry_run: false,
        claims: valid_claims_for(subject, &workspace_id, "erase"),
        pii_vault_secret: None,
    };
    execute_privacy_erase(first).await.expect("first erasure");
    let after_first = total_erase_changelog_count(store.pool(), &workspace_id).await;
    let second = PrivacyEraseContext {
        pool: store.pool().clone(),
        workspace_id: workspace_id.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "second erase".to_string(),
        dry_run: false,
        claims: valid_claims_for(subject, &workspace_id, "erase"),
        pii_vault_secret: None,
    };

    let response = execute_privacy_erase(second).await.expect("second erasure");

    assert_eq!(response.candidate_count, 0);
    assert_eq!(response.erased_count, 0);
    assert_eq!(
        total_erase_changelog_count(store.pool(), &workspace_id).await,
        after_first
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_cross_workspace_is_noop_for_graph_data() {
    // Pins: erase candidate enumeration stays scoped to the requested workspace.
    let _guard = PRIVACY_ERASE_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let workspace_a = format!("privacy-erase-a-{}", Uuid::now_v7().simple());
    let workspace_b = format!("privacy-erase-b-{}", Uuid::now_v7().simple());
    let subject = Uuid::now_v7();
    let uid_b = create_erase_test_node(
        store.pool(),
        &workspace_b,
        &subject.to_string(),
        "other workspace fact",
    )
    .await;
    let ctx = PrivacyEraseContext {
        pool: store.pool().clone(),
        workspace_id: workspace_a.clone(),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "wrong workspace erase".to_string(),
        dry_run: false,
        claims: valid_claims_for(subject, &workspace_a, "erase"),
        pii_vault_secret: None,
    };

    let response = execute_privacy_erase(ctx)
        .await
        .expect("wrong workspace erasure is idempotent");

    assert_eq!(response.erased_count, 0);
    assert_eq!(node_count(store.pool(), uid_b).await, 1);
    assert_eq!(
        total_erase_changelog_count(store.pool(), &workspace_a).await,
        0
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn privacy_erase_unknown_op_rejected() {
    // Pins: privacy erasure does not add unsupported crypto-shredding op variants.
    let _guard = PRIVACY_ERASE_TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated test store");
    let workspace_id = format!("privacy-erase-check-{}", Uuid::now_v7().simple());
    let mut tx = begin_app_scoped_tx(store.pool(), &workspace_id, &Uuid::now_v7().to_string())
        .await
        .expect("begin app tx");

    let error = sqlx::query(
        r#"
        INSERT INTO moa.graph_changelog
            (workspace_id, actor_id, actor_kind, op, target_kind, target_label,
             target_uid, payload, pii_class)
        VALUES ($1, 'ops-admin', 'admin', 'deferred_encryption', 'user', 'User',
                $2, '{}'::jsonb, 'phi')
        "#,
    )
    .bind(&workspace_id)
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
    tokio::fs::write(export_dir.join("skill_addenda.jsonl"), "")
        .await
        .expect("write addenda");
    tokio::fs::write(export_dir.join("changelog.jsonl"), "")
        .await
        .expect("write changelog");

    let claims = valid_claims(subject);
    let ctx = PrivacyExportContext {
        pool: PgPool::connect_lazy("postgres://unused").expect("lazy pool"),
        workspace: Some("workspace-a".to_string()),
        subject_user: subject,
        subject_user_id: subject.to_string(),
        reason: "GDPR Art.15 request".to_string(),
        claims,
    };
    let counts = BTreeMap::from([
        ("facts", 1),
        ("entities", 0),
        ("relationships", 0),
        ("embeddings", 0),
        ("skills", 0),
        ("skill_addenda", 0),
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
