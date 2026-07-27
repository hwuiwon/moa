//! Integration coverage for the merged contradiction candidate retrieval query.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::types::security::SensitivityClass;
use moa_core::{types::identifiers::TenantId, types::memory::RlsContext};
use moa_crypto::LocalKmsProvider;
use moa_memory_graph::{GraphStore, NodeLabel, NodeWriteIntent, PostgresGraphStore};
use moa_memory_ingest::{ContradictionContext, RrfPlusJudgeDetector};
use moa_memory_vector::{VECTOR_DIMENSION, VectorItem, VectorMatch, VectorQuery, VectorStore};
use moa_session::testing;
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

/// Vector store whose KNN always returns one caller-chosen uid, so a test can
/// drive the vector branch of candidate retrieval without pgvector rows.
struct StubVectorStore {
    hit: Uuid,
}

#[async_trait]
impl VectorStore for StubVectorStore {
    fn backend(&self) -> &'static str {
        "stub-contradiction"
    }

    fn dimension(&self) -> usize {
        VECTOR_DIMENSION
    }

    async fn upsert(&self, _items: &[VectorItem]) -> Result<(), moa_memory_vector::Error> {
        Ok(())
    }

    async fn upsert_in_tx(
        &self,
        _conn: &mut sqlx::PgConnection,
        _items: &[VectorItem],
    ) -> Result<(), moa_memory_vector::Error> {
        Ok(())
    }

    async fn knn(
        &self,
        _query: &VectorQuery,
    ) -> Result<Vec<VectorMatch>, moa_memory_vector::Error> {
        Ok(vec![VectorMatch {
            uid: self.hit,
            score: 0.99,
        }])
    }

    async fn delete(&self, _uids: &[Uuid]) -> Result<(), moa_memory_vector::Error> {
        Ok(())
    }

    async fn delete_in_tx(
        &self,
        _conn: &mut sqlx::PgConnection,
        _uids: &[Uuid],
    ) -> Result<(), moa_memory_vector::Error> {
        Ok(())
    }
}

fn fact_intent(storage_partition_id: &str, uid: Uuid, name: &str) -> NodeWriteIntent {
    NodeWriteIntent {
        barrier: None,
        uid,
        data_subject_id: Uuid::parse_str(storage_partition_id)
            .expect("storage partition fixture should be a tenant UUID"),
        label: NodeLabel::Fact,
        storage_partition_id: Some(storage_partition_id.to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        name: name.to_string(),
        properties: json!({ "name": name, "summary": name, "source": "contradiction_candidates" }),
        pii_class: SensitivityClass::None,
        confidence: Some(0.9),
        valid_from: moa_test_support::fixtures::pg_now(),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        embedding_text: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

#[tokio::test]
async fn candidates_merge_hydrates_lexical_and_vector_hits_excluding_inactive_db_memory() {
    // Pins: the merged lexical+hydrate query returns fully hydrated rows for both
    // the lexical matches and the caller-supplied vector uids in one round trip,
    // and excludes invalidated nodes exactly as the prior separate hydrate query
    // did (item: merge contradiction lexical+hydrate queries).
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let storage_partition_id = tenant_id.0.to_string();
    let scope = RlsContext::tenant(tenant_id);
    let graph = PostgresGraphStore::scoped_for_app_role(
        store.pool().clone(),
        scope.clone(),
        Arc::new(LocalKmsProvider::new()),
    );

    let lexical_uid = Uuid::now_v7();
    let vector_uid = Uuid::now_v7();
    let inactive_uid = Uuid::now_v7();
    graph
        .create_node(fact_intent(
            &storage_partition_id,
            lexical_uid,
            "checkout deploys railway",
        ))
        .await
        .expect("create lexical fact");
    graph
        .create_node(fact_intent(
            &storage_partition_id,
            vector_uid,
            "unrelated widget gizmo",
        ))
        .await
        .expect("create vector-only fact");
    graph
        .create_node(fact_intent(
            &storage_partition_id,
            inactive_uid,
            "checkout deploys railway",
        ))
        .await
        .expect("create inactive fact");
    graph
        .invalidate_node(inactive_uid, "contradiction candidates test")
        .await
        .expect("invalidate inactive fact");

    let detector = RrfPlusJudgeDetector::default();
    let vector = Arc::new(StubVectorStore { hit: vector_uid });
    let ctx = ContradictionContext::for_app_role(store.pool().clone(), scope, vector);
    let embedding = vec![0.0_f32; VECTOR_DIMENSION];

    let candidates = detector
        .candidates(
            "checkout deploys railway",
            &embedding,
            NodeLabel::Fact,
            SensitivityClass::None,
            &ctx,
        )
        .await
        .expect("retrieve merged candidates");

    let uids = candidates
        .iter()
        .map(|candidate| candidate.uid)
        .collect::<Vec<_>>();
    assert!(
        uids.contains(&lexical_uid),
        "lexical match is hydrated: {uids:?}"
    );
    assert!(
        uids.contains(&vector_uid),
        "vector-only match is hydrated: {uids:?}"
    );
    assert!(
        !uids.contains(&inactive_uid),
        "invalidated node is excluded: {uids:?}"
    );
    // Full hydration (not uid-only): names come back on both rows.
    assert_eq!(
        candidates
            .iter()
            .find(|candidate| candidate.uid == lexical_uid)
            .expect("lexical row present")
            .name,
        "checkout deploys railway"
    );
    assert_eq!(
        candidates
            .iter()
            .find(|candidate| candidate.uid == vector_uid)
            .expect("vector row present")
            .name,
        "unrelated widget gizmo"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
