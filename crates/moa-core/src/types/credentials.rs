//! Typed tenant credential persistence, reference, and resolution types.
//!
//! These types replace the old free-form `(service, scope)` credential address.
//! Two boundaries are encoded in the type system rather than in strings:
//!
//! - **Persistence identity** is `(tenant, owning connection, kind, version)`.
//!   It says *which stored secret* a row is, and nothing about who may read it.
//! - **Resolution context** carries the acting principal (or a closed service
//!   actor), the requested operation, and a replay-stable operation identity.
//!   It says *who is asking and why*, and is never persisted as part of the
//!   credential's address.
//!
//! Deployment-owned operator secrets (Email/SMS transport credentials) have no
//! tenant connection, so they are a distinct [`CredentialSource`] variant rather
//! than a tenant credential with a synthetic connection.

use std::fmt;

use chrono::{DateTime, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::identifiers::TenantId;

/// Kind of credential material stored for one tenant connection.
///
/// The kind is part of the persistence identity, so a caller holding a
/// reference to an API-key credential cannot resolve it as OAuth material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// Long-lived provider account key or token (for example Merge or Nango).
    ProviderApiKey,
    /// OAuth access/refresh material brokered for a tenant connection.
    OAuth,
}

impl CredentialKind {
    /// Returns the stable storage/audit name for this kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderApiKey => "provider_api_key",
            Self::OAuth => "oauth",
        }
    }

    /// Parses a stored kind name, rejecting anything outside the closed set.
    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        match value {
            "provider_api_key" => Some(Self::ProviderApiKey),
            "oauth" => Some(Self::OAuth),
            _ => None,
        }
    }
}

/// Persistence identity of a credential *series* for one tenant connection.
///
/// A series accumulates versions through rotation; the active version is the
/// one resolution returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CredentialIdentity {
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Owning knowledge connection this credential belongs to.
    pub connection_uid: Uuid,
    /// Material kind stored under this identity.
    pub kind: CredentialKind,
}

/// Opaque durable handle to one exact stored credential version.
///
/// This is what is persisted in `KnowledgeConnection`, Restate state, events,
/// and API payloads. It carries no secret material and no parseable address:
/// callers cannot construct a reference to another tenant's credential by
/// string manipulation, and resolution re-validates the full identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CredentialRef(Uuid);

impl CredentialRef {
    /// Wraps a durable credential-version identifier.
    #[must_use]
    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Returns the durable credential-version identifier.
    #[must_use]
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for CredentialRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Deployment-owned operator secret that has no tenant connection.
///
/// A closed set: transport credentials belong to the deployment, not to a
/// tenant, so they never acquire a tenant credential row and can never be
/// reached through a tenant credential reference.
///
/// A few entries — the Twilio account SID, API key SID, sender number, and
/// messaging service SID — are operator *identifiers* rather than secrets. They
/// are carried in the same closed set because they come from the same
/// deployment-owned source and are only ever consumed together with the secret
/// they authenticate; uniform redaction is the safe default, whereas splitting
/// them across two sources reintroduces exactly the free-form addressing this
/// type replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentSecret {
    /// Postmark server token used for outbound email delivery.
    PostmarkServerToken,
    /// Twilio account SID used for outbound SMS delivery.
    TwilioAccountSid,
    /// Twilio auth token used for outbound SMS delivery.
    TwilioAuthToken,
    /// Twilio API key SID used instead of the account auth token.
    TwilioApiKeySid,
    /// Twilio API key secret paired with [`DeploymentSecret::TwilioApiKeySid`].
    TwilioApiKeySecret,
    /// Default Twilio sender phone number for outbound SMS.
    TwilioFromNumber,
    /// Default Twilio messaging service SID for outbound SMS.
    TwilioMessagingServiceSid,
}

impl DeploymentSecret {
    /// Returns the stable audit name for this deployment secret.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PostmarkServerToken => "postmark_server_token",
            Self::TwilioAccountSid => "twilio_account_sid",
            Self::TwilioAuthToken => "twilio_auth_token",
            Self::TwilioApiKeySid => "twilio_api_key_sid",
            Self::TwilioApiKeySecret => "twilio_api_key_secret",
            Self::TwilioFromNumber => "twilio_from_number",
            Self::TwilioMessagingServiceSid => "twilio_messaging_service_sid",
        }
    }
}

/// The deployment's operator-secret source.
///
/// This is the counterpart of the tenant credential vault for material that has
/// no tenant connection. It is a closed, in-memory set built once during runtime
/// composition: there is no lookup by name, so a caller cannot reach a value
/// that the deployment did not explicitly register, and an unset value is a
/// typed [`CredentialError::DeploymentSecretMissing`] rather than an empty
/// string that reaches a provider.
#[derive(Default)]
pub struct DeploymentSecrets {
    values: std::collections::HashMap<DeploymentSecret, SecretString>,
}

impl DeploymentSecrets {
    /// Creates an empty deployment secret set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one deployment secret, ignoring absent and blank values.
    ///
    /// A blank value is treated as unset so a present-but-empty deployment
    /// variable cannot make a provider look configured.
    #[must_use]
    pub fn with(mut self, secret: DeploymentSecret, value: Option<String>) -> Self {
        if let Some(value) = value
            && !value.trim().is_empty()
        {
            self.values.insert(secret, SecretString::from(value));
        }
        self
    }

    /// Returns whether a deployment secret is configured.
    #[must_use]
    pub fn contains(&self, secret: DeploymentSecret) -> bool {
        self.values.contains_key(&secret)
    }

    /// Resolves one deployment secret for an authorized outbound request.
    pub fn resolve(&self, secret: DeploymentSecret) -> Result<RedactedSecret, CredentialError> {
        self.values
            .get(&secret)
            .map(|value| RedactedSecret::new(value.expose_secret().to_string()))
            .ok_or(CredentialError::DeploymentSecretMissing)
    }

    /// Resolves one deployment secret, returning `None` when it is unset.
    ///
    /// Used for genuinely optional transport settings, where absence selects a
    /// different provider configuration rather than failing the send.
    #[must_use]
    pub fn resolve_optional(&self, secret: DeploymentSecret) -> Option<RedactedSecret> {
        self.values
            .get(&secret)
            .map(|value| RedactedSecret::new(value.expose_secret().to_string()))
    }
}

impl fmt::Debug for DeploymentSecrets {
    /// Renders only which secrets are configured, never any value.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut configured: Vec<&'static str> =
            self.values.keys().map(|secret| secret.as_str()).collect();
        configured.sort_unstable();
        formatter
            .debug_struct("DeploymentSecrets")
            .field("configured", &configured)
            .finish()
    }
}

/// Where a secret being resolved comes from.
///
/// The two variants are the two ownership boundaries in the system; encoding
/// them as one string was what previously allowed a tenant lookup to silently
/// fall back to a deployment credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "source")]
pub enum CredentialSource {
    /// A tenant-owned credential version stored in the vault.
    TenantConnection {
        /// Opaque handle to the exact stored version.
        reference: CredentialRef,
    },
    /// A deployment-owned operator secret with no tenant connection.
    Deployment {
        /// Which operator secret is requested.
        secret: DeploymentSecret,
    },
}

/// Operation being performed against the credential owner.
///
/// There is deliberately no enumeration/list operation: a caller-selected
/// tenant enumeration surface cannot be authorized meaningfully and was removed
/// rather than left as an unaudited exception.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialOperation {
    /// Store the first version of a new credential series.
    Create,
    /// Read the active version's plaintext for an authorized outbound request.
    Resolve,
    /// Store a new active version, superseding the current one.
    Rotate,
    /// Mark a version unusable without deleting its audit history.
    Revoke,
    /// Remove credential state as part of tenant lifecycle.
    Delete,
}

impl CredentialOperation {
    /// Returns the stable audit name for this operation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Resolve => "resolve",
            Self::Rotate => "rotate",
            Self::Revoke => "revoke",
            Self::Delete => "delete",
        }
    }
}

/// Durable service actor permitted to resolve credentials without a caller.
///
/// A closed allowlist, each entry bound to one exact tenant/connection-scoped
/// operation. This exists so durable workflows can resolve their own
/// connection's credential after reconstruction; it is not a general service
/// bypass and cannot be widened by a caller-supplied string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialServiceActor {
    /// The durable knowledge sync workflow listing provider records.
    KnowledgeSyncListing,
    /// Trusted content fetch for an already-listed knowledge record.
    KnowledgeContentFetch,
    /// The durable tenant-purge workflow removing a tenant's credential state.
    ///
    /// Tenant purge has no caller to attribute — it is a lifecycle operation on
    /// the tenant itself, admitted and authorized at the edge — so it acts as
    /// its own actor rather than borrowing a credential owner's identity.
    TenantLifecyclePurge,
}

impl CredentialServiceActor {
    /// Returns the stable audit name for this service actor.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KnowledgeSyncListing => "knowledge_sync_listing",
            Self::KnowledgeContentFetch => "knowledge_content_fetch",
            Self::TenantLifecyclePurge => "tenant_lifecycle_purge",
        }
    }

    /// Returns whether this actor may perform `operation`.
    ///
    /// Each actor is bound to exactly one operation: the knowledge actors read
    /// and never write, and the purge actor deletes and can never resolve
    /// material. There is no actor that can do both.
    #[must_use]
    pub fn permits(self, operation: CredentialOperation) -> bool {
        match self {
            Self::KnowledgeSyncListing | Self::KnowledgeContentFetch => {
                matches!(operation, CredentialOperation::Resolve)
            }
            Self::TenantLifecyclePurge => matches!(operation, CredentialOperation::Delete),
        }
    }
}

/// Principal on whose authority a credential operation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "principal")]
pub enum CredentialPrincipal {
    /// A caller-facing identity that passed tenant/operation authorization.
    ///
    /// `delegated_by` records the owner that delegated authority when the
    /// acting identity is not itself the recorded owner. This is authorization
    /// metadata, not privacy-subject ownership.
    Caller {
        /// Identity performing the operation.
        identity_id: Uuid,
        /// Owner that delegated authority, when acting under delegation.
        delegated_by: Option<Uuid>,
    },
    /// A durable service actor from the closed allowlist.
    Service {
        /// Which service actor is acting.
        actor: CredentialServiceActor,
    },
}

impl CredentialPrincipal {
    /// Returns the owner identity a create operation should stamp, if any.
    #[must_use]
    pub fn owner_identity(self) -> Option<Uuid> {
        match self {
            Self::Caller { identity_id, .. } => Some(identity_id),
            Self::Service { .. } => None,
        }
    }

    /// Returns whether this principal may perform `operation` at all.
    ///
    /// This is the principal-shape gate only; tenant and connection
    /// authorization happen before the vault is called.
    #[must_use]
    pub fn permits(self, operation: CredentialOperation) -> bool {
        match self {
            Self::Caller { .. } => true,
            Self::Service { actor } => actor.permits(operation),
        }
    }
}

/// Replay-stable context for one credential operation.
///
/// `operation_id` is supplied by the caller and is stable across retries of the
/// same logical operation; `request_hash` is a canonical hash of the operation's
/// selector and inputs. Together they make every operation replay-safe: the same
/// pair replays one audit row, while the same id with a different hash is a
/// typed conflict rather than a silent overwrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialContext {
    /// Tenant whose credential state is being operated on.
    pub tenant_id: TenantId,
    /// Principal on whose authority the operation runs.
    pub principal: CredentialPrincipal,
    /// Operation being performed.
    pub operation: CredentialOperation,
    /// Caller-supplied, replay-stable operation identifier.
    pub operation_id: String,
    /// Canonical hash of the operation's selector and inputs.
    pub request_hash: String,
}

/// Non-secret description of a stored credential version.
///
/// Returned by create/rotate so callers can persist the opaque reference and
/// record the version without ever seeing material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialVersion {
    /// Opaque handle to this exact version.
    pub reference: CredentialRef,
    /// Persistence identity this version belongs to.
    pub identity: CredentialIdentity,
    /// Monotonic version number within the series, starting at 1.
    pub version: i64,
    /// Whether this version is the active one.
    pub active: bool,
    /// Whether this version has been revoked.
    pub revoked: bool,
    /// When this version was created.
    pub created_at: DateTime<Utc>,
}

/// Plaintext secret handed to an authorized outbound request.
///
/// Deliberately not `Clone`, not `Serialize`, and not `Display`. Its `Debug`
/// renders a fixed redaction, so the plaintext cannot reach a log line, a model
/// payload, an event, or a persisted row by accident. The only way to read it is
/// [`RedactedSecret::expose_for_outbound_request`], whose name is intended to be
/// conspicuous at the call site.
pub struct RedactedSecret {
    plaintext: SecretString,
}

impl RedactedSecret {
    /// Wraps plaintext material for immediate outbound use.
    ///
    /// The inner [`SecretString`] zeroizes on drop, so the plaintext does not
    /// linger in freed heap memory after the request completes.
    #[must_use]
    pub fn new(plaintext: String) -> Self {
        Self {
            plaintext: SecretString::from(plaintext),
        }
    }

    /// Exposes the plaintext for one authorized outbound request.
    ///
    /// Call this as late as possible — ideally inline in the request builder —
    /// and never bind the result to a longer-lived variable.
    #[must_use]
    pub fn expose_for_outbound_request(&self) -> &str {
        self.plaintext.expose_secret()
    }
}

impl fmt::Debug for RedactedSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RedactedSecret(<redacted>)")
    }
}

/// Typed failure of a credential operation.
///
/// Every variant is safe to surface: none carries plaintext, ciphertext, token
/// fragments, request payloads, or provider error bodies.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    /// No credential exists for the supplied reference or identity.
    #[error("credential not found")]
    NotFound,
    /// The referenced version exists but has been revoked.
    #[error("credential version is revoked")]
    Revoked,
    /// The referenced version is no longer the active one.
    #[error("credential version is stale")]
    StaleVersion,
    /// The reference does not belong to the tenant in the resolution context.
    #[error("credential does not belong to the requesting tenant")]
    WrongTenant,
    /// The reference does not belong to the expected owning connection.
    #[error("credential does not belong to the expected connection")]
    WrongConnection,
    /// The reference exists but stores a different material kind.
    #[error("credential is not of the requested kind")]
    WrongKind,
    /// The principal may not perform this operation.
    #[error("principal is not authorized for this credential operation")]
    Unauthorized,
    /// The operation id was reused with a different selector or operation.
    #[error("credential operation id was reused with different inputs")]
    IdempotencyConflict,
    /// A concurrent writer advanced the series past the expected version.
    #[error("credential version changed concurrently")]
    VersionConflict,
    /// The deployment secret is not configured.
    #[error("deployment secret is not configured")]
    DeploymentSecretMissing,
    /// Storage, encryption, or key-management failure.
    #[error("credential storage failure: {0}")]
    Storage(String),
}

#[cfg(test)]
mod tests;
