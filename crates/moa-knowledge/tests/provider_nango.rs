//! Offline Nango provider adapter coverage.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use moa_core::TenantId;
use moa_knowledge::{
    Error,
    domain::{
        ApplySourceSelectionRequest, ConnectionStatus, CreateLinkTokenRequest, KnowledgeConnection,
        ListChangedRecordsRequest, TriggerSyncRequest,
    },
    providers::{LinkedIntegrationProvider, nango::NangoProvider},
};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::json;
use sha2::Sha256;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{bearer_token, body_json, header, method, path, query_param},
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
        source_selection: json!({}),
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
async fn link_token_creation_forwards_nango_metadata_selection() {
    // Pins: Nango link-token creation uses the current connect-session shape.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/connect/sessions"))
        .and(bearer_token("nango-test-key"))
        .and(body_json(json!({
            "tags": {
                "tenant_id": connection().tenant_id.to_string(),
                "external_account_id": "account-123"
            },
            "allowed_integrations": ["google-drive"]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "token": "link-token-123",
                "connect_link": "https://connect.nango.dev/session/link-token-123"
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let token = provider
        .create_link_token(CreateLinkTokenRequest {
            tenant_id: connection().tenant_id,
            connector: "google-drive".to_string(),
            external_account_id: Some("account-123".to_string()),
            end_user_email_address: None,
            redirect_url: None,
            source_selection: json!({
                "metadata": {
                    "selected_folder_ids": ["folder-1"],
                    "selected_file_ids": ["file-1"]
                }
            }),
        })
        .await
        .expect("create Nango link token with source selection metadata");

    assert_eq!(token.provider, "nango");
    assert_eq!(token.token, "link-token-123");
    assert_eq!(
        token.link_url.as_deref(),
        Some("https://connect.nango.dev/session/link-token-123")
    );
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
            "syncs": ["documents"]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
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
            variant: None,
        })
        .await
        .expect("trigger sync through local Nango fixture");

    assert_eq!(triggered.provider, "nango");
    assert_eq!(triggered.provider_sync_id, None);
    assert_eq!(triggered.status, "accepted");
    assert!(triggered.metadata.get("provider_token").is_none());
}

#[tokio::test]
async fn source_selection_updates_nango_metadata_and_sync_variants() {
    // Pins: selected Nango sources are applied through connection metadata and optional sync variants.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/connections/metadata"))
        .and(bearer_token("nango-test-key"))
        .and(body_json(json!({
            "connection_id": "conn_123",
            "provider_config_key": "google-drive",
            "metadata": {
                "selected_folder_ids": ["folder-1"],
                "selected_file_ids": ["file-1"]
            }
        })))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sync/documents/variant/selected-sources"))
        .and(bearer_token("nango-test-key"))
        .and(body_json(json!({
            "connection_id": "conn_123",
            "provider_config_key": "google-drive"
        })))
        .respond_with(ResponseTemplate::new(409))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let mut connection = connection();
    connection.source_selection = json!({
        "metadata": {
            "selected_folder_ids": ["folder-1"],
            "selected_file_ids": ["file-1"]
        },
        "sync_name": "documents",
        "variant": "selected-sources"
    });

    let outcome = provider
        .apply_source_selection(ApplySourceSelectionRequest { connection })
        .await;
    // Pins the concrete 409 outcome: Nango returns 409 Conflict when the
    // selected-sources variant already exists, and the provider treats that
    // conflict as idempotent success rather than surfacing it as an error.
    assert!(
        outcome.is_ok(),
        "409 Conflict on variant creation must be treated as idempotent success, got {outcome:?}"
    );
}

#[tokio::test]
async fn source_selection_surfaces_non_conflict_variant_error() {
    // Pins: a non-409 variant-creation failure (500) is surfaced as an HttpStatus
    // error rather than swallowed like the idempotent 409 conflict path.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/connections/metadata"))
        .and(bearer_token("nango-test-key"))
        .respond_with(ResponseTemplate::new(201))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sync/documents/variant/selected-sources"))
        .and(bearer_token("nango-test-key"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let mut connection = connection();
    connection.source_selection = json!({
        "metadata": {
            "selected_folder_ids": ["folder-1"],
            "selected_file_ids": ["file-1"]
        },
        "sync_name": "documents",
        "variant": "selected-sources"
    });

    let error = provider
        .apply_source_selection(ApplySourceSelectionRequest { connection })
        .await
        .expect_err("non-409 variant creation failure must surface as an error");
    assert!(
        matches!(error, Error::HttpStatus { status: 500, .. }),
        "500 from variant creation should surface as an HttpStatus error, got {error:?}"
    );
}

#[tokio::test]
async fn trigger_and_records_list_include_selected_variant() {
    // Pins: Nango sync trigger and record reads target the selected records-cache variant.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/sync/trigger"))
        .and(bearer_token("nango-test-key"))
        .and(body_json(json!({
            "connection_id": "conn_123",
            "provider_config_key": "google-drive",
            "syncs": [
                {
                    "name": "documents",
                    "variant": "selected-sources"
                }
            ]
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/records"))
        .and(bearer_token("nango-test-key"))
        .and(header("Connection-Id", "conn_123"))
        .and(header("Provider-Config-Key", "google-drive"))
        .and(query_param("model", "documents"))
        .and(query_param("limit", "1"))
        .and(query_param("variant", "selected-sources"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "records": [],
            "next_cursor": null
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let mut connection = connection();
    connection.source_selection = json!({
        "model": "documents",
        "variant": "selected-sources"
    });

    provider
        .trigger_sync(TriggerSyncRequest {
            connection: connection.clone(),
            model: Some("documents".to_string()),
            variant: None,
        })
        .await
        .expect("trigger selected Nango variant");
    let page = provider
        .list_changed_records(ListChangedRecordsRequest {
            connection,
            cursor: None,
            modified_after: None,
            limit: Some(1),
            variant: None,
        })
        .await
        .expect("list selected Nango variant records");

    assert_eq!(page.records.len(), 0);
}

#[tokio::test]
async fn records_list_maps_cursor_deleted_metadata_and_change_tokens() {
    // Pins: Nango record-cache pagination maps cursors and deleted-record metadata without live credentials.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/records"))
        .and(bearer_token("nango-test-key"))
        .and(header("Connection-Id", "conn_123"))
        .and(header("Provider-Config-Key", "google-drive"))
        .and(query_param("model", "documents"))
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
    let mut connection = connection();
    connection.source_selection = json!({ "model": "documents" });
    let page = provider
        .list_changed_records(ListChangedRecordsRequest {
            connection,
            cursor: Some("cursor-1".to_string()),
            modified_after: None,
            limit: Some(2),
            variant: None,
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
    // Pins: Nango webhook payloads are trusted only after HMAC verification, and signed account fields are preserved for local binding.
    let signing_key = "nango-webhook-secret";
    let provider = NangoProvider::with_client(
        reqwest::Client::new(),
        "https://nango.invalid",
        "nango-test-key",
    )
    .with_webhook_signing_key(signing_key);
    let body = Bytes::from_static(
        br#"{"id":"evt_1","type":"sync:completed","connection_id":"conn_123","provider_config_key":"google-drive","sync_name":"documents"}"#,
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
    assert_eq!(event.metadata["provider_config_key"], "google-drive");

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
