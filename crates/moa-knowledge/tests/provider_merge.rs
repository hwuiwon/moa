//! Offline Merge provider adapter coverage.

use base64::{Engine as _, engine::general_purpose};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use moa_core::TenantId;
use moa_knowledge::{
    Error,
    domain::{
        ConnectionStatus, CreateLinkTokenRequest, ExchangePublicTokenRequest, KnowledgeConnection,
        ListChangedRecordsRequest,
    },
    providers::{LinkedIntegrationProvider, merge::MergeProvider},
};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::json;
use sha2::Sha256;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{bearer_token, body_bytes, body_json, header, method, path, query_param},
};

fn tenant_id() -> TenantId {
    TenantId::from(Uuid::from_u128(202))
}

fn connection() -> KnowledgeConnection {
    let now = Utc::now();
    KnowledgeConnection {
        connection_uid: Uuid::from_u128(201),
        tenant_id: tenant_id(),
        provider: "merge".to_string(),
        connector: "crm".to_string(),
        provider_account_id: "linked-account-123".to_string(),
        credential_ref: "account-token-123".to_string(),
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

fn signature(body: &[u8], signature_key: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(signature_key.as_bytes())
        .expect("test signature key should initialize HMAC");
    mac.update(body);
    general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

#[tokio::test]
async fn link_token_creation_posts_merge_link_shape() {
    // Pins: Merge link-token creation sends tenant-scoped end-user identity and requested category.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/integrations/create-link-token"))
        .and(bearer_token("merge-test-key"))
        .and(body_json(json!({
            "end_user_origin_id": "operator-facing-account",
            "end_user_email_address": "operator@example.com",
            "end_user_organization_name": tenant_id().to_string(),
            "categories": ["crm"],
            "redirect_uri": "https://app.example/merge/callback"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "link_token": "link-token-123",
            "magic_link_url": "https://link.merge.dev/link-token-123",
            "expires_at": "2026-06-26T12:00:00Z"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        MergeProvider::with_client(reqwest::Client::new(), server.uri(), "merge-test-key");
    let token = provider
        .create_link_token(CreateLinkTokenRequest {
            tenant_id: tenant_id(),
            connector: "crm".to_string(),
            external_account_id: Some("operator-facing-account".to_string()),
            end_user_email_address: Some("operator@example.com".to_string()),
            redirect_url: Some("https://app.example/merge/callback".to_string()),
            source_selection: json!({}),
        })
        .await
        .expect("create Merge link token through local fixture");

    assert_eq!(token.provider, "merge");
    assert_eq!(token.token, "link-token-123");
    assert_eq!(
        token.link_url.as_deref(),
        Some("https://link.merge.dev/link-token-123")
    );
    assert_eq!(token.expires_at, Some(ts("2026-06-26T12:00:00Z")));
}

#[tokio::test]
async fn public_token_exchange_gets_account_token_path_and_maps_metadata() {
    // Pins: Merge public-token exchange uses the official bodyless account-token GET path.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/integrations/account-token/public%2Ftoken%3Fabc"))
        .and(bearer_token("merge-test-key"))
        .and(body_bytes(Vec::<u8>::new()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "account_token": "account-token-123",
            "id": "linked-account-123",
            "integration": {
                "name": "Salesforce",
                "access_token": "must-redact"
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        MergeProvider::with_client(reqwest::Client::new(), server.uri(), "merge-test-key");
    let account = provider
        .exchange_public_token(ExchangePublicTokenRequest {
            tenant_id: tenant_id(),
            public_token: "public/token?abc".to_string(),
            source_selection: json!({}),
        })
        .await
        .expect("exchange Merge public token through local fixture");

    assert_eq!(account.provider, "merge");
    assert_eq!(account.provider_account_id, "linked-account-123");
    assert_eq!(account.credential_ref, "merge-account-token");
    assert_eq!(
        account.credential_material.as_deref(),
        Some("account-token-123")
    );
    assert_eq!(account.metadata["name"], "Salesforce");
    assert!(account.metadata.get("access_token").is_none());
}

#[tokio::test]
async fn changed_records_request_includes_modified_after_and_maps_results() {
    // Pins: Merge incremental reads use the official knowledgebase articles endpoint.
    let server = MockServer::start().await;
    let modified_after = ts("2026-06-26T12:00:00Z");
    Mock::given(method("GET"))
        .and(path("/api/knowledgebase/v1/articles"))
        .and(bearer_token("merge-test-key"))
        .and(header("X-Account-Token", "account-token-123"))
        .and(query_param("cursor", "cursor-1"))
        .and(query_param("page_size", "2"))
        .and(query_param("modified_after", modified_after.to_rfc3339()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "next": "cursor-2",
            "results": [
                {
                    "id": "article-1",
                    "remote_id": "remote-article-1",
                    "model": "article",
                    "title": "Reset VPN",
                    "article_url": "https://kb.example/articles/reset-vpn",
                    "modified_at": "2026-06-26T12:30:00Z",
                    "remote_was_deleted": false,
                    "access_token": "must-redact"
                },
                {
                    "id": "article-2",
                    "remote_id": "remote-article-2",
                    "object_type": "article",
                    "title": "Deleted KB",
                    "modified_at": "2026-06-26T12:31:00Z",
                    "remote_was_deleted": true
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        MergeProvider::with_client(reqwest::Client::new(), server.uri(), "merge-test-key");
    let page = provider
        .list_changed_records(ListChangedRecordsRequest {
            connection: connection(),
            cursor: Some("cursor-1".to_string()),
            modified_after: Some(modified_after),
            limit: Some(2),
            variant: None,
        })
        .await
        .expect("list Merge records through local fixture");

    assert_eq!(page.next_cursor.as_deref(), Some("cursor-2"));
    assert_eq!(page.records.len(), 2);
    assert_eq!(page.records[0].source_id, "article-1");
    assert_eq!(page.records[0].object_type, "article");
    assert_eq!(page.records[0].title.as_deref(), Some("Reset VPN"));
    assert_eq!(
        page.records[0].source_uri.as_deref(),
        Some("https://kb.example/articles/reset-vpn")
    );
    assert_eq!(
        page.records[0].source_updated_at,
        Some(ts("2026-06-26T12:30:00Z"))
    );
    assert!(!page.records[0].deleted);
    assert!(page.records[0].payload.get("access_token").is_none());
    assert_eq!(page.records[1].source_id, "article-2");
    assert!(page.records[1].deleted);
    assert_eq!(
        page.records[1].source_updated_at,
        Some(ts("2026-06-26T12:31:00Z"))
    );
}

#[tokio::test]
async fn linked_account_synced_webhook_verifies_signature_and_rejects_bad_signature() {
    // Pins: Merge linked-account synced webhooks are trusted only after signature verification, and signed linked account IDs are preserved for local binding.
    let signature_key = "merge-webhook-secret";
    let provider = MergeProvider::with_client(
        reqwest::Client::new(),
        "https://merge.invalid",
        "merge-test-key",
    )
    .with_webhook_signature_key(signature_key);
    let body = Bytes::from_static(
        br#"{"hook":{"id":"hook_1"},"event":"linked_account.synced","linked_account":{"id":"linked-account-123"}}"#,
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-merge-webhook-signature",
        HeaderValue::from_str(&signature(&body, signature_key))
            .expect("valid base64 signature header"),
    );

    let event = provider
        .verify_webhook(headers.clone(), body.clone())
        .await
        .expect("valid Merge webhook signature");
    assert_eq!(event.provider, "merge");
    assert_eq!(event.event_id, "hook_1");
    assert_eq!(event.event_type, "linked_account.synced");
    assert_eq!(event.metadata["linked_account"]["id"], "linked-account-123");

    headers.insert(
        "x-merge-webhook-signature",
        HeaderValue::from_static("AAAAAAAAAAAAAAAAAAAAAA"),
    );
    let error = provider
        .verify_webhook(headers, body)
        .await
        .expect_err("bad Merge signature should fail");
    assert!(matches!(
        error,
        Error::Provider { provider, message }
            if provider == "merge" && message.contains("signature verification failed")
    ));
}
