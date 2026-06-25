//! Wiremock offline counterpart for Turbopuffer vector-store live coverage.

use moa_memory_vector::{
    Error, TurbopufferStore, VECTOR_DIMENSION, VectorItem, VectorQuery, VectorStore,
};
use secrecy::SecretString;
use serde_json::json;
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn turbopuffer_offline_round_trip() {
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
            "rows": [{ "id": uid.to_string(), "$dist": 0.2 }]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("deletes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rows_affected": 1
        })))
        .mount(&server)
        .await;

    let storage_partition_id = Uuid::now_v7().to_string();
    let store = TurbopufferStore::new(server.uri(), SecretString::from("test-key"), "test", false)
        .expect("store")
        .with_storage_partition_id(storage_partition_id.clone());
    store
        .upsert(&[test_item(uid, &storage_partition_id)])
        .await
        .expect("upsert");
    let matches = store
        .knn(&VectorQuery {
            embedding: basis_vector(0),
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: "restricted".to_string(),
            include_global: false,
            as_of: None,
        })
        .await
        .expect("query");
    store
        .delete_in_storage_partition(&storage_partition_id, &[uid])
        .await
        .expect("delete");

    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].uid, uid);
    assert!((matches[0].score - 0.8).abs() < f32::EPSILON);
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(requests.len(), 3);
}

#[tokio::test]
async fn turbopuffer_as_of_query_returns_unsupported_without_http_request() {
    // Pins: Turbopuffer historical queries return the typed unsupported feature error locally.
    let server = MockServer::start().await;
    let store = TurbopufferStore::new(server.uri(), SecretString::from("test-key"), "test", false)
        .expect("store")
        .with_storage_partition_id(Uuid::now_v7().to_string());
    let error = store
        .knn(&VectorQuery {
            embedding: basis_vector(0),
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: "restricted".to_string(),
            include_global: false,
            as_of: Some(
                chrono::DateTime::parse_from_rfc3339("2026-03-01T00:00:00Z")
                    .expect("test timestamp should parse")
                    .with_timezone(&chrono::Utc),
            ),
        })
        .await
        .expect_err("as-of query should be rejected before HTTP");

    assert!(matches!(
        error,
        Error::UnsupportedQueryFeature {
            backend: "turbopuffer",
            feature: "as_of"
        }
    ));
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(requests.len(), 0);
}

fn test_item(uid: Uuid, storage_partition_id: &str) -> VectorItem {
    let _ = storage_partition_id;
    VectorItem {
        uid,
        user_id: None,
        label: "Fact".to_string(),
        pii_class: "none".to_string(),
        embedding: basis_vector(0),
        embedding_model: "test-embed".to_string(),
        embedding_model_version: 1,
        valid_to: None,
    }
}

fn basis_vector(index: usize) -> Vec<f32> {
    let mut embedding = vec![0.0; VECTOR_DIMENSION];
    embedding[index % VECTOR_DIMENSION] = 1.0;
    embedding
}
