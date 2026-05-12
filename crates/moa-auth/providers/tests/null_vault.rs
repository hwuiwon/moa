//! Tests for the null token vault provider.

use moa_auth_providers::NullTokenVaultProvider;
use moa_core::traits::{TokenVaultError, TokenVaultProvider};
use uuid::Uuid;

#[tokio::test]
async fn null_vault_get_token_returns_not_configured() {
    // Pins: deployments without a vault fail token retrieval explicitly.
    let provider = NullTokenVaultProvider;
    let result = provider.get_token(Uuid::from_u128(1), "github").await;
    let error = match result {
        Ok(_) => panic!("null vault token retrieval must fail"),
        Err(error) => error,
    };
    match error {
        TokenVaultError::NotConfigured => {}
        other => panic!("expected NotConfigured, got {other:?}"),
    }
}

#[tokio::test]
async fn null_vault_list_connections_returns_empty() {
    // Pins: informational connection listing is empty, not an error.
    let provider = NullTokenVaultProvider;
    let connections = provider
        .list_connections(Uuid::from_u128(2))
        .await
        .expect("null vault lists empty connections");
    assert_eq!(connections, Vec::<String>::new());
}
