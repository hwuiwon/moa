//! Integration coverage for envelope-encrypted restricted/PHI graph content.
//!
//! Exercises the encrypted storage model end to end against Postgres: restricted and
//! PHI node content is sealed with moa-crypto, the indexed plaintext columns hold
//! only a redaction placeholder (so the secret never reaches the `name_tsv` /
//! `properties_tsv` full-text indexes or the vector index), reads decrypt at the
//! retrieval boundary, and a crypto-shredded data subject fails closed while
//! other subjects keep decrypting.

use std::sync::Arc;

use moa_core::types::contact::ContactId;
use moa_core::types::identifiers::{StoragePartitionId, TenantId};
use moa_core::types::memory::RlsContext;
use moa_core::types::security::SensitivityClass;
use moa_crypto::LocalKmsProvider;
use moa_db::ScopedConn;
use moa_memory_graph::{
    GraphStore, NodeContentUpdateIntent, NodeLabel, NodeWriteIntent, PostgresGraphStore,
};
use moa_memory_vector::PgvectorStore;
use moa_session::testing;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn tenant_scope(tenant_id: TenantId) -> RlsContext {
    RlsContext::tenant(tenant_id)
}

fn basis_vector(index: usize) -> Vec<f32> {
    let mut vector = vec![0.0; 1024];
    vector[index % 1024] = 1.0;
    vector
}

/// A tenant-scoped graph store sharing `kms`, with a real pgvector backend so the
/// vector-index-skip for restricted rows can be observed.
fn tenant_graph_store(
    pool: &PgPool,
    tenant_id: TenantId,
    kms: Arc<LocalKmsProvider>,
) -> PostgresGraphStore {
    let scope = tenant_scope(tenant_id);
    let vector = PgvectorStore::new_for_app_role(pool.clone(), scope.clone());
    PostgresGraphStore::scoped_for_app_role(pool.clone(), scope, kms)
        .with_vector_store(Arc::new(vector))
}

fn tenant_node_intent(
    tenant_id: TenantId,
    label: NodeLabel,
    name: &str,
    properties: Value,
    pii_class: SensitivityClass,
    embedding: Option<Vec<f32>>,
) -> NodeWriteIntent {
    NodeWriteIntent {
        barrier: None,
        uid: Uuid::now_v7(),
        data_subject_id: tenant_id.0,
        label,
        storage_partition_id: Some(StoragePartitionId::for_tenant(tenant_id).to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        name: name.to_string(),
        properties,
        pii_class,
        confidence: Some(0.9),
        valid_from: moa_test_support::fixtures::pg_now(),
        embedding,
        embedding_model: Some("test-model".to_string()),
        embedding_model_version: Some(1),
        embedding_text: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

/// A contact-scoped intent used for the per-subject crypto-shred coverage.
fn contact_scoped_intent(
    tenant_id: TenantId,
    contact_id: ContactId,
    name: &str,
    properties: Value,
    pii_class: SensitivityClass,
) -> NodeWriteIntent {
    let mut intent = tenant_node_intent(
        tenant_id,
        NodeLabel::Fact,
        name,
        properties,
        pii_class,
        None,
    );
    intent.data_subject_id = contact_id.0;
    intent.contact_id = Some(contact_id.to_string());
    intent.scope = "contact".to_string();
    intent
}

/// Raw (undecrypted) storage of one node, read directly from `moa.node_index`.
struct RawStoredNode {
    name: String,
    properties_summary: Option<Value>,
    content_sealed: Option<Vec<u8>>,
}

async fn read_raw_tenant_node(pool: &PgPool, tenant_id: TenantId, uid: Uuid) -> RawStoredNode {
    let mut conn = ScopedConn::begin_as_app(pool, &tenant_scope(tenant_id), true)
        .await
        .expect("begin raw read");
    let row = sqlx::query(
        "SELECT name, properties_summary, content_sealed \
         FROM moa.node_index WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read raw node_index row");
    conn.commit().await.expect("commit raw read");
    RawStoredNode {
        name: row.try_get("name").expect("decode name"),
        properties_summary: row
            .try_get("properties_summary")
            .expect("decode properties"),
        content_sealed: row
            .try_get("content_sealed")
            .expect("decode content_sealed"),
    }
}

/// Whether the `name_tsv` full-text index matches `term` for `uid` — the exact
/// index path a lexical seed lookup would take, so a positive match proves the
/// secret is searchable.
async fn name_tsv_matches(pool: &PgPool, tenant_id: TenantId, uid: Uuid, term: &str) -> bool {
    let mut conn = ScopedConn::begin_as_app(pool, &tenant_scope(tenant_id), true)
        .await
        .expect("begin tsv read");
    let matched = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS ( \
             SELECT 1 FROM moa.node_index \
             WHERE uid = $1 AND name_tsv @@ plainto_tsquery('simple', $2) \
         )",
    )
    .bind(uid)
    .bind(term)
    .fetch_one(conn.as_mut())
    .await
    .expect("evaluate name_tsv match");
    conn.commit().await.expect("commit tsv read");
    matched
}

async fn vector_row_count(pool: &PgPool, tenant_id: TenantId, uid: Uuid) -> i64 {
    let mut conn = ScopedConn::begin_as_app(pool, &tenant_scope(tenant_id), true)
        .await
        .expect("begin vector read");
    let count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.embeddings WHERE uid = $1")
        .bind(uid)
        .fetch_one(conn.as_mut())
        .await
        .expect("count vector rows");
    conn.commit().await.expect("commit vector read");
    count
}

async fn assert_database_rejects_spoofed_unsealed_embedding(
    pool: &PgPool,
    tenant_id: TenantId,
    uid: Uuid,
) {
    let mut conn = ScopedConn::begin_as_app(pool, &tenant_scope(tenant_id), true)
        .await
        .expect("begin direct embedding write");
    let vector = format!("[{}]", vec!["0"; 1024].join(","));
    let error = sqlx::query(
        r#"
        INSERT INTO moa.embeddings (
            uid, storage_partition_id, label, pii_class, embedding,
            embedding_model, embedding_model_version
        ) VALUES ($1, $2, 'Fact', 'none', $3::public.halfvec, 'spoofed', 1)
        "#,
    )
    .bind(uid)
    .bind(StoragePartitionId::for_tenant(tenant_id).to_string())
    .bind(vector)
    .execute(conn.as_mut())
    .await
    .expect_err("database must reject an embedding falsely classified as unsealed");
    assert_eq!(
        error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );
    conn.rollback()
        .await
        .expect("rollback rejected embedding write");
}

/// Pins the partition's embedder to the test model/dimension so the `none`
/// node's embedding is accepted (the default configured model differs).
async fn seed_embedder_state(pool: &PgPool, tenant_id: TenantId) {
    let partition = StoragePartitionId::for_tenant(tenant_id).to_string();
    let mut conn = ScopedConn::begin_as_app(pool, &tenant_scope(tenant_id), true)
        .await
        .expect("begin embedder seed");
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, 'test-model', 1, 1024)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension
        "#,
    )
    .bind(&partition)
    .execute(conn.as_mut())
    .await
    .expect("seed embedder state");
    conn.commit().await.expect("commit embedder seed");
}

#[tokio::test]
async fn restricted_node_content_is_sealed_at_rest_and_decrypted_on_read_db_memory() {
    // Pins: a restricted node round-trips its real name/properties through
    // create + read, while at rest the indexed plaintext columns hold only the
    // redaction placeholder (secret absent from name/properties and the name_tsv
    // full-text index) and the sealed content payload is populated. Its
    // embeddings are rejected. A `none` node in the same
    // store round-trips byte-identically and keeps its vector row. Reverting the
    // write-path sealing makes the plaintext-column and tsv assertions fail.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let kms = Arc::new(LocalKmsProvider::new());
    let graph = tenant_graph_store(pool, tenant_id, kms.clone());
    seed_embedder_state(pool, tenant_id).await;

    let secret_name = "Jane Roe SSN 000-11-2222";
    let secret_props = json!({
        "name": secret_name,
        "account_number": "ACCT-9182736455",
        "balance_usd": 42_000
    });
    let rejected = tenant_node_intent(
        tenant_id,
        NodeLabel::Fact,
        secret_name,
        secret_props.clone(),
        SensitivityClass::Restricted,
        Some(basis_vector(3)),
    );
    let error = graph
        .create_node(rejected)
        .await
        .expect_err("sealed embedding must be rejected");
    assert!(matches!(error, moa_memory_graph::Error::SealedEmbedding));
    let restricted = tenant_node_intent(
        tenant_id,
        NodeLabel::Fact,
        secret_name,
        secret_props.clone(),
        SensitivityClass::Restricted,
        None,
    );
    let restricted_uid = restricted.uid;
    graph
        .create_node(restricted)
        .await
        .expect("create restricted node");
    assert_database_rejects_spoofed_unsealed_embedding(pool, tenant_id, restricted_uid).await;

    let plain_name = "public quarterly summary";
    let plain_props = json!({ "name": plain_name, "topic": "earnings" });
    let plain = tenant_node_intent(
        tenant_id,
        NodeLabel::Fact,
        plain_name,
        plain_props.clone(),
        SensitivityClass::None,
        Some(basis_vector(4)),
    );
    let plain_uid = plain.uid;
    graph.create_node(plain).await.expect("create none node");

    // Read boundary decrypts restricted content back to the original.
    let read_restricted = graph
        .get_node(restricted_uid)
        .await
        .expect("get restricted node")
        .expect("restricted node present");
    assert_eq!(read_restricted.name, secret_name);
    assert_eq!(read_restricted.properties_summary, Some(secret_props));
    assert_eq!(read_restricted.pii_class, SensitivityClass::Restricted);

    let replacement_name = "Jane Roe restricted account";
    let replacement_props = json!({ "account_number": "ACCT-0000000001" });
    graph
        .update_node_content(NodeContentUpdateIntent {
            uid: restricted_uid,
            name: replacement_name.to_string(),
            properties: replacement_props.clone(),
            confidence: Some(0.91),
            actor_id: Uuid::now_v7().to_string(),
            actor_kind: "system".to_string(),
        })
        .await
        .expect("replace complete sealed content");
    let updated = graph
        .get_node(restricted_uid)
        .await
        .expect("get updated restricted node")
        .expect("updated restricted node present");
    assert_eq!(updated.name, replacement_name);
    assert_eq!(updated.properties_summary, Some(replacement_props));

    // At rest, the indexed plaintext columns carry only the placeholder and the
    // sealed content column carries the ciphertext.
    let raw = read_raw_tenant_node(pool, tenant_id, restricted_uid).await;
    assert_eq!(raw.name, "[RESTRICTED]");
    assert_eq!(raw.properties_summary, Some(json!({ "redacted": true })));
    assert!(
        raw.content_sealed.is_some(),
        "content ciphertext must be stored"
    );
    // The secret must not appear anywhere in the indexed plaintext.
    let plaintext_blob = format!(
        "{}{}",
        raw.name,
        raw.properties_summary
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default()
    );
    assert!(
        !plaintext_blob.contains("000-11-2222") && !plaintext_blob.contains("ACCT-9182736455"),
        "secret leaked into indexed plaintext: {plaintext_blob}"
    );
    // ...and it must not be full-text searchable, while the placeholder is.
    assert!(
        !name_tsv_matches(pool, tenant_id, restricted_uid, "Jane Roe").await,
        "restricted name must not be full-text searchable"
    );
    assert!(
        name_tsv_matches(pool, tenant_id, restricted_uid, "RESTRICTED").await,
        "placeholder should be what the tsv index holds"
    );
    // Restricted content is excluded from the vector index; the none node is not.
    assert_eq!(
        vector_row_count(pool, tenant_id, restricted_uid).await,
        0,
        "restricted embedding must be withheld from the vector index"
    );

    // The `none` node is completely unaffected: real content in the plaintext
    // columns, no ciphertext, and a live vector row.
    let read_plain = graph
        .get_node(plain_uid)
        .await
        .expect("get none node")
        .expect("none node present");
    assert_eq!(read_plain.name, plain_name);
    assert_eq!(read_plain.properties_summary, Some(plain_props));
    let raw_plain = read_raw_tenant_node(pool, tenant_id, plain_uid).await;
    assert_eq!(raw_plain.name, plain_name);
    assert!(raw_plain.content_sealed.is_none());
    assert_eq!(
        vector_row_count(pool, tenant_id, plain_uid).await,
        1,
        "none node keeps its vector row"
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn crypto_shred_subject_fails_closed_only_for_that_subject_db_memory() {
    // Pins: crypto-shredding one data subject's KEK makes that subject's sealed
    // node fail closed (never return plaintext or a fallback) on read, while another
    // subject in the same tenant still decrypts its own restricted content. This
    // is the erasure-isolation property the privacy erase path relies on.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_a = ContactId(Uuid::now_v7());
    let contact_b = ContactId(Uuid::now_v7());
    let kms = Arc::new(LocalKmsProvider::new());

    let graph_a = PostgresGraphStore::scoped_for_app_role(
        pool.clone(),
        RlsContext::contact(tenant_id, contact_a),
        kms.clone(),
    );
    let graph_b = PostgresGraphStore::scoped_for_app_role(
        pool.clone(),
        RlsContext::contact(tenant_id, contact_b),
        kms.clone(),
    );

    let secret_a = "contact A restricted: card 4111-1111-1111-1111";
    let node_a = contact_scoped_intent(
        tenant_id,
        contact_a,
        secret_a,
        json!({ "name": secret_a, "cvv": "123" }),
        SensitivityClass::Phi,
    );
    let uid_a = node_a.uid;
    graph_a
        .create_node(node_a)
        .await
        .expect("create contact A restricted node");

    let secret_b = "contact B restricted: card 4222-2222-2222-2222";
    let node_b = contact_scoped_intent(
        tenant_id,
        contact_b,
        secret_b,
        json!({ "name": secret_b, "cvv": "456" }),
        SensitivityClass::Phi,
    );
    let uid_b = node_b.uid;
    graph_b
        .create_node(node_b)
        .await
        .expect("create contact B restricted node");

    // Both decrypt before erasure.
    assert_eq!(
        graph_a
            .get_node(uid_a)
            .await
            .expect("get A")
            .expect("A present")
            .name,
        secret_a
    );
    assert_eq!(
        graph_b
            .get_node(uid_b)
            .await
            .expect("get B")
            .expect("B present")
            .name,
        secret_b
    );

    // Crypto-shred contact A's per-subject KEK. Subject id is the contact UUID,
    // exactly as the write path sealed it.
    moa_crypto::crypto_shred_subject(kms.as_ref(), tenant_id.0, contact_a.0)
        .await
        .expect("crypto-shred contact A");

    // A now fails closed; the ciphertext row is still present but unrecoverable.
    let error = graph_a
        .get_node(uid_a)
        .await
        .expect_err("shredded subject read must fail closed");
    assert!(error.to_string().contains("crypto-shredded"));

    // B, a different subject in the same tenant, still decrypts.
    let read_b = graph_b
        .get_node(uid_b)
        .await
        .expect("get B after shred")
        .expect("B present");
    assert_eq!(read_b.name, secret_b);

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
