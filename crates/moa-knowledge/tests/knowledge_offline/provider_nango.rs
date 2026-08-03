//! Offline Nango provider adapter coverage.

use bytes::Bytes;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use moa_core::types::credentials::RedactedSecret;
use moa_core::types::identifiers::TenantId;
use moa_knowledge::{
    Error,
    domain::{
        ApplySourceSelectionRequest, CreateLinkTokenRequest, FetchRecordContentRequest,
        KnowledgeConnection, ListChangedRecordsRequest, ProviderIntegration, ProviderRecord,
        ProviderRecordAcl, RemoteRevokeRequest, StartInitialSyncRequest, TriggerSyncRequest,
    },
    providers::{LinkedIntegrationProvider, nango::NangoProvider},
};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::json;
use sha2::Sha256;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{bearer_token, body_bytes, body_json, header, method, path, query_param},
};

fn connection() -> KnowledgeConnection {
    let now = moa_test_support::fixtures::pg_now();
    KnowledgeConnection {
        connection_uid: Uuid::from_u128(101),
        tenant_id: TenantId::from(Uuid::from_u128(102)),
        provider: "nango".to_string(),
        connector: "google-drive".to_string(),
        provider_account_id: "conn_123".to_string(),
        metadata: json!({ "safe": true }),
        source_selection: json!({}),
        information_barrier: None,
        created_at: now,
        updated_at: now,
        last_synced_at: None,
    }
}

fn provider_record_acl() -> ProviderRecordAcl {
    ProviderRecordAcl {
        provider_revision: "fixture-acl-rev".to_string(),
        complete: true,
        entries: Vec::new(),
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
async fn initial_link_sync_uses_idempotent_start_and_never_the_one_off_trigger() {
    // Pins: the initial link uses Nango's naturally idempotent `/sync/start`, so
    // a crash between the durable sync-run claim and dispatch can replay the
    // exact call. The one-off `/sync/trigger` is not idempotent and must never
    // be reachable from a link that replays.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/sync/start"))
        .and(bearer_token("nango-test-key"))
        .and(body_json(json!({
            "connection_id": "conn_123",
            "provider_config_key": "google-drive",
            "syncs": []
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "provider_token": "must-redact"
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/sync/trigger"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "success": true })))
        .expect(0)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    for _ in 0..2 {
        let started = provider
            .start_initial_sync(StartInitialSyncRequest {
                credential: test_credential(),
                connection: connection(),
            })
            .await
            .expect("initial link sync start through local Nango fixture");

        assert_eq!(started.provider, "nango");
        assert!(
            !started.completed,
            "Nango starts asynchronously, so a successful start never proves completion"
        );
        assert!(started.metadata.get("provider_token").is_none());
    }
}

#[tokio::test]
async fn initial_link_sync_fails_closed_when_the_provider_rejects_the_start() {
    // Pins: a rejected start is an error, not a silently "running" sync, so the
    // owning link cannot finalize on a sync that never began.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/sync/start"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "success": false })))
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    provider
        .start_initial_sync(StartInitialSyncRequest {
            credential: test_credential(),
            connection: connection(),
        })
        .await
        .expect_err("a rejected start must fail the initial link closed");
}

#[tokio::test]
async fn remote_revoke_deletes_the_exact_nango_connection_without_a_secret_body() {
    // Pins: remote revocation uses Nango's current connection-delete contract:
    // exact provider account path, provider-config query, environment bearer
    // key, and no credential-bearing request body.
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/connections/conn_123"))
        .and(query_param("provider_config_key", "google-drive"))
        .and(bearer_token("nango-test-key"))
        .and(body_bytes(Vec::<u8>::new()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "success": true })))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let request = RemoteRevokeRequest {
        connection: connection(),
        credential: None,
    };

    provider
        .revoke_remote_connection(request)
        .await
        .expect("delete the exact Nango connection through the local fixture");

    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain the exact revoke request");
    assert_eq!(requests.len(), 1, "exactly one remote delete is allowed");
    assert_eq!(
        requests[0].url.query(),
        Some("provider_config_key=google-drive")
    );
}

#[tokio::test]
async fn remote_revoke_rejects_a_false_nango_delete_acknowledgement() {
    // Pins: a raced Nango delete can return HTTP 200 with `success:false`; that
    // does not prove the remote account is absent and cannot advance disconnect.
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/connections/conn_123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "success": false })))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let error = provider
        .revoke_remote_connection(RemoteRevokeRequest {
            connection: connection(),
            credential: None,
        })
        .await
        .expect_err("success:false must not be reported as a completed revoke");

    assert!(
        matches!(
            error,
            Error::Provider { provider, message }
                if provider == "nango" && message == "connection revoke was not confirmed by the provider"
        ),
        "false acknowledgement should produce the exact safe provider error"
    );
}

#[tokio::test]
async fn remote_revoke_does_not_invent_nango_already_absent_replay_success() {
    // Pins: Nango publishes possible 404s but no idempotent replay guarantee.
    // Until durable disconnect progress resolves the unknown-outcome boundary,
    // an already-absent-looking response remains an error rather than success.
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/connections/conn_123"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error": { "code": "unknown_connection" },
            "reflected_secret": "primary-secret-must-not-leak"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let error = provider
        .revoke_remote_connection(RemoteRevokeRequest {
            connection: connection(),
            credential: None,
        })
        .await
        .expect_err("an undocumented replay response must remain resumable/unknown");

    assert!(matches!(error, Error::HttpStatus { status: 404, .. }));
    assert!(
        !format!("{error:?}").contains("primary-secret-must-not-leak"),
        "provider response bodies and credentials must not enter errors"
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
            credential: test_credential(),
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
            credential: test_credential(),
            connection: connection.clone(),
            model: Some("documents".to_string()),
            variant: None,
        })
        .await
        .expect("trigger selected Nango variant");
    let page = provider
        .list_changed_records(ListChangedRecordsRequest {
            acl_key: std::sync::Arc::new(moa_knowledge::acl_key::SourceAclKey::new(1, vec![7; 32])),
            credential: test_credential(),
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
async fn records_list_fails_fast_when_no_sync_model_selected() {
    // Pins: Nango's GET /records requires a `model`; with no sync model in the
    // connection's source_selection, list_changed_records fails fast with an
    // actionable error and never issues an HTTP request that would return nothing.
    let provider = NangoProvider::with_client(
        reqwest::Client::new(),
        "https://api.invalid",
        "nango-test-key",
    );
    let connection = connection();
    assert!(
        connection.source_selection.get("model").is_none(),
        "fixture connection must have no selected sync model"
    );

    let error = provider
        .list_changed_records(ListChangedRecordsRequest {
            acl_key: std::sync::Arc::new(moa_knowledge::acl_key::SourceAclKey::new(1, vec![7; 32])),
            credential: test_credential(),
            connection,
            cursor: None,
            modified_after: None,
            limit: Some(1),
            variant: None,
        })
        .await
        .expect_err("missing sync model should be a clear error, not a silent empty page");

    match error {
        Error::Provider { provider, message } => {
            assert_eq!(provider, "nango");
            assert!(
                message.contains("requires a sync model"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected Error::Provider, got {other:?}"),
    }
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
            acl_key: std::sync::Arc::new(moa_knowledge::acl_key::SourceAclKey::new(1, vec![7; 32])),
            credential: test_credential(),
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
async fn list_integrations_maps_nango_configs_to_id_display_and_logo() {
    // Pins: GET /integrations maps each config's unique_key -> id (the connector),
    // display_name -> display_name (falling back to provider), and logo -> logo_url.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/integrations"))
        .and(bearer_token("nango-test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {
                    "unique_key": "google-drive",
                    "provider": "google-drive",
                    "display_name": "Google Drive",
                    "logo": "https://logos.example/google-drive.svg"
                },
                {
                    "unique_key": "notion",
                    "provider": "notion",
                    "display_name": null,
                    "logo": null
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let integrations = provider
        .list_integrations()
        .await
        .expect("integration listing should succeed");

    assert_eq!(
        integrations,
        vec![
            ProviderIntegration {
                id: "google-drive".to_string(),
                display_name: "Google Drive".to_string(),
                logo_url: Some("https://logos.example/google-drive.svg".to_string()),
            },
            ProviderIntegration {
                id: "notion".to_string(),
                display_name: "notion".to_string(),
                logo_url: None,
            },
        ]
    );
}

#[tokio::test]
async fn list_integrations_surfaces_upstream_errors() {
    // Pins: a non-success status from /integrations surfaces as an HttpStatus
    // error rather than an empty catalog.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/integrations"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let error = provider
        .list_integrations()
        .await
        .expect_err("upstream 500 must surface as an error");

    assert!(
        matches!(error, Error::HttpStatus { status: 500, .. }),
        "expected an HttpStatus 500 error, got {error:?}"
    );
}

fn drive_record(source_id: &str, mime_type: &str) -> ProviderRecord {
    ProviderRecord {
        acl: provider_record_acl(),
        source_id: source_id.to_string(),
        object_type: "drive_file".to_string(),
        title: Some(format!("{source_id} title")),
        // Auth-walled browser viewer link; never a fetchable content URL.
        source_uri: Some(format!("https://drive.google.com/file/d/{source_id}/view")),
        change_token: None,
        deleted: false,
        source_updated_at: None,
        metadata: json!({}),
        payload: json!({ "mimeType": mime_type }),
    }
}

#[tokio::test]
async fn content_fetch_exports_google_apps_files_as_plain_text() {
    // Pins: a Google Workspace editor file (google-apps MIME) is fetched through
    // the Nango proxy export endpoint requesting text/plain, with the same auth
    // headers as /records, and returns the exported text with a text/plain MIME.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/proxy/drive/v3/files/doc-apps/export"))
        .and(bearer_token("nango-test-key"))
        .and(header("Connection-Id", "conn_123"))
        .and(header("Provider-Config-Key", "google-drive"))
        .and(query_param("mimeType", "text/plain"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/plain; charset=utf-8")
                .set_body_string("Exported roadmap body."),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let content = provider
        .fetch_record_content(FetchRecordContentRequest {
            credential: test_credential(),
            connection: connection(),
            record: drive_record("doc-apps", "application/vnd.google-apps.document"),
        })
        .await
        .expect("export fetch should succeed")
        .expect("google-apps record should yield content");

    assert_eq!(content.bytes, b"Exported roadmap body.");
    assert_eq!(content.mime_type.as_deref(), Some("text/plain"));
}

#[tokio::test]
async fn content_fetch_streams_binary_files_via_alt_media() {
    // Pins: a non-editor Drive file is fetched verbatim through the proxy with
    // alt=media, and the response content type is preserved as the MIME.
    let server = MockServer::start().await;
    let pdf_bytes = b"%PDF-1.7\n%binary body".to_vec();
    Mock::given(method("GET"))
        .and(path("/proxy/drive/v3/files/doc-bin"))
        .and(bearer_token("nango-test-key"))
        .and(header("Connection-Id", "conn_123"))
        .and(header("Provider-Config-Key", "google-drive"))
        .and(query_param("alt", "media"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/pdf")
                .set_body_bytes(pdf_bytes.clone()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let content = provider
        .fetch_record_content(FetchRecordContentRequest {
            credential: test_credential(),
            connection: connection(),
            record: drive_record("doc-bin", "application/pdf"),
        })
        .await
        .expect("binary fetch should succeed")
        .expect("binary record should yield content");

    assert_eq!(content.bytes, pdf_bytes);
    assert_eq!(content.mime_type.as_deref(), Some("application/pdf"));
}

#[tokio::test]
async fn content_fetch_rejects_bodies_over_the_size_cap() {
    // Pins: a response larger than the 10 MiB content cap is rejected as a decode
    // error rather than buffered into memory.
    let server = MockServer::start().await;
    // One byte over the crate's 10 MiB record content cap.
    let oversized = vec![b'a'; 10 * 1024 * 1024 + 1];
    Mock::given(method("GET"))
        .and(path("/proxy/drive/v3/files/doc-huge"))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(oversized))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let error = provider
        .fetch_record_content(FetchRecordContentRequest {
            credential: test_credential(),
            connection: connection(),
            record: drive_record("doc-huge", "application/octet-stream"),
        })
        .await
        .expect_err("oversized content must be rejected");

    assert!(
        matches!(&error, Error::Decode(message) if message.contains("size cap")),
        "expected a size-cap decode error, got {error:?}"
    );
}

#[tokio::test]
async fn content_fetch_returns_none_for_non_text_exportable_google_apps_types() {
    // Pins: a google-apps type with no text export (e.g. a Drive folder) yields
    // None and issues no request, instead of a doomed export that Drive rejects.
    let provider = NangoProvider::with_client(
        reqwest::Client::new(),
        "https://api.invalid",
        "nango-test-key",
    );

    let content = provider
        .fetch_record_content(FetchRecordContentRequest {
            credential: test_credential(),
            connection: connection(),
            record: drive_record("folder-1", "application/vnd.google-apps.folder"),
        })
        .await
        .expect("non-exportable google-apps type should not error");

    assert!(
        content.is_none(),
        "a Drive folder has no fetchable content and must return None"
    );
}

#[tokio::test]
async fn content_fetch_exports_spreadsheets_as_csv() {
    // Pins: Google Sheets export to text/csv (text/plain is rejected by Drive).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/proxy/drive/v3/files/sheet-1/export"))
        .and(query_param("mimeType", "text/csv"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/csv")
                .set_body_string("a,b\n1,2"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let content = provider
        .fetch_record_content(FetchRecordContentRequest {
            credential: test_credential(),
            connection: connection(),
            record: drive_record("sheet-1", "application/vnd.google-apps.spreadsheet"),
        })
        .await
        .expect("spreadsheet export should succeed")
        .expect("spreadsheet should yield content");

    assert_eq!(content.bytes, b"a,b\n1,2");
    assert_eq!(content.mime_type.as_deref(), Some("text/csv"));
}

#[tokio::test]
async fn content_fetch_returns_none_for_non_drive_integrations() {
    // Pins: an integration without a known proxy content path yields None (not an
    // error) and issues no HTTP request.
    let provider = NangoProvider::with_client(
        reqwest::Client::new(),
        "https://api.invalid",
        "nango-test-key",
    );
    let mut connection = connection();
    connection.connector = "notion".to_string();

    let content = provider
        .fetch_record_content(FetchRecordContentRequest {
            credential: test_credential(),
            connection,
            record: drive_record("page-1", "text/markdown"),
        })
        .await
        .expect("unsupported integration should not error");

    assert!(
        content.is_none(),
        "unsupported integration should return no content"
    );
}

#[tokio::test]
async fn content_fetch_surfaces_upstream_errors() {
    // Pins: an upstream non-success status surfaces as an HttpStatus error so the
    // pipeline can distinguish a failed fetch from an unsupported one.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/proxy/drive/v3/files/doc-error"))
        .and(query_param("alt", "media"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        NangoProvider::with_client(reqwest::Client::new(), server.uri(), "nango-test-key");
    let error = provider
        .fetch_record_content(FetchRecordContentRequest {
            credential: test_credential(),
            connection: connection(),
            record: drive_record("doc-error", "application/pdf"),
        })
        .await
        .expect_err("upstream 500 must surface as an error");

    assert!(
        matches!(error, Error::HttpStatus { status: 500, .. }),
        "expected an HttpStatus 500 error, got {error:?}"
    );
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

/// Builds the resolved credential a provider request carries.
///
/// Provider requests take a non-serializable redacted secret, so tests build one
/// explicitly instead of smuggling material through the connection.
fn test_credential() -> Option<RedactedSecret> {
    None
}
