//! Ignored end-to-end coverage for the local API-key authentication path.

use moa_auth_providers::api_keys::{self, CreateApiKeyRequest, CreateApiKeyResponse, Env};
use moa_authz::{FgaClient, FgaConfig};
use moa_authz_schema::{ObjectType, Relation, TupleKey, TupleOp, UserType};
use moa_core::TenantId;
use moa_core::traits::{Identity, IdentityType};
use uuid::Uuid;

/// Returns `true` when `name` is set to a common truthy value (`1`, `true`,
/// `yes`, or `on`, case-insensitively after trimming), matching how live-test
/// flags are written in a developer's `.env`.
fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[tokio::test]
#[ignore = "requires running stack: make dev and MOA_RUN_LIVE_LOCAL_AUTH_TESTS=1"]
async fn create_present_validate_revoke() -> Result<(), Box<dyn std::error::Error>> {
    if !env_flag_enabled("MOA_RUN_LIVE_LOCAL_AUTH_TESTS") {
        return Ok(());
    }

    let edge_url =
        std::env::var("MOA_EDGE_URL").unwrap_or_else(|_| "http://127.0.0.1:10000".to_string());
    let orchestrator_url = std::env::var("MOA_ORCHESTRATOR_ENDPOINT")
        .unwrap_or_else(|_| "http://127.0.0.1:10010".to_string());

    let user_id = Uuid::new_v4();
    let tenant_id = Uuid::new_v4();
    let fga = live_fga_client()?;
    let bootstrap_tuple = TupleKey::new(
        UserType::Operator,
        user_id,
        Relation::Admin,
        ObjectType::Tenant,
        tenant_id,
    );
    fga.apply(TupleOp::Write, &bootstrap_tuple).await?;

    let direct_identity = Identity {
        identity_type: IdentityType::Operator,
        id: user_id,
        tenant_id: TenantId::from(tenant_id),
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let client = reqwest::Client::new();
    let issued = post_orchestrator::<_, CreateApiKeyResponse>(
        &client,
        &orchestrator_url,
        "/ApiKeys/create",
        &direct_identity,
        &CreateApiKeyRequest {
            name: "live-local-auth-e2e".to_string(),
            description: Some("ignored integration test".to_string()),
            env: Env::Dev,
            for_agent_id: None,
        },
    )
    .await?;

    let whoami = client
        .get(format!("{edge_url}/v1/whoami"))
        .bearer_auth(&issued.key)
        .send()
        .await?;
    assert_eq!(whoami.status(), reqwest::StatusCode::OK);
    let identity = whoami.json::<Identity>().await?;
    assert_eq!(identity.identity_type, IdentityType::Operator);
    assert_eq!(identity.id, user_id);
    assert_eq!(identity.tenant_id, TenantId::from(tenant_id));
    assert_eq!(identity.api_key_id, Some(issued.id));

    post_orchestrator_void(
        &client,
        &orchestrator_url,
        "/ApiKeys/revoke",
        &direct_identity,
        &issued.id,
    )
    .await?;

    let revoked = client
        .get(format!("{edge_url}/v1/whoami"))
        .bearer_auth(&issued.key)
        .send()
        .await?;
    assert_eq!(revoked.status(), reqwest::StatusCode::UNAUTHORIZED);

    fga.apply(TupleOp::Delete, &bootstrap_tuple).await?;

    Ok(())
}

#[test]
fn github_secret_scanning_regex_is_public_contract() {
    // Pins: partner registration regex stays exactly aligned with docs.
    assert_eq!(
        api_keys::GITHUB_SECRET_SCANNING_REGEX,
        r"moa_(live|prod|stg|dev)_[A-Za-z0-9]{32}_[a-f0-9]{8}"
    );
}

fn live_fga_client() -> Result<FgaClient, Box<dyn std::error::Error>> {
    Ok(FgaClient::new(FgaConfig {
        url: std::env::var("MOA_AUTHZ_OPENFGA_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:10030".to_string()),
        preshared_key: std::env::var("MOA_AUTHZ_OPENFGA_PRESHARED_KEY")
            .unwrap_or_else(|_| "localdev-preshared-key-do-not-use-in-prod".to_string()),
        store_id: std::env::var("MOA_AUTHZ_OPENFGA_STORE_ID").expect(
            "MOA_AUTHZ_OPENFGA_STORE_ID must be set when live local auth tests are enabled",
        ),
        model_id: std::env::var("MOA_AUTHZ_OPENFGA_MODEL_ID").expect(
            "MOA_AUTHZ_OPENFGA_MODEL_ID must be set when live local auth tests are enabled",
        ),
        timeout_ms: 5000,
    })?)
}

async fn post_orchestrator<Request, Response>(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    identity: &Identity,
    body: &Request,
) -> Result<Response, Box<dyn std::error::Error>>
where
    Request: serde::Serialize + ?Sized,
    Response: serde::de::DeserializeOwned,
{
    let response = client
        .post(format!("{}{path}", base_url.trim_end_matches('/')))
        .header("x-moa-identity-type", "operator")
        .header("x-moa-identity-id", identity.id.to_string())
        .header("x-moa-tenant-id", identity.tenant_id.to_string())
        .json(body)
        .send()
        .await?;
    let status = response.status();
    let bytes = response.bytes().await?;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "orchestrator call {path} failed with body {}",
        String::from_utf8_lossy(&bytes)
    );
    Ok(serde_json::from_slice(&bytes)?)
}

async fn post_orchestrator_void<Request>(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    identity: &Identity,
    body: &Request,
) -> Result<(), Box<dyn std::error::Error>>
where
    Request: serde::Serialize + ?Sized,
{
    let response = client
        .post(format!("{}{path}", base_url.trim_end_matches('/')))
        .header("x-moa-identity-type", "operator")
        .header("x-moa-identity-id", identity.id.to_string())
        .header("x-moa-tenant-id", identity.tenant_id.to_string())
        .json(body)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "orchestrator call {path} failed with body {body}",
    );
    Ok(())
}
