//! Behavior tests for deployment MCP credential loading.

use moa_config::{McpCredentialConfig, McpServerConfig};

use super::McpDeploymentCredentials;

fn server(name: &str, credentials: Option<McpCredentialConfig>) -> McpServerConfig {
    McpServerConfig {
        required: false,
        discovery: moa_config::McpDiscoveryMode::Eager,
        name: name.to_string(),
        url: "http://127.0.0.1:1".to_string(),
        credentials,
        trust_tool_annotations: false,
        allowed_data_classes: Vec::new(),
    }
}

#[test]
fn bearer_and_api_key_credentials_render_the_configured_headers() {
    // Pins: deployment credential selectors are read once and shaped into the
    // exact header form configured for each MCP server.
    const BEARER_ENV: &str = "MOA_TEST_MCP_DEPLOYMENT_BEARER";
    const API_KEY_ENV: &str = "MOA_TEST_MCP_DEPLOYMENT_API_KEY";
    // SAFETY: each test-only variable name is unique within the suite.
    unsafe {
        std::env::set_var(BEARER_ENV, "bearer-secret");
        std::env::set_var(API_KEY_ENV, "api-secret");
    }
    let bearer = server(
        "search",
        Some(McpCredentialConfig::Bearer {
            token_env: BEARER_ENV.to_string(),
        }),
    );
    let api_key = server(
        "crm",
        Some(McpCredentialConfig::ApiKey {
            header: "X-Api-Key".to_string(),
            value_env: API_KEY_ENV.to_string(),
        }),
    );

    let credentials =
        McpDeploymentCredentials::from_mcp_servers(&[bearer.clone(), api_key.clone()])
            .expect("deployment credentials load");

    assert_eq!(
        credentials
            .headers_for(&bearer)
            .expect("bearer headers")
            .get("Authorization")
            .map(String::as_str),
        Some("Bearer bearer-secret")
    );
    assert_eq!(
        credentials
            .headers_for(&api_key)
            .expect("API key headers")
            .get("X-Api-Key")
            .map(String::as_str),
        Some("api-secret")
    );
}

#[test]
fn missing_configured_environment_variable_fails_closed() {
    // Pins: an MCP server that declares authentication never falls back to an
    // unauthenticated connection when its deployment secret is missing.
    const MISSING_ENV: &str = "MOA_TEST_MCP_INTENTIONALLY_MISSING";
    // SAFETY: each test-only variable name is unique within the suite.
    unsafe {
        std::env::remove_var(MISSING_ENV);
    }

    let error = McpDeploymentCredentials::from_mcp_servers(&[server(
        "search",
        Some(McpCredentialConfig::Bearer {
            token_env: MISSING_ENV.to_string(),
        }),
    )])
    .expect_err("missing secret must fail");

    assert!(error.to_string().contains(MISSING_ENV));
}

#[test]
fn credentialless_server_has_no_authentication_headers() {
    // Pins: omitting the credential configuration explicitly represents an
    // unauthenticated deployment connector.
    let server = server("public", None);
    let credentials = McpDeploymentCredentials::from_mcp_servers(std::slice::from_ref(&server))
        .expect("credentialless server loads");

    assert!(
        credentials
            .headers_for(&server)
            .expect("headers resolve")
            .is_empty()
    );
}
