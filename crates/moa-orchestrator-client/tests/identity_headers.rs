//! Tests for client-side trusted identity header propagation.

use moa_core::WorkspaceId;
use moa_core::traits::{Identity, IdentityType};
use moa_orchestrator_client::OrchestratorClient;
use mockito::{Matcher, Server};
use uuid::Uuid;

#[tokio::test]
async fn with_identity_attaches_all_identity_headers() {
    // Pins: client requests carry the same trusted identity header contract as moa-edge.
    let identity_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
        .expect("identity UUID fixture parses");
    let tenant_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222")
        .expect("tenant UUID fixture parses");
    let api_key_id = Uuid::parse_str("33333333-3333-3333-3333-333333333333")
        .expect("api key UUID fixture parses");
    let acting_user_id = Uuid::parse_str("44444444-4444-4444-4444-444444444444")
        .expect("acting user UUID fixture parses");
    let mut server = Server::new_async().await;
    let mock = server
        .mock("POST", "/ToolExecutor/list_tools")
        .match_header("x-moa-identity-type", "agent")
        .match_header("x-moa-identity-id", identity_id.to_string().as_str())
        .match_header("x-moa-tenant-id", tenant_id.to_string().as_str())
        .match_header("x-moa-api-key-id", api_key_id.to_string().as_str())
        .match_header(
            "x-moa-acting-on-behalf-of",
            acting_user_id.to_string().as_str(),
        )
        .match_body(Matcher::Exact("\"workspace-identity-test\"".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create_async()
        .await;

    let identity = Identity {
        identity_type: IdentityType::Agent,
        id: identity_id,
        tenant_id,
        api_key_id: Some(api_key_id),
        acting_on_behalf_of: Some(acting_user_id),
    };
    let client = OrchestratorClient::new(server.url())
        .expect("mock endpoint should parse")
        .with_identity(identity);

    let tool_names = client
        .tool_names(WorkspaceId::new("workspace-identity-test"))
        .await
        .expect("mock response should decode");

    assert_eq!(tool_names, Vec::<String>::new());
    mock.assert_async().await;
}
