//! Live Turbopuffer integration tests.

use moa_memory_vector::{TurbopufferStore, VECTOR_DIMENSION, VectorItem, VectorQuery, VectorStore};
use uuid::Uuid;

fn live_store() -> TurbopufferStore {
    if std::env::var("MOA_RUN_LIVE_TURBOPUFFER_TESTS").as_deref() != Ok("1") {
        panic!("set MOA_RUN_LIVE_TURBOPUFFER_TESTS=1 to run live Turbopuffer tests");
    }
    TurbopufferStore::from_env().expect("TURBOPUFFER_API_KEY and Turbopuffer config")
}

#[tokio::test]
#[ignore = "live Turbopuffer test; requires MOA_RUN_LIVE_TURBOPUFFER_TESTS=1 and TURBOPUFFER_API_KEY"]
async fn turbopuffer_live_round_trip() {
    let store = live_store();
    let workspace_id = format!("live-{}", Uuid::now_v7());
    let uid = Uuid::now_v7();
    let item = VectorItem {
        uid,
        workspace_id: Some(workspace_id.clone()),
        user_id: None,
        label: "Fact".to_string(),
        pii_class: "none".to_string(),
        embedding: basis_vector(7),
        embedding_model: "live-test".to_string(),
        embedding_model_version: 1,
        valid_to: None,
    };

    store
        .upsert(std::slice::from_ref(&item))
        .await
        .expect("upsert");
    let matches = store
        .knn(&VectorQuery {
            workspace_id: Some(workspace_id.clone()),
            embedding: item.embedding,
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: "restricted".to_string(),
            include_global: false,
        })
        .await
        .expect("query");
    assert!(
        matches.iter().any(|row| row.uid == uid),
        "live query did not return inserted uid: {matches:?}"
    );

    store
        .delete_in_workspace(&workspace_id, &[uid])
        .await
        .expect("delete");
}

fn basis_vector(index: usize) -> Vec<f32> {
    let mut embedding = vec![0.0; VECTOR_DIMENSION];
    embedding[index % VECTOR_DIMENSION] = 1.0;
    embedding
}
