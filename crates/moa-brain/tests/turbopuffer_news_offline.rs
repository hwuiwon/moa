//! Wiremock offline counterpart for Turbopuffer news retrieval live coverage.

use moa_memory_vector::{TurbopufferStore, VECTOR_DIMENSION, VectorItem, VectorQuery, VectorStore};
use secrecy::SecretString;
use serde_json::json;
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn turbopuffer_news_offline_upsert_and_query_returns_promoted_news_fact() {
    let server = MockServer::start().await;
    let uid = Uuid::now_v7();
    Mock::given(method("POST"))
        .and(body_string_contains("upsert_rows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rows_affected": 1,
            "rows_upserted": 1
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("rank_by"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rows": [{ "id": uid.to_string(), "$dist": 0.0 }]
        })))
        .mount(&server)
        .await;

    let store = TurbopufferStore::new(
        server.uri(),
        SecretString::from("test-key"),
        "offline",
        false,
    )
    .expect("store should build");
    let workspace_id = "artemis-news-offline";
    store
        .upsert(&[VectorItem {
            uid,
            workspace_id: Some(workspace_id.to_string()),
            user_id: None,
            label: "Fact".to_string(),
            pii_class: "none".to_string(),
            embedding: basis_vector(7),
            embedding_model: "offline-news-embed".to_string(),
            embedding_model_version: 1,
            valid_to: None,
        }])
        .await
        .expect("wiremock upsert should succeed");
    let matches = store
        .knn(&VectorQuery {
            workspace_id: Some(workspace_id.to_string()),
            embedding: basis_vector(7),
            k: 5,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: "restricted".to_string(),
            include_global: false,
        })
        .await
        .expect("wiremock query should succeed");

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].uid, uid);
    assert_eq!(matches[0].score, 1.0);
}

fn basis_vector(index: usize) -> Vec<f32> {
    let mut embedding = vec![0.0; VECTOR_DIMENSION];
    embedding[index % VECTOR_DIMENSION] = 1.0;
    embedding
}
