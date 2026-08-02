//! Wiremock offline counterpart for Turbopuffer vector-store live coverage.

use moa_config::TurbopufferVectorType;
use moa_core::types::security::SensitivityClass;
use moa_memory_vector::{
    Error, TurbopufferStore, TurbopufferTextQuery, VECTOR_DIMENSION, VectorItem, VectorQuery,
    VectorStore,
};
use secrecy::SecretString;
use serde_json::json;
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn turbopuffer_offline_round_trip() {
    // Pins: upsert, query, and delete all use the one serving namespace derived
    // from vector type plus storage partition; there is no rebuild override.
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
    store.upsert(&[test_item(uid)]).await.expect("upsert");
    let matches = store
        .knn(&VectorQuery {
            embedding: moa_memory_vector::QueryEmbedding::new(
                basis_vector(0),
                "test-model".to_string(),
            )
            .expect("valid query embedding"),
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: SensitivityClass::Restricted,
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
    let namespace = format!("/v2/namespaces/moa-test-f16-{storage_partition_id}");
    let query_namespace = format!("{namespace}/query");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        vec![
            namespace.as_str(),
            query_namespace.as_str(),
            namespace.as_str(),
        ]
    );
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
            embedding: moa_memory_vector::QueryEmbedding::new(
                basis_vector(0),
                "test-model".to_string(),
            )
            .expect("valid query embedding"),
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: SensitivityClass::Restricted,
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

#[tokio::test]
async fn turbopuffer_offline_query_enforces_pii_ceiling_and_excludes_global_scope() {
    // Pins the Turbopuffer-side privacy boundary: the knn request body must carry
    // the `pii_rank <= ceiling` term and, with include_global=false, the
    // `scope != global` term. Without these, the backend would return rows above
    // the caller's PII ceiling or leak global-scope rows. `SensitivityClass::Phi`
    // (rank 2) makes the ceiling meaningful (it excludes restricted=3).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("rank_by"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "rows": [] })))
        .mount(&server)
        .await;

    let store = TurbopufferStore::new(server.uri(), SecretString::from("test-key"), "test", false)
        .expect("store")
        .with_storage_partition_id(Uuid::now_v7().to_string());

    store
        .knn(&VectorQuery {
            embedding: moa_memory_vector::QueryEmbedding::new(
                basis_vector(0),
                "test-model".to_string(),
            )
            .expect("valid query embedding"),
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: SensitivityClass::Phi,
            include_global: false,
            as_of: None,
        })
        .await
        .expect("query");

    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    let query_request = requests
        .iter()
        .find(|request| String::from_utf8_lossy(&request.body).contains("rank_by"))
        .expect("a knn query request was sent");
    let body: serde_json::Value =
        serde_json::from_slice(&query_request.body).expect("query body is JSON");
    let filters = body
        .get("filters")
        .expect("knn body carries a filters clause")
        .to_string();

    assert!(
        filters.contains(r#"["pii_rank","Lte",2]"#),
        "filters must cap pii_rank at the requested ceiling: {filters}"
    );
    assert!(
        filters.contains(r#"["scope","NotEq","global"]"#),
        "filters must exclude global scope when include_global=false: {filters}"
    );
}

#[tokio::test]
async fn turbopuffer_bm25_upsert_indexes_only_admitted_chunk_text() {
    // Pins: Turbopuffer FTS content is only written for tenant knowledge chunks
    // with admitted retrieval text; contact/session memory and non-chunk rows
    // cannot accidentally become BM25-searchable.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("upsert_rows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rows_affected": 3,
            "rows_upserted": 3
        })))
        .mount(&server)
        .await;

    let store = TurbopufferStore::new(server.uri(), SecretString::from("test-key"), "test", false)
        .expect("store")
        .with_storage_partition_id(Uuid::now_v7().to_string());
    let admitted_chunk = VectorItem {
        label: "Chunk".to_string(),
        search_text: Some("deployment runbook abc-123".to_string()),
        ..test_item(Uuid::now_v7())
    };
    let fact_with_text = VectorItem {
        label: "Fact".to_string(),
        search_text: Some("fact text must not be indexed".to_string()),
        ..test_item(Uuid::now_v7())
    };
    let contact_chunk = VectorItem {
        label: "Chunk".to_string(),
        user_id: Some("contact-1".to_string()),
        search_text: Some("contact memory must not be indexed".to_string()),
        ..test_item(Uuid::now_v7())
    };

    store
        .upsert(&[admitted_chunk, fact_with_text, contact_chunk])
        .await
        .expect("upsert");

    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("upsert body is JSON");
    assert_eq!(body.get("schema"), Some(&bm25_schema()));
    let rows = body["upsert_rows"]
        .as_array()
        .expect("upsert rows should be an array");
    let content_rows = rows
        .iter()
        .filter_map(|row| row.get("content"))
        .collect::<Vec<_>>();
    assert_eq!(content_rows, vec![&json!("deployment runbook abc-123")]);
}

#[tokio::test]
async fn turbopuffer_bm25_query_uses_content_rank_by_and_privacy_filters() {
    // Pins: BM25 requests rank over the `content` field and carry the same
    // validity, PII, scope, and label filters as vector KNN requests.
    let server = MockServer::start().await;
    let uid = Uuid::now_v7();
    Mock::given(method("POST"))
        .and(body_string_contains("BM25"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rows": [{ "id": uid.to_string(), "$score": 3.25 }]
        })))
        .mount(&server)
        .await;

    let store = TurbopufferStore::new(server.uri(), SecretString::from("test-key"), "test", false)
        .expect("store")
        .with_storage_partition_id(Uuid::now_v7().to_string());
    let matches = store
        .bm25(&TurbopufferTextQuery {
            query_text: "abc-123 deployment runbook".to_string(),
            k: 7,
            label_filter: Some(vec!["Chunk".to_string()]),
            max_pii_class: SensitivityClass::Phi,
            include_global: false,
        })
        .await
        .expect("bm25 query");

    assert_eq!(
        matches,
        vec![moa_memory_vector::VectorMatch { uid, score: 3.25 }]
    );
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("query body is JSON");
    assert_eq!(
        body["rank_by"],
        json!(["content", "BM25", "abc-123 deployment runbook"])
    );
    assert_eq!(body["top_k"], json!(7));

    let filters = body
        .get("filters")
        .expect("BM25 body carries a filters clause")
        .to_string();
    assert!(
        filters.contains(r#"["pii_rank","Lte",2]"#),
        "filters must cap pii_rank at the requested ceiling: {filters}"
    );
    assert!(
        filters.contains(r#"["valid_to","Eq","open"]"#),
        "filters must reject invalidated rows: {filters}"
    );
    assert!(
        filters.contains(r#"["scope","NotEq","global"]"#),
        "filters must exclude global scope when include_global=false: {filters}"
    );
    assert!(
        filters.contains(r#"["label","Eq","Chunk"]"#),
        "filters must carry the label allowlist: {filters}"
    );
}

#[tokio::test]
async fn turbopuffer_bm25_empty_query_returns_empty_without_http_request() {
    // Pins: empty lexical text has the same short-circuit behavior as an empty
    // vector query budget and does not spend a provider request.
    let server = MockServer::start().await;
    let store = TurbopufferStore::new(server.uri(), SecretString::from("test-key"), "test", false)
        .expect("store")
        .with_storage_partition_id(Uuid::now_v7().to_string());

    let matches = store
        .bm25(&TurbopufferTextQuery {
            query_text: "  ".to_string(),
            k: 10,
            label_filter: Some(vec!["Chunk".to_string()]),
            max_pii_class: SensitivityClass::Restricted,
            include_global: false,
        })
        .await
        .expect("empty BM25 query should short-circuit");

    assert!(matches.is_empty());
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(requests.len(), 0);
}

#[tokio::test]
async fn turbopuffer_default_upsert_declares_f16_vector_schema() {
    // Pins: the default Turbopuffer projection stores vectors as f16 in a typed
    // namespace while continuing to send f32 JSON query vectors to the API.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("upsert_rows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rows_affected": 1,
            "rows_upserted": 1
        })))
        .mount(&server)
        .await;

    let storage_partition_id = Uuid::now_v7().to_string();
    let store = TurbopufferStore::new(server.uri(), SecretString::from("test-key"), "test", false)
        .expect("store")
        .with_storage_partition_id(storage_partition_id.clone());

    store
        .upsert(&[test_item(Uuid::now_v7())])
        .await
        .expect("upsert");

    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(
        requests[0].url.path(),
        format!("/v2/namespaces/moa-test-f16-{storage_partition_id}")
    );
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("upsert body is JSON");
    assert_eq!(
        body.get("schema"),
        Some(&json!({
            "vector": {
                "type": "[1024]f16",
                "ann": true
            }
        }))
    );
}

#[test]
fn turbopuffer_f32_override_preserves_legacy_namespace_shape() {
    // Pins: operators can still target an existing f32 namespace explicitly
    // after the f16 default takes over for new projections.
    let storage_partition_id = Uuid::now_v7().to_string();
    let store = TurbopufferStore::new_with_vector_type(
        "https://example.test",
        SecretString::from("test-key"),
        "test",
        false,
        TurbopufferVectorType::F32,
    )
    .expect("store");

    assert_eq!(
        store
            .namespace_for_storage_partition(&storage_partition_id)
            .expect("namespace"),
        format!("moa-test-{storage_partition_id}")
    );
}

fn bm25_schema() -> serde_json::Value {
    json!({
        "vector": {
            "type": "[1024]f16",
            "ann": true
        },
        "content": {
            "type": "string",
            "full_text_search": true
        }
    })
}

fn test_item(uid: Uuid) -> VectorItem {
    VectorItem {
        uid,
        user_id: None,
        label: "Fact".to_string(),
        pii_class: SensitivityClass::None,
        embedding: basis_vector(0),
        embedding_model: "test-embed".to_string(),
        embedding_model_version: 1,
        search_text: None,
        valid_to: None,
    }
}

fn basis_vector(index: usize) -> Vec<f32> {
    let mut embedding = vec![0.0; VECTOR_DIMENSION];
    embedding[index % VECTOR_DIMENSION] = 1.0;
    embedding
}
