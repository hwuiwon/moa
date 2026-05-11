//! Authentication, token-vault, and async-authorization trait surfaces.
//!
//! These traits live in `moa-core::traits` so downstream crates depend on the
//! contracts without pulling in any provider implementation dependencies.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

/// Principal category resolved from an inbound credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityType {
    /// Human user.
    User,
    /// AI agent principal.
    Agent,
    /// Internal service principal.
    Service,
}

/// Authenticated caller identity propagated through MOA.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Principal category.
    pub identity_type: IdentityType,
    /// Principal UUID.
    pub id: Uuid,
    /// Tenant UUID for this request.
    pub tenant_id: Uuid,
    /// API key UUID when authentication used a local API key.
    pub api_key_id: Option<Uuid>,
    /// User UUID when an agent is acting on behalf of a user.
    pub acting_on_behalf_of: Option<Uuid>,
}

/// Raw credential presented to an authentication provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// Local MOA API key.
    ApiKey(String),
    /// Bearer JWT from Auth0 or a generic OIDC provider.
    BearerJwt(String),
}

/// Authentication provider failures.
#[derive(Debug, Error)]
pub enum AuthError {
    /// Credential syntax was invalid before provider verification.
    #[error("invalid credential format")]
    InvalidFormat,
    /// Provider rejected the credential.
    #[error("credential rejected by provider")]
    Rejected,
    /// Credential was valid but expired.
    #[error("credential expired")]
    Expired,
    /// Provider could not be reached or returned a transient error.
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    /// Provider is not configured for this credential type.
    #[error("provider not configured for this credential type")]
    NotConfigured,
    /// Provider returned an internal error.
    #[error("internal: {0}")]
    Internal(String),
}

/// Resolves inbound credentials to MOA identities.
#[async_trait]
pub trait AuthProvider: Send + Sync {
    /// Resolve a credential to an identity.
    async fn authenticate(&self, credential: &Credential) -> Result<Identity, AuthError>;

    /// Return a short stable provider name for logs and metrics.
    fn name(&self) -> &'static str;
}

/// Third-party access token returned by a token-vault provider.
#[derive(Clone)]
pub struct VaultToken {
    /// Access token secret.
    pub access_token: SecretString,
    /// Optional token expiration.
    pub expires_at: Option<DateTime<Utc>>,
    /// Provider scopes associated with the token.
    pub scopes: Vec<String>,
}

/// Token-vault provider failures.
#[derive(Debug, Error)]
pub enum TokenVaultError {
    /// No token vault is configured.
    #[error("vault not configured")]
    NotConfigured,
    /// The requested user has not linked the connection.
    #[error("connection not linked for this user")]
    NotLinked,
    /// Vault could not be reached or returned a transient error.
    #[error("vault unavailable: {0}")]
    Unavailable(String),
    /// Vault returned an internal error.
    #[error("internal: {0}")]
    Internal(String),
}

/// Retrieves third-party OAuth tokens for user-owned connections.
#[async_trait]
pub trait TokenVaultProvider: Send + Sync {
    /// Retrieve a current access token for `(user_id, connection_name)`.
    async fn get_token(
        &self,
        user_id: Uuid,
        connection_name: &str,
    ) -> Result<VaultToken, TokenVaultError>;

    /// List linked connection names for a user.
    async fn list_connections(&self, user_id: Uuid) -> Result<Vec<String>, TokenVaultError>;

    /// Return a short stable provider name for logs and metrics.
    fn name(&self) -> &'static str;
}

/// Request to initiate an async human approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Session UUID waiting on the approval.
    pub session_id: Uuid,
    /// User UUID whose decision resolves the approval.
    pub deciding_user_id: Uuid,
    /// One-line action summary for approval surfaces.
    pub action_summary: String,
    /// Full action payload.
    pub action_details: serde_json::Value,
    /// Restate awakeable ID to resolve when the decision arrives.
    pub awakeable_id: String,
    /// Approval timeout.
    pub timeout: Duration,
}

/// Provider-specific handle returned for an async approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalHandle {
    /// Approval UUID.
    pub id: Uuid,
    /// Restate awakeable ID associated with the approval.
    pub awakeable_id: String,
    /// Provider metadata, such as an Auth0 CIBA `auth_req_id`.
    #[serde(default)]
    pub provider_specific: serde_json::Value,
}

/// Final decision for an async approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum ApprovalDecision {
    /// The action was approved.
    Approved,
    /// The action was denied.
    Denied {
        /// Optional denial reason.
        reason: Option<String>,
    },
    /// The approval timed out.
    Timeout,
}

/// Async-authorization provider failures.
#[derive(Debug, Error)]
pub enum AsyncAuthzError {
    /// No async-authorization provider is configured.
    #[error("not configured")]
    NotConfigured,
    /// Provider could not be reached or returned a transient error.
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    /// Provider returned an internal error.
    #[error("internal: {0}")]
    Internal(String),
}

/// Starts and tracks human-in-the-loop approval requests.
#[async_trait]
pub trait AsyncAuthzProvider: Send + Sync {
    /// Initiate an out-of-band approval without blocking the workflow.
    async fn request_approval(
        &self,
        request: ApprovalRequest,
    ) -> Result<ApprovalHandle, AsyncAuthzError>;

    /// Poll a provider-backed approval handle for a decision.
    async fn poll_decision(
        &self,
        _handle: &ApprovalHandle,
    ) -> Result<Option<ApprovalDecision>, AsyncAuthzError> {
        Ok(None)
    }

    /// Return a short stable provider name for logs and metrics.
    fn name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyAuthProvider;

    #[async_trait]
    impl AuthProvider for DummyAuthProvider {
        async fn authenticate(&self, _credential: &Credential) -> Result<Identity, AuthError> {
            Ok(Identity {
                identity_type: IdentityType::User,
                id: Uuid::from_u128(1),
                tenant_id: Uuid::from_u128(2),
                api_key_id: Some(Uuid::from_u128(3)),
                acting_on_behalf_of: None,
            })
        }

        fn name(&self) -> &'static str {
            "dummy-auth"
        }
    }

    struct DummyTokenVaultProvider;

    #[async_trait]
    impl TokenVaultProvider for DummyTokenVaultProvider {
        async fn get_token(
            &self,
            _user_id: Uuid,
            _connection_name: &str,
        ) -> Result<VaultToken, TokenVaultError> {
            Err(TokenVaultError::NotConfigured)
        }

        async fn list_connections(&self, _user_id: Uuid) -> Result<Vec<String>, TokenVaultError> {
            Ok(vec!["github".to_string(), "drive".to_string()])
        }

        fn name(&self) -> &'static str {
            "dummy-vault"
        }
    }

    struct DummyAsyncAuthzProvider;

    #[async_trait]
    impl AsyncAuthzProvider for DummyAsyncAuthzProvider {
        async fn request_approval(
            &self,
            request: ApprovalRequest,
        ) -> Result<ApprovalHandle, AsyncAuthzError> {
            Ok(ApprovalHandle {
                id: Uuid::from_u128(4),
                awakeable_id: request.awakeable_id,
                provider_specific: serde_json::json!({"provider": "dummy"}),
            })
        }

        fn name(&self) -> &'static str {
            "dummy-async-authz"
        }
    }

    #[tokio::test]
    async fn auth_provider_trait_object_authenticates_identity() {
        // Pins: AuthProvider remains object-safe and returns the resolved identity contract.
        let provider: Box<dyn AuthProvider> = Box::new(DummyAuthProvider);
        let identity = provider
            .authenticate(&Credential::ApiKey("moa_dev_example".to_string()))
            .await
            .expect("dummy auth provider should resolve identity");

        assert_eq!(provider.name(), "dummy-auth");
        assert_eq!(identity.identity_type, IdentityType::User);
        assert_eq!(identity.id, Uuid::from_u128(1));
        assert_eq!(identity.tenant_id, Uuid::from_u128(2));
        assert_eq!(identity.api_key_id, Some(Uuid::from_u128(3)));
        assert_eq!(identity.acting_on_behalf_of, None);
    }

    #[tokio::test]
    async fn token_vault_provider_trait_object_lists_connections() {
        // Pins: TokenVaultProvider remains object-safe for downstream crates.
        let provider: Box<dyn TokenVaultProvider> = Box::new(DummyTokenVaultProvider);
        let connections = provider
            .list_connections(Uuid::from_u128(1))
            .await
            .expect("dummy vault should list connections");

        assert_eq!(provider.name(), "dummy-vault");
        assert_eq!(connections, vec!["github".to_string(), "drive".to_string()]);
    }

    #[tokio::test]
    async fn async_authz_provider_trait_object_returns_handle() {
        // Pins: AsyncAuthzProvider remains object-safe and preserves awakeable IDs.
        let provider: Box<dyn AsyncAuthzProvider> = Box::new(DummyAsyncAuthzProvider);
        let handle = provider
            .request_approval(ApprovalRequest {
                session_id: Uuid::from_u128(1),
                deciding_user_id: Uuid::from_u128(2),
                action_summary: "run command".to_string(),
                action_details: serde_json::json!({"tool": "bash"}),
                awakeable_id: "awakeable-1".to_string(),
                timeout: Duration::from_secs(30),
            })
            .await
            .expect("dummy async authz should return handle");

        assert_eq!(provider.name(), "dummy-async-authz");
        assert_eq!(handle.id, Uuid::from_u128(4));
        assert_eq!(handle.awakeable_id, "awakeable-1");
        assert_eq!(
            handle.provider_specific,
            serde_json::json!({"provider": "dummy"})
        );
    }
}
