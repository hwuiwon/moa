//! Public tenant-knowledge route and webhook translation.

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, Uri};
use base64::{Engine as _, engine::general_purpose};
use moa_core::TenantId;
use moa_core::wire::knowledge::{
    KnowledgeConnectionListRequest, KnowledgeCreateLinkTokenRequest, KnowledgeExchangeTokenRequest,
    KnowledgeIntegrationListRequest, KnowledgeObjectInspectRequest, KnowledgeObjectListRequest,
    KnowledgeProviderWebhookRequest, KnowledgeQueryTraceRequest, KnowledgeSyncEventsRequest,
    KnowledgeSyncRequest, KnowledgeSyncStatusRequest,
    KnowledgeUpdateConnectionSourceSelectionRequest,
};

use super::{
    KNOWLEDGE_WEBHOOK_BODY_LIMIT_BYTES, RouteTranslation, forwarded_webhook_headers,
    rejects_raw_webhook_content_type, translate_knowledge_json_body, webhook_event_id,
    webhook_event_type,
};

pub(super) fn translate(
    method: &Method,
    uri: &Uri,
    body: &Bytes,
    tenant_id: TenantId,
) -> Option<RouteTranslation> {
    if *method != Method::POST {
        return None;
    }
    let translation = match uri.path() {
        "/v1/knowledge/link-token" => translate_knowledge_json_body::<
            KnowledgeCreateLinkTokenRequest,
        >(body, "/Knowledge/create_link_token", tenant_id),
        "/v1/knowledge/exchange-token" => translate_knowledge_json_body::<
            KnowledgeExchangeTokenRequest,
        >(
            body, "/Knowledge/exchange_public_token", tenant_id
        ),
        "/v1/knowledge/sync" => translate_knowledge_json_body::<KnowledgeSyncRequest>(
            body,
            "/Knowledge/sync_connection",
            tenant_id,
        ),
        "/v1/knowledge/sync-status" => translate_knowledge_json_body::<KnowledgeSyncStatusRequest>(
            body,
            "/Knowledge/sync_status",
            tenant_id,
        ),
        "/v1/knowledge/sync-events" => translate_knowledge_json_body::<KnowledgeSyncEventsRequest>(
            body,
            "/Knowledge/sync_events",
            tenant_id,
        ),
        "/v1/knowledge/connections" => translate_knowledge_json_body::<
            KnowledgeConnectionListRequest,
        >(body, "/Knowledge/list_connections", tenant_id),
        "/v1/knowledge/integrations" => translate_knowledge_json_body::<
            KnowledgeIntegrationListRequest,
        >(body, "/Knowledge/list_integrations", tenant_id),
        "/v1/knowledge/connections/source-selection" => {
            translate_knowledge_json_body::<KnowledgeUpdateConnectionSourceSelectionRequest>(
                body,
                "/Knowledge/update_connection_source_selection",
                tenant_id,
            )
        }
        "/v1/knowledge/objects" => translate_knowledge_json_body::<KnowledgeObjectListRequest>(
            body,
            "/Knowledge/list_objects",
            tenant_id,
        ),
        "/v1/knowledge/object" => translate_knowledge_json_body::<KnowledgeObjectInspectRequest>(
            body,
            "/Knowledge/inspect_object",
            tenant_id,
        ),
        "/v1/knowledge/query-trace" => translate_knowledge_json_body::<KnowledgeQueryTraceRequest>(
            body,
            "/Knowledge/query_trace",
            tenant_id,
        ),
        _ => return None,
    };
    Some(translation)
}

pub(super) fn translate_provider_webhook(
    provider: &'static str,
    headers: &HeaderMap,
    body: &Bytes,
) -> RouteTranslation {
    if body.len() > KNOWLEDGE_WEBHOOK_BODY_LIMIT_BYTES {
        return RouteTranslation::BadRequest("knowledge webhook body too large");
    }
    if rejects_raw_webhook_content_type(headers) {
        return RouteTranslation::BadRequest(
            "knowledge webhook route does not accept raw document uploads",
        );
    }

    let payload: serde_json::Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(_) => return RouteTranslation::BadRequest("bad knowledge webhook body"),
    };
    if !payload.is_object() {
        return RouteTranslation::BadRequest("knowledge webhook body must be object");
    }
    let Some(event_id) = webhook_event_id(headers, &payload) else {
        return RouteTranslation::BadRequest("knowledge webhook missing event id");
    };
    let Some(event_type) = webhook_event_type(headers, &payload) else {
        return RouteTranslation::BadRequest("knowledge webhook missing event type");
    };

    let request = KnowledgeProviderWebhookRequest {
        provider: provider.to_string(),
        event_id,
        event_type,
        payload,
        headers: forwarded_webhook_headers(headers),
        body_base64: Some(general_purpose::STANDARD.encode(body)),
    };
    let bytes = match serde_json::to_vec(&request) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(error = %error, provider, "serialize knowledge webhook body failed");
            return RouteTranslation::BadRequest("bad knowledge webhook body");
        }
    };
    RouteTranslation::Forward {
        method: Method::POST,
        path: "/Knowledge/provider_webhook".to_string(),
        body: bytes,
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Bytes;
    use axum::http::{HeaderMap, Method, Uri, header};
    use base64::{Engine as _, engine::general_purpose};
    use moa_core::wire::knowledge::KnowledgeProviderWebhookRequest;

    use crate::routes::test_support::{TEST_TENANT_ID, test_tenant_json, translate};
    use crate::routes::{KNOWLEDGE_WEBHOOK_BODY_LIMIT_BYTES, RouteTranslation};

    #[test]
    fn knowledge_public_routes_translate_to_restate_handlers() {
        // Pins: hosted knowledge routes forward typed DTOs with tenant id derived from auth.
        let connection_uid = "11111111-1111-1111-1111-111111111111";
        let sync_run_uid = "22222222-2222-2222-2222-222222222222";
        let object_uid = "33333333-3333-3333-3333-333333333333";
        let trace_uid = "44444444-4444-4444-4444-444444444444";
        let caller_tenant = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let cases = [
            (
                "/v1/knowledge/link-token",
                "/Knowledge/create_link_token",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "provider": "nango",
                    "connector": "google-drive",
                    "external_account_id": "account-1"
                }),
            ),
            (
                "/v1/knowledge/exchange-token",
                "/Knowledge/exchange_public_token",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "provider": "merge",
                    "exchange_token": "public-token"
                }),
            ),
            (
                "/v1/knowledge/sync",
                "/Knowledge/sync_connection",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "connection_uid": connection_uid,
                    "parser": "native",
                    "max_records": 25
                }),
            ),
            (
                "/v1/knowledge/sync-status",
                "/Knowledge/sync_status",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "sync_run_uid": sync_run_uid
                }),
            ),
            (
                "/v1/knowledge/sync-events",
                "/Knowledge/sync_events",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "sync_run_uid": sync_run_uid,
                    "object_uid": object_uid,
                    "cursor": "page-2",
                    "limit": 50
                }),
            ),
            (
                "/v1/knowledge/integrations",
                "/Knowledge/list_integrations",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "provider": "nango"
                }),
            ),
            (
                "/v1/knowledge/connections",
                "/Knowledge/list_connections",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "provider": "nango"
                }),
            ),
            (
                "/v1/knowledge/connections/source-selection",
                "/Knowledge/update_connection_source_selection",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "connection_uid": connection_uid,
                    "source_selection": {
                        "nango": {
                            "metadata": {
                                "selected_folder_ids": ["folder-1"]
                            }
                        }
                    },
                    "sync": true
                }),
            ),
            (
                "/v1/knowledge/objects",
                "/Knowledge/list_objects",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "connection_uid": connection_uid,
                    "object_type": "document",
                    "cursor": "page-2",
                    "limit": 25
                }),
            ),
            (
                "/v1/knowledge/object",
                "/Knowledge/inspect_object",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "object_uid": object_uid
                }),
            ),
            (
                "/v1/knowledge/query-trace",
                "/Knowledge/query_trace",
                serde_json::json!({
                    "tenant_id": test_tenant_json(),
                    "trace_uid": trace_uid
                }),
            ),
        ];

        for (public_path, internal_path, expected_body) in cases {
            let uri = public_path.parse::<Uri>().expect("route path should parse");
            let mut input_body = expected_body.clone();
            let object = input_body.as_object_mut().expect("expected body is object");
            object.insert("tenant_id".to_string(), serde_json::json!(caller_tenant));
            let body = Bytes::from(input_body.to_string());

            let translation = translate(&Method::POST, &uri, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{public_path} must remain POST");
                    assert_eq!(path, internal_path, "{public_path} target changed");
                    let forwarded: serde_json::Value =
                        serde_json::from_slice(&forwarded_body).expect("forwarded body is JSON");
                    assert_eq!(forwarded, expected_body, "{public_path} body changed");
                }
                RouteTranslation::NoChange => {
                    panic!("{public_path} should translate to {internal_path}")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{public_path} should not fail translation: {message}")
                }
            }
        }
    }

    #[test]
    fn knowledge_public_routes_reject_bad_typed_payloads() {
        // Pins: knowledge route translation validates against the typed wire DTO before forwarding.
        let uri = "/v1/knowledge/sync"
            .parse::<Uri>()
            .expect("route path should parse");
        let missing_connection = Bytes::from_static(br#"{"parser":"native"}"#);
        let non_object = Bytes::from_static(br#"[]"#);

        match translate(&Method::POST, &uri, &missing_connection) {
            RouteTranslation::BadRequest(message) => {
                assert_eq!(message, "bad knowledge route body");
            }
            RouteTranslation::Forward { .. } => panic!("missing connection_uid must not forward"),
            RouteTranslation::NoChange => panic!("knowledge sync route should not fall through"),
        }

        match translate(&Method::POST, &uri, &non_object) {
            RouteTranslation::BadRequest(message) => {
                assert_eq!(message, "knowledge route body must be object");
            }
            RouteTranslation::Forward { .. } => panic!("non-object payload must not forward"),
            RouteTranslation::NoChange => panic!("knowledge sync route should not fall through"),
        }
    }

    #[test]
    fn knowledge_provider_webhooks_translate_without_tenant_injection() {
        // Pins: provider webhooks bypass end-user auth and forward raw signed event material only.
        let body = Bytes::from_static(br#"{"event_id":"evt-1","event_type":"sync.completed"}"#);
        let providers = ["llamaparse", "reducto", "nango", "merge"];

        for provider in providers {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                "application/json"
                    .parse()
                    .expect("content-type should parse"),
            );
            headers.insert(
                header::AUTHORIZATION,
                "Bearer should-not-forward"
                    .parse()
                    .expect("authorization header should parse"),
            );
            headers.insert(
                "x-moa-tenant-id",
                TEST_TENANT_ID.parse().expect("tenant header should parse"),
            );
            headers.insert(
                "x-test-signature",
                "valid".parse().expect("signature header should parse"),
            );

            let translation = super::translate_provider_webhook(provider, &headers, &body);

            match translation {
                RouteTranslation::Forward {
                    method,
                    path,
                    body: forwarded_body,
                } => {
                    assert_eq!(method, Method::POST, "{provider} must remain POST");
                    assert_eq!(path, "/Knowledge/provider_webhook");
                    let forwarded: KnowledgeProviderWebhookRequest =
                        serde_json::from_slice(&forwarded_body)
                            .expect("forwarded webhook body should decode");
                    assert_eq!(forwarded.provider, provider);
                    assert_eq!(forwarded.event_id, "evt-1");
                    assert_eq!(forwarded.event_type, "sync.completed");
                    assert_eq!(forwarded.payload.get("tenant_id"), None);
                    assert!(
                        forwarded.body_base64.is_some(),
                        "raw body should be base64 encoded"
                    );
                    let raw_body = general_purpose::STANDARD
                        .decode(forwarded.body_base64.as_deref().expect("body_base64"))
                        .expect("body_base64 should decode");
                    assert_eq!(raw_body, body.as_ref());
                    assert!(
                        !forwarded
                            .headers
                            .iter()
                            .any(|(name, _)| name.eq_ignore_ascii_case("authorization")),
                        "authorization must not be forwarded for webhook verification"
                    );
                    assert!(
                        !forwarded
                            .headers
                            .iter()
                            .any(|(name, _)| name.eq_ignore_ascii_case("x-moa-tenant-id")),
                        "caller-supplied MOA headers must not be forwarded"
                    );
                    assert!(
                        forwarded
                            .headers
                            .iter()
                            .any(|(name, value)| name == "x-test-signature" && value == "valid"),
                        "provider signature header should be forwarded"
                    );
                }
                RouteTranslation::NoChange => {
                    panic!("{provider} webhook should translate to Knowledge/provider_webhook")
                }
                RouteTranslation::BadRequest(message) => {
                    panic!("{provider} webhook should not fail translation: {message}")
                }
            }
        }
    }

    #[test]
    fn knowledge_provider_webhooks_reject_documents_and_malformed_events() {
        // Pins: webhook routes accept provider event JSON, not direct document uploads.
        let mut raw_document_headers = HeaderMap::new();
        raw_document_headers.insert(
            header::CONTENT_TYPE,
            "application/pdf"
                .parse()
                .expect("content-type should parse"),
        );
        let document_body = Bytes::from_static(b"%PDF-raw-document");
        match super::translate_provider_webhook("llamaparse", &raw_document_headers, &document_body)
        {
            RouteTranslation::BadRequest(message) => {
                assert_eq!(
                    message,
                    "knowledge webhook route does not accept raw document uploads"
                );
            }
            RouteTranslation::Forward { .. } => panic!("raw document webhook must not forward"),
            RouteTranslation::NoChange => panic!("webhook route should not fall through"),
        }

        let mut json_headers = HeaderMap::new();
        json_headers.insert(
            header::CONTENT_TYPE,
            "application/json"
                .parse()
                .expect("content-type should parse"),
        );
        let missing_event_type = Bytes::from_static(br#"{"event_id":"evt-1"}"#);
        match super::translate_provider_webhook("nango", &json_headers, &missing_event_type) {
            RouteTranslation::BadRequest(message) => {
                assert_eq!(message, "knowledge webhook missing event type");
            }
            RouteTranslation::Forward { .. } => panic!("malformed webhook must not forward"),
            RouteTranslation::NoChange => panic!("webhook route should not fall through"),
        }

        let large_body = Bytes::from(vec![b' '; KNOWLEDGE_WEBHOOK_BODY_LIMIT_BYTES + 1]);
        match super::translate_provider_webhook("merge", &json_headers, &large_body) {
            RouteTranslation::BadRequest(message) => {
                assert_eq!(message, "knowledge webhook body too large");
            }
            RouteTranslation::Forward { .. } => panic!("oversized webhook must not forward"),
            RouteTranslation::NoChange => panic!("webhook route should not fall through"),
        }
    }
}
