//! Null token vault provider used by zero-dependency local deployments.

use async_trait::async_trait;
use moa_core::traits::{TokenVaultError, TokenVaultProvider, VaultToken};
use uuid::Uuid;

/// Token vault implementation that reports no configured vault.
pub struct NullTokenVaultProvider;

#[async_trait]
impl TokenVaultProvider for NullTokenVaultProvider {
    async fn get_token(
        &self,
        _user_id: Uuid,
        _connection_name: &str,
    ) -> Result<VaultToken, TokenVaultError> {
        Err(TokenVaultError::NotConfigured)
    }

    async fn list_connections(&self, _user_id: Uuid) -> Result<Vec<String>, TokenVaultError> {
        Ok(Vec::new())
    }

    fn name(&self) -> &'static str {
        "null"
    }
}
