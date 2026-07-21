//! Database coverage for the resumable sealed-content memory backfill.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use moa_crypto::{
    Ciphertext, DataKeyDecryptRequest, EncryptionContext, Error as CryptoError, GeneratedDataKey,
    KeyHandle, KeyManagementProvider, LocalKmsProvider, PlaintextDek,
};
use moa_memory_graph::Error;
use moa_memory_graph::backfill::backfill_memory_sealed_content;
use moa_session::testing;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tokio::sync::Barrier;
use uuid::Uuid;

const SUBJECT_CONSTRAINTS: &[&str] = &[
    "node_index_data_subject_required",
    "node_index_data_subject_scope",
    "node_index_sealed_content_state",
];

#[derive(Clone)]
struct HistoricalNode {
    uid: Uuid,
    tenant_id: Uuid,
    contact_id: Option<Uuid>,
    pii_class: &'static str,
    name: String,
    properties: Value,
    has_embedding: bool,
}

impl HistoricalNode {
    fn restricted(uid: u128, tenant_id: Uuid, contact_id: Option<Uuid>) -> Self {
        let name = format!("historical secret {uid}");
        Self {
            uid: Uuid::from_u128(uid),
            tenant_id,
            contact_id,
            pii_class: if contact_id.is_some() {
                "phi"
            } else {
                "restricted"
            },
            properties: json!({
                "summary": name,
                "base_confidence": 0.73,
                "secret": format!("secret-{uid}"),
            }),
            name,
            has_embedding: true,
        }
    }

    fn unsealed(uid: u128, tenant_id: Uuid) -> Self {
        let name = format!("historical public {uid}");
        Self {
            uid: Uuid::from_u128(uid),
            tenant_id,
            contact_id: None,
            pii_class: "none",
            properties: json!({
                "summary": name,
                "base_confidence": 0.61,
            }),
            name,
            has_embedding: false,
        }
    }
}

struct FailOnSecondGenerate {
    inner: Arc<LocalKmsProvider>,
    calls: AtomicUsize,
}

struct SynchronizeFirstFourGenerates {
    inner: LocalKmsProvider,
    calls: AtomicUsize,
    first_claims: Barrier,
}

#[async_trait]
impl KeyManagementProvider for FailOnSecondGenerate {
    async fn generate_data_keys(
        &self,
        contexts: &[EncryptionContext],
    ) -> Result<Vec<GeneratedDataKey>, CryptoError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
            return Err(CryptoError::Backend(
                "injected worker restart after first committed batch".to_string(),
            ));
        }
        self.inner.generate_data_keys(contexts).await
    }

    async fn decrypt_data_keys(
        &self,
        requests: &[DataKeyDecryptRequest],
    ) -> Result<Vec<PlaintextDek>, CryptoError> {
        self.inner.decrypt_data_keys(requests).await
    }

    async fn destroy_key(&self, handle: &KeyHandle) -> Result<(), CryptoError> {
        self.inner.destroy_key(handle).await
    }

    async fn destroy_subject_key(
        &self,
        tenant_id: Uuid,
        subject_id: Uuid,
    ) -> Result<(), CryptoError> {
        self.inner.destroy_subject_key(tenant_id, subject_id).await
    }
}

#[async_trait]
impl KeyManagementProvider for SynchronizeFirstFourGenerates {
    async fn generate_data_keys(
        &self,
        contexts: &[EncryptionContext],
    ) -> Result<Vec<GeneratedDataKey>, CryptoError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) < 4 {
            self.first_claims.wait().await;
        }
        self.inner.generate_data_keys(contexts).await
    }

    async fn decrypt_data_keys(
        &self,
        requests: &[DataKeyDecryptRequest],
    ) -> Result<Vec<PlaintextDek>, CryptoError> {
        self.inner.decrypt_data_keys(requests).await
    }

    async fn destroy_key(&self, handle: &KeyHandle) -> Result<(), CryptoError> {
        self.inner.destroy_key(handle).await
    }

    async fn destroy_subject_key(
        &self,
        tenant_id: Uuid,
        subject_id: Uuid,
    ) -> Result<(), CryptoError> {
        self.inner.destroy_subject_key(tenant_id, subject_id).await
    }
}

async fn seed_historical_nodes(pool: &PgPool, nodes: &[HistoricalNode]) {
    let mut tx = pool.begin().await.expect("begin historical seed");
    for constraint in SUBJECT_CONSTRAINTS {
        sqlx::query(&format!(
            "ALTER TABLE moa.node_index DROP CONSTRAINT {constraint}"
        ))
        .execute(tx.as_mut())
        .await
        .expect("drop deferred node constraint for historical fixture");
    }
    sqlx::query("ALTER TABLE moa.embeddings DROP CONSTRAINT embeddings_unsealed_content_only")
        .execute(tx.as_mut())
        .await
        .expect("drop deferred embedding constraint for historical fixture");
    sqlx::query("ALTER TABLE moa.embeddings DISABLE TRIGGER embeddings_reject_sealed_node")
        .execute(tx.as_mut())
        .await
        .expect("disable sealed embedding trigger for historical fixture");

    let vector = format!("[{}]", vec!["0"; 1024].join(","));
    for node in nodes {
        sqlx::query(
            r#"
            INSERT INTO moa.node_index (
                uid, label, storage_partition_id, user_id, tenant_id, contact_id, name, pii_class,
                confidence, properties_summary, content_sealed, data_subject_id
            ) VALUES ($1, 'Fact', $2, $3, $4, $5, $6, $7, 0.9, $8, NULL, NULL)
            "#,
        )
        .bind(node.uid)
        .bind(node.tenant_id.to_string())
        .bind(node.contact_id.map(|contact_id| contact_id.to_string()))
        .bind(node.tenant_id)
        .bind(node.contact_id)
        .bind(&node.name)
        .bind(node.pii_class)
        .bind(&node.properties)
        .execute(tx.as_mut())
        .await
        .expect("insert historical node");

        if node.has_embedding {
            sqlx::query(
                r#"
                INSERT INTO moa.embeddings (
                    uid, storage_partition_id, user_id, label, pii_class, embedding,
                    embedding_model, embedding_model_version
                ) VALUES ($1, $2, $3, 'Fact', $4, $5::public.halfvec, 'historical', 1)
                "#,
            )
            .bind(node.uid)
            .bind(node.tenant_id.to_string())
            .bind(node.contact_id.map(|contact_id| contact_id.to_string()))
            .bind(node.pii_class)
            .bind(&vector)
            .execute(tx.as_mut())
            .await
            .expect("insert historical embedding");
        }
    }

    for constraint in SUBJECT_CONSTRAINTS {
        let definition = match *constraint {
            "node_index_data_subject_required" => "CHECK (data_subject_id IS NOT NULL)",
            "node_index_data_subject_scope" => {
                "CHECK (data_subject_id = CASE WHEN contact_id IS NOT NULL THEN contact_id ELSE tenant_id END)"
            }
            "node_index_sealed_content_state" => {
                "CHECK (((pii_class IN ('phi', 'restricted') AND data_subject_id IS NOT NULL AND name = '[RESTRICTED]' AND properties_summary = '{\"redacted\": true}'::jsonb AND content_sealed IS NOT NULL AND octet_length(content_sealed) > 0) OR (pii_class NOT IN ('phi', 'restricted') AND content_sealed IS NULL)))"
            }
            _ => unreachable!("known node constraint"),
        };
        sqlx::query(&format!(
            "ALTER TABLE moa.node_index ADD CONSTRAINT {constraint} {definition} NOT VALID"
        ))
        .execute(tx.as_mut())
        .await
        .expect("restore deferred node constraint");
    }
    sqlx::query(
        "ALTER TABLE moa.embeddings ADD CONSTRAINT embeddings_unsealed_content_only CHECK (pii_class NOT IN ('phi', 'restricted')) NOT VALID",
    )
    .execute(tx.as_mut())
    .await
    .expect("restore deferred embedding constraint");
    sqlx::query("ALTER TABLE moa.embeddings ENABLE TRIGGER embeddings_reject_sealed_node")
        .execute(tx.as_mut())
        .await
        .expect("restore sealed embedding trigger");
    tx.commit().await.expect("commit historical seed");
}

async fn assert_constraints_validated(pool: &PgPool) {
    let rows: Vec<(String, bool)> = sqlx::query_as(
        r#"
        SELECT constraint_row.conname, constraint_row.convalidated
          FROM pg_catalog.pg_constraint AS constraint_row
          JOIN pg_catalog.pg_class AS table_row ON table_row.oid = constraint_row.conrelid
          JOIN pg_catalog.pg_namespace AS schema_row ON schema_row.oid = table_row.relnamespace
         WHERE schema_row.nspname = 'moa'
           AND table_row.relname IN ('node_index', 'embeddings')
           AND constraint_row.conname = ANY($1)
         ORDER BY constraint_row.conname
        "#,
    )
    .bind([
        "embeddings_unsealed_content_only",
        "node_index_data_subject_required",
        "node_index_data_subject_scope",
        "node_index_sealed_content_state",
    ])
    .fetch_all(pool)
    .await
    .expect("read backfill constraint validation state");
    assert_eq!(
        rows,
        vec![
            ("embeddings_unsealed_content_only".to_string(), true),
            ("node_index_data_subject_required".to_string(), true),
            ("node_index_data_subject_scope".to_string(), true),
            ("node_index_sealed_content_state".to_string(), true),
        ]
    );
}

#[tokio::test]
async fn backfill_resumes_after_restart_and_enforces_sealed_state_db_memory() {
    // Pins: a failed worker leaves its earlier bounded transaction committed; a
    // fresh invocation resumes without rewriting it and finishes every subject,
    // ciphertext, vector-delete, sidecar, and deferred-constraint invariant.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated backfill database");
    let tenant_id = Uuid::now_v7();
    let contact_id = Uuid::now_v7();
    let nodes = vec![
        HistoricalNode::restricted(1, tenant_id, None),
        HistoricalNode::restricted(2, tenant_id, Some(contact_id)),
        HistoricalNode::unsealed(3, tenant_id),
    ];
    seed_historical_nodes(store.pool(), &nodes).await;

    let kms = Arc::new(LocalKmsProvider::new());
    let failing_kms = FailOnSecondGenerate {
        inner: kms.clone(),
        calls: AtomicUsize::new(0),
    };
    let interrupted = backfill_memory_sealed_content(store.pool(), &failing_kms, 1)
        .await
        .expect_err("second encryption batch should simulate a worker restart");
    assert!(matches!(
        interrupted,
        Error::Crypto(CryptoError::Backend(message))
            if message == "injected worker restart after first committed batch"
    ));

    let interrupted_state: (i64, i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT count(*) FILTER (WHERE data_subject_id IS NOT NULL),
               count(*) FILTER (WHERE content_sealed IS NOT NULL),
               (SELECT count(*) FROM moa.embeddings),
               (SELECT count(*) FROM moa.vector_sync_outbox)
          FROM moa.node_index
        "#,
    )
    .fetch_one(store.pool())
    .await
    .expect("read committed state after interruption");
    assert_eq!(interrupted_state, (1, 1, 1, 1));

    let resumed = backfill_memory_sealed_content(store.pool(), kms.as_ref(), 1)
        .await
        .expect("fresh worker resumes historical backfill");
    assert_eq!(resumed.rows_claimed, 2);
    assert_eq!(resumed.rows_sealed, 1);
    assert_eq!(resumed.subjects_set, 2);
    assert_eq!(resumed.embeddings_deleted, 1);
    assert_eq!(resumed.batches_committed, 2);

    let rows = sqlx::query(
        "SELECT uid, data_subject_id, name, properties_summary, content_sealed, base_confidence FROM moa.node_index ORDER BY uid",
    )
    .fetch_all(store.pool())
    .await
    .expect("read converted nodes");
    assert_eq!(rows.len(), 3);
    for row in &rows {
        let uid: Uuid = row.try_get("uid").expect("decode node uid");
        let historical = nodes
            .iter()
            .find(|node| node.uid == uid)
            .expect("backfilled uid belongs to fixture");
        let expected_subject = historical.contact_id.unwrap_or(tenant_id);
        assert_eq!(
            row.try_get::<Uuid, _>("data_subject_id")
                .expect("decode data subject"),
            expected_subject
        );
        assert_eq!(
            row.try_get::<Option<f64>, _>("base_confidence")
                .expect("decode base confidence"),
            Some(if historical.pii_class == "none" {
                0.61
            } else {
                0.73
            })
        );
        let properties: Value = row
            .try_get("properties_summary")
            .expect("decode properties");
        assert!(properties.get("base_confidence").is_none());
        if historical.pii_class != "none" {
            assert_eq!(
                row.try_get::<String, _>("name").expect("decode name"),
                "[RESTRICTED]"
            );
            assert_eq!(properties, json!({ "redacted": true }));
            assert!(
                row.try_get::<Option<Vec<u8>>, _>("content_sealed")
                    .expect("decode sealed content")
                    .is_some()
            );
        } else {
            assert_eq!(
                row.try_get::<Option<Vec<u8>>, _>("content_sealed")
                    .expect("decode unsealed content"),
                None
            );
        }
    }

    let tenant_restricted_row = rows
        .iter()
        .find(|row| row.try_get::<Uuid, _>("uid").ok() == Some(nodes[0].uid))
        .expect("tenant restricted fixture row");
    let sealed: Vec<u8> = tenant_restricted_row
        .try_get("content_sealed")
        .expect("decode first sealed content");
    let ciphertext = Ciphertext::from_bytes(&sealed).expect("parse single ciphertext payload");
    let plaintext = moa_crypto::decrypt(
        kms.as_ref(),
        &ciphertext,
        &EncryptionContext::new(tenant_id, tenant_id, nodes[0].uid.to_string(), "restricted"),
    )
    .await
    .expect("decrypt backfilled payload");
    let payload: Value = serde_json::from_slice(&plaintext).expect("parse sealed content document");
    assert_eq!(
        payload,
        json!({
            "version": 1,
            "name": nodes[0].name,
            "properties": {
                "summary": nodes[0].name,
                "secret": "secret-1",
            }
        })
    );

    let sealed_columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns WHERE table_schema = 'moa' AND table_name = 'node_index' AND column_name LIKE '%sealed' ORDER BY column_name",
    )
    .fetch_all(store.pool())
    .await
    .expect("inspect sealed storage columns");
    assert_eq!(sealed_columns, vec!["content_sealed".to_string()]);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.embeddings")
            .fetch_one(store.pool())
            .await
            .expect("count remaining embeddings"),
        0
    );
    let outbox: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT uid, op FROM moa.vector_sync_outbox ORDER BY uid")
            .fetch_all(store.pool())
            .await
            .expect("read external vector delete outbox");
    assert_eq!(
        outbox,
        vec![
            (nodes[0].uid, "delete".to_string()),
            (nodes[1].uid, "delete".to_string()),
        ]
    );
    assert_constraints_validated(store.pool()).await;

    let restarted = backfill_memory_sealed_content(store.pool(), kms.as_ref(), 1)
        .await
        .expect("completed backfill is restart-idempotent");
    assert_eq!(restarted, Default::default());

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated backfill database");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backfill_independent_pools_cooperate_and_serialize_finalizers_db_memory() {
    // Pins: Kubernetes-style workers with independent pools claim disjoint
    // SKIP-LOCKED batches, while concurrent finalizers serialize and all return
    // only after one exact conversion of every historical row.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated concurrent backfill database");
    let tenant_id = Uuid::now_v7();
    let mut nodes = (100..124)
        .map(|uid| HistoricalNode::restricted(uid, tenant_id, None))
        .collect::<Vec<_>>();
    nodes.extend((124..132).map(|uid| HistoricalNode::unsealed(uid, tenant_id)));
    seed_historical_nodes(store.pool(), &nodes).await;

    let kms = Arc::new(SynchronizeFirstFourGenerates {
        inner: LocalKmsProvider::new(),
        calls: AtomicUsize::new(0),
        first_claims: Barrier::new(4),
    });
    let barrier = Arc::new(Barrier::new(4));
    let mut workers = Vec::new();
    for _ in 0..4 {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&database_url)
            .await
            .expect("connect independent worker pool");
        let kms = kms.clone();
        let barrier = barrier.clone();
        workers.push(tokio::spawn(async move {
            barrier.wait().await;
            let report = backfill_memory_sealed_content(&pool, kms.as_ref(), 1).await;
            pool.close().await;
            report
        }));
    }

    let mut totals = (0_u64, 0_u64, 0_u64, 0_u64, 0_u64);
    let mut reports = Vec::new();
    for worker in workers {
        let report = worker
            .await
            .expect("concurrent backfill worker joins")
            .expect("concurrent backfill worker succeeds");
        totals.0 += report.rows_claimed;
        totals.1 += report.rows_sealed;
        totals.2 += report.subjects_set;
        totals.3 += report.embeddings_deleted;
        totals.4 += report.batches_committed;
        reports.push(report);
    }
    assert_eq!(reports.len(), 4);
    assert!(
        reports.iter().all(|report| report.batches_committed > 0),
        "all four independent workers must claim at least one batch: {reports:?}"
    );
    assert_eq!(totals, (32, 24, 32, 24, 32));
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM moa.node_index WHERE data_subject_id = tenant_id",
        )
        .fetch_one(store.pool())
        .await
        .expect("count converted subjects"),
        32
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.vector_sync_outbox")
            .fetch_one(store.pool())
            .await
            .expect("count external vector deletes"),
        24
    );
    assert_constraints_validated(store.pool()).await;

    let finalizer_barrier = Arc::new(Barrier::new(4));
    let mut finalizers = Vec::new();
    for _ in 0..4 {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("connect independent finalizer pool");
        let kms = kms.clone();
        let barrier = finalizer_barrier.clone();
        finalizers.push(tokio::spawn(async move {
            barrier.wait().await;
            let report = backfill_memory_sealed_content(&pool, kms.as_ref(), 1).await;
            pool.close().await;
            report
        }));
    }
    for finalizer in finalizers {
        assert_eq!(
            finalizer
                .await
                .expect("simultaneous finalizer joins")
                .expect("simultaneous finalizer succeeds"),
            Default::default()
        );
    }

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated concurrent backfill database");
}
