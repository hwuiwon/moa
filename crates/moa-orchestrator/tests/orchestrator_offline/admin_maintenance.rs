//! Admin maintenance service helper coverage.

use chrono::Utc;
use moa_core::traits::IdentityType;
use moa_core::wire::admin::CheckpointRollbackResponse;
use moa_core::{types::identifiers::TenantId, types::session::CheckpointHandle};
use moa_memory_vector::PromotionReport;
use moa_orchestrator::ctx::RequestHeaders;
use moa_orchestrator::services::admin_maintenance::{
    authorize_platform_maintenance, platform_maintenance_identity, promotion_response_from_report,
    promotion_update_response,
};
use restate_sdk::prelude::{HandlerError, HeaderMap};
use uuid::Uuid;

struct TestHeaders(HeaderMap);

impl RequestHeaders for TestHeaders {
    fn request_headers(&self) -> &HeaderMap {
        &self.0
    }
}

#[test]
fn promotion_report_maps_to_wire_response() {
    // Pins: hosted vector promotion preserves copied count, validation overlap, backend, and dual-read metadata.
    let tenant_id = TenantId::from(uuid::Uuid::from_u128(1));
    let response = promotion_response_from_report(
        tenant_id,
        PromotionReport {
            storage_partition_id: moa_core::types::identifiers::StoragePartitionId::for_tenant(
                tenant_id,
            )
            .to_string(),
            copied: 42,
            validation_overlap: 0.981,
            vector_backend: "turbopuffer".to_string(),
            vector_backend_state: "dual_read".to_string(),
        },
        Some(24),
    );

    assert_eq!(response.tenant_id, tenant_id);
    assert_eq!(response.copied_vectors, 42);
    assert_eq!(response.validation_overlap, 0.981);
    assert_eq!(response.vector_backend, "turbopuffer");
    assert_eq!(response.vector_backend_state, "dual_read");
    assert_eq!(response.dual_read_hours, Some(24));
}

#[test]
fn promotion_update_response_marks_steady_state() {
    // Pins: rollback/finalize responses report a steady backend state without pretending vectors were copied.
    let tenant_id = TenantId::from(uuid::Uuid::from_u128(1));
    let response = promotion_update_response(tenant_id, "pgvector", "steady");

    assert_eq!(response.tenant_id, tenant_id);
    assert_eq!(response.copied_vectors, 0);
    assert_eq!(response.validation_overlap, 1.0);
    assert_eq!(response.vector_backend, "pgvector");
    assert_eq!(response.vector_backend_state, "steady");
    assert_eq!(response.dual_read_hours, None);
}

#[tokio::test]
async fn tenant_scoped_identities_cannot_authorize_checkpoint_maintenance() {
    // Pins: global checkpoint create/list/rollback/cleanup fail closed before tenant-admin authz.
    let cases = [
        ("checkpoint_create", IdentityType::Operator, None),
        ("checkpoint_list", IdentityType::Agent, None),
        ("checkpoint_rollback", IdentityType::Contact, None),
        (
            "checkpoint_cleanup",
            IdentityType::Operator,
            Some(Uuid::from_u128(9001)),
        ),
    ];

    for (operation, identity_type, api_key_id) in cases {
        let headers = headers(identity_type, api_key_id);

        let error = authorize_platform_maintenance(&headers)
            .await
            .expect_err("tenant-scoped checkpoint maintenance should be forbidden");

        assert_eq!(
            handler_error_text(error),
            "Terminal error [403]: platform maintenance requires service workspace admin",
            "{operation} should reject tenant-scoped identity before maintenance work"
        );
    }
}

#[test]
fn service_identity_passes_platform_maintenance_identity_gate() {
    // Pins: service callers are the only identity type admitted to the workspace-admin check.
    let headers = headers(IdentityType::Service, None);

    let identity =
        platform_maintenance_identity(&headers).expect("service identity should reach FGA check");

    assert_eq!(identity.identity_type, IdentityType::Service);
    assert_eq!(identity.api_key_id, None);
}

#[test]
fn api_key_identity_cannot_authorize_checkpoint_maintenance() {
    // Pins: API-key authenticated callers cannot become platform maintenance principals.
    let headers = headers(IdentityType::Service, Some(Uuid::from_u128(9010)));

    let error = platform_maintenance_identity(&headers)
        .expect_err("API-key identity should not pass platform maintenance gate");

    assert_eq!(
        handler_error_text(error),
        "Terminal error [403]: platform maintenance requires service workspace admin"
    );
}

#[test]
fn checkpoint_rollback_response_does_not_carry_database_url_field() {
    // Pins: rollback responses do not duplicate the raw database URL as a top-level API field.
    let response = CheckpointRollbackResponse {
        handle: CheckpointHandle {
            id: "br-checkpoint".to_string(),
            label: "before-deploy".to_string(),
            connection_url: "postgres://checkpoint.example/moa".to_string(),
            created_at: Utc::now(),
            session_id: None,
        },
    };

    let value =
        serde_json::to_value(response).expect("checkpoint rollback response should serialize");

    assert_eq!(value.get("database_url"), None);
}

fn headers(identity_type: IdentityType, api_key_id: Option<Uuid>) -> TestHeaders {
    let mut headers = HeaderMap::with_capacity(5);
    headers.insert(
        "x-moa-identity-type",
        identity_type_header(identity_type).to_string(),
    );
    headers.insert("x-moa-identity-id", Uuid::from_u128(100).to_string());
    headers.insert("x-moa-tenant-id", Uuid::from_u128(1_000).to_string());
    if let Some(api_key_id) = api_key_id {
        headers.insert("x-moa-api-key-id", api_key_id.to_string());
    }
    TestHeaders(headers)
}

fn identity_type_header(identity_type: IdentityType) -> &'static str {
    identity_type.as_str()
}

fn handler_error_text(error: HandlerError) -> String {
    let error_ref = <HandlerError as AsRef<dyn std::error::Error + Send + Sync>>::as_ref(&error);
    error_ref.to_string()
}
