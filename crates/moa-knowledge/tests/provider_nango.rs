//! Offline Nango provider adapter coverage.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use moa_core::TenantId;
use moa_knowledge::{
    Error,
    domain::{
        ConnectionStatus, KnowledgeConnection, ListChangedRecordsRequest, TriggerSyncRequest,
    },
    providers::{LinkedIntegrationProvider, nango::NangoProvider},
};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::json;
use sha2::Sha256;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{bearer_token, body_json, method, path, query_param},
};

fn connection() -> KnowledgeConnection {
    let now = Utc::now();
    KnowledgeConnection {
        connection_uid: Uuid::from_u128(101),
        tenant_id: TenantId::from(Uuid::from_u128(102)),
        provider: "nango".to_string(),
        connector: "google-drive".to_string(),
        provider_account_id: "conn_123".to_string(),
        credential_ref: "vault://tenant/nango/google-drive".to_string(),
        status: ConnectionStatus::Active,
        metadata: json!({ "safe": true }),
        created_at: now,
        updated_at: now,
        last_synced_at: None,
    }
}

fn ts(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("fixture timestamp should be RFC3339")
        .with_timezone(&Utc)
}

fn signature(body: &[u8], signing_key: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(signing_key.as_bytes())
        .expect("test signing key should initialize HMAC");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

#[tokio::test]
async fn trigger_sync_posts_provider_config_connection_and_sync_name() {
    // Pins: Nango one-off sync trigger requests use the provider_config_key, connection_id, and sync name.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/sync/trigger"))
        .and(bearer_token("nango-test-key"))
        .and(body_json(json!({
            "connection_id": "conn_123",
            "provider_config_key": "google-drive",
            "sync_name": "documents"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sync_id": "sync_456",
            "status": "started",
            "provider_token": "must-redact"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let triggered = provider
        .trigger_sync(TriggerSyncRequest {
            connection: connection(),
            model: Some("documents".to_string()),
        })
        .await
        .expect("trigger sync through local Nango fixture");

    assert_eq!(triggered.provider, "nango");
    assert_eq!(triggered.provider_sync_id.as_deref(), Some("sync_456"));
    assert_eq!(triggered.status, "started");
    assert!(triggered.metadata.get("provider_token").is_none());
}

#[tokio::test]
async fn records_list_maps_cursor_deleted_metadata_and_change_tokens() {
    // Pins: Nango record-cache pagination maps cursors and deleted-record metadata without live credentials.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/records"))
        .and(bearer_token("nango-test-key"))
        .and(query_param("connection_id", "conn_123"))
        .and(query_param("provider_config_key", "google-drive"))
        .and(query_param("cursor", "cursor-1"))
        .and(query_param("limit", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "records": [
                {
                    "id": "doc-1",
                    "model": "drive_file",
                    "deleted": false,
                    "modified_at": "2026-06-26T12:00:00Z",
                    "metadata": {"safe": "primary"},
                    "title": "Roadmap",
                    "url": "https://drive.example/doc-1",
                    "_nango_metadata": {"last_action": "UPDATED"}
                },
                {
                    "id": "doc-2",
                    "model": "drive_file",
                    "deleted": true,
                    "modified_at": "2026-06-26T12:05:00Z",
                    "metadata": {
                        "safe": "deleted",
                        "deleted_at": "2026-06-26T12:06:00Z"
                    },
                    "name": "Old roadmap",
                    "_nango_metadata": {"last_action": "DELETED"}
                }
            ],
            "next_cursor": "cursor-2"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let page = provider
        .list_changed_records(ListChangedRecordsRequest {
            connection: connection(),
            cursor: Some("cursor-1".to_string()),
            modified_after: None,
            limit: Some(2),
        })
        .await
        .expect("list Nango records through local fixture");

    assert_eq!(page.next_cursor.as_deref(), Some("cursor-2"));
    assert_eq!(page.records.len(), 2);
    assert_eq!(page.records[0].source_id, "doc-1");
    assert_eq!(page.records[0].object_type, "drive_file");
    assert_eq!(page.records[0].title.as_deref(), Some("Roadmap"));
    assert_eq!(
        page.records[0].source_uri.as_deref(),
        Some("https://drive.example/doc-1")
    );
    assert_eq!(page.records[0].change_token.as_deref(), Some("UPDATED"));
    assert_eq!(
        page.records[0].source_updated_at,
        Some(ts("2026-06-26T12:00:00Z"))
    );
    assert!(!page.records[0].deleted);

    assert_eq!(page.records[1].source_id, "doc-2");
    assert!(page.records[1].deleted);
    assert_eq!(page.records[1].metadata["safe"], "deleted");
    assert_eq!(
        page.records[1].metadata["deleted_at"],
        "2026-06-26T12:06:00Z"
    );
    assert_eq!(page.records[1].change_token.as_deref(), Some("DELETED"));
}

#[tokio::test]
async fn sync_completed_webhook_verifies_signature_and_rejects_bad_signature() {
    // Pins: Nango webhook payloads are trusted only after HMAC verification.
    let signing_key = "nango-webhook-secret";
    let provider = NangoProvider::with_client(
        reqwest::Client::new(),
        "https://nango.invalid",
        "nango-test-key",
    )
    .with_webhook_signing_key(signing_key);
    let body = Bytes::from_static(
        br#"{"id":"evt_1","type":"sync:completed","connection_id":"conn_123","sync_name":"documents"}"#,
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-nango-hmac-sha256",
        HeaderValue::from_str(&signature(&body, signing_key)).expect("valid hex signature header"),
    );

    let event = provider
        .verify_webhook(headers.clone(), body.clone())
        .await
        .expect("valid Nango webhook signature");
    assert_eq!(event.provider, "nango");
    assert_eq!(event.event_id, "evt_1");
    assert_eq!(event.event_type, "sync:completed");
    assert_eq!(event.metadata["connection_id"], "conn_123");

    headers.insert(
        "x-nango-hmac-sha256",
        HeaderValue::from_static("00000000000000000000000000000000"),
    );
    let error = provider
        .verify_webhook(headers, body)
        .await
        .expect_err("bad Nango signature should fail");
    assert!(matches!(
        error,
        Error::Provider { provider, message }
            if provider == "nango" && message.contains("signature verification failed")
    ));
}
