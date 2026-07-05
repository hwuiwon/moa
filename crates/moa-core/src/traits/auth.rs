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

use crate::TenantId;

/// Principal category resolved from an inbound credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityType {
    /// Human user.
    User,
    /// Tenant-local end-user contact.
    Contact,
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
    /// Tenant runtime boundary for this request.
    pub tenant_id: TenantId,
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
    /// Local first-party user login session token.
    UserSessionToken(String),
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

    /// Return whether callers must provide a credential before authentication runs.
    fn requires_credentials(&self) -> bool {
        true
    }

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
    /// Full action payload. Builtin local approvals require callers to include
    /// the request tenant under `_tenant_id` until the trait grows a first-class
    /// tenant field.
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

    fn assert_object_safe<T: ?Sized>() {}

    #[test]
    fn auth_traits_remain_object_safe() {
        // Pins: downstream crates can continue storing provider implementations
        // behind trait objects without asserting behavior on dummy providers.
        assert_object_safe::<dyn AuthProvider>();
        assert_object_safe::<dyn TokenVaultProvider>();
        assert_object_safe::<dyn AsyncAuthzProvider>();
    }
}
