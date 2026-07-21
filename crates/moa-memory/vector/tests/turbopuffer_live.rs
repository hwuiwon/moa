// Live counterpart: see turbopuffer_offline.rs for the wiremock version that runs in PR CI.

//! Live Turbopuffer integration tests.

use moa_core::types::security::SensitivityClass;
use moa_memory_vector::{TurbopufferStore, VECTOR_DIMENSION, VectorItem, VectorQuery, VectorStore};
use uuid::Uuid;

/// Returns `true` when `name` is set to a common truthy value (`1`, `true`,
/// `yes`, or `on`, case-insensitively after trimming), matching how live-test
/// flags are written in a developer's `.env`.
fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn live_store() -> TurbopufferStore {
    if !env_flag_enabled("MOA_RUN_LIVE_TURBOPUFFER_TESTS") {
        panic!("set MOA_RUN_LIVE_TURBOPUFFER_TESTS=1 to run live Turbopuffer tests");
    }
    TurbopufferStore::from_env().expect("MOA_TURBOPUFFER_API_KEY and Turbopuffer config")
}

#[tokio::test]
#[ignore = "live Turbopuffer test; requires MOA_RUN_LIVE_TURBOPUFFER_TESTS=1 and MOA_TURBOPUFFER_API_KEY"]
async fn turbopuffer_live_round_trip() {
    let storage_partition_id = format!("live-{}", Uuid::now_v7());
    let store = live_store().with_storage_partition_id(storage_partition_id.clone());
    let uid = Uuid::now_v7();
    let item = VectorItem {
        uid,
        user_id: None,
        label: "Fact".to_string(),
        pii_class: SensitivityClass::None,
        embedding: basis_vector(7),
        embedding_model: "live-test".to_string(),
        embedding_model_version: 1,
        search_text: None,
        valid_to: None,
    };

    store
        .upsert(std::slice::from_ref(&item))
        .await
        .expect("upsert");
    let matches = store
        .knn(&VectorQuery {
            embedding: item.embedding,
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: SensitivityClass::Restricted,
            include_global: false,
            as_of: None,
        })
        .await
        .expect("query");
    assert!(
        matches.iter().any(|row| row.uid == uid),
        "live query did not return inserted uid: {matches:?}"
    );

    store
        .delete_in_storage_partition(&storage_partition_id, &[uid])
        .await
        .expect("delete");
}

fn basis_vector(index: usize) -> Vec<f32> {
    let mut embedding = vec![0.0; VECTOR_DIMENSION];
    embedding[index % VECTOR_DIMENSION] = 1.0;
    embedding
}
