//! Contact identity types for agent-facing end users.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{SessionId, UserId, WorkspaceId};

/// Prefix used for contact-backed user-scope memory subjects.
pub const CONTACT_USER_ID_PREFIX: &str = "contact:";

uuid_id!(
    /// Identifier for an agent-facing contact.
    pub struct ContactId
);

impl ContactId {
    /// Returns the user-scope memory id for this contact.
    #[must_use]
    pub fn as_user_id(self) -> UserId {
        UserId::new(format!("{CONTACT_USER_ID_PREFIX}{}", self.0))
    }

    /// Parses a user-scope memory id that was created from a contact id.
    #[must_use]
    pub fn from_user_id(user_id: &UserId) -> Option<Self> {
        user_id
            .as_str()
            .strip_prefix(CONTACT_USER_ID_PREFIX)
            .and_then(|value| Uuid::parse_str(value).ok())
            .map(Self)
    }
}

uuid_id!(
    /// Identifier for one normalized contact point.
    pub struct ContactPointId
);

uuid_id!(
    /// Identifier for a contact verification challenge.
    pub struct ContactVerificationChallengeId
);

/// Assurance state for an agent-facing contact.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ContactVerificationState {
    /// Contact has no verified or unverified identifier yet.
    Anonymous,
    /// Contact has a provided identifier but it has not been verified.
    Unverified,
    /// Contact has verified ownership of at least one contact point.
    Verified,
    /// Contact has been linked to a canonical verified contact.
    Merged,
}

impl ContactVerificationState {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    /// Returns whether the contact state is verified for high-assurance scopes.
    #[must_use]
    pub fn is_verified(self) -> bool {
        matches!(self, Self::Verified)
    }
}

/// Supported contact-point categories.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ContactPointKind {
    /// Email address contact point.
    Email,
    /// Phone number contact point.
    Phone,
    /// Customer-system stable external identifier.
    ExternalId,
    /// Anonymous browser or device handle.
    AnonymousHandle,
}

impl ContactPointKind {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// Supported delivery channel for contact-owned messages.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ContactDeliveryChannel {
    /// Email delivery through the configured email provider.
    Email,
    /// SMS delivery through the configured SMS provider.
    Sms,
}

impl ContactDeliveryChannel {
    /// Returns the stable API representation.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// Contact-point value supplied by an integration or verification workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPointInput {
    /// Contact-point category.
    pub kind: ContactPointKind,
    /// Raw contact-point value before MOA normalization.
    pub value: String,
    /// Optional display-safe label to retain with the point.
    #[serde(default)]
    pub display_value: Option<String>,
}

/// Persisted contact-point projection safe to expose to callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPointRef {
    /// Contact point identifier.
    pub id: ContactPointId,
    /// Contact-point category.
    pub kind: ContactPointKind,
    /// Optional display-safe label.
    #[serde(default)]
    pub display_value: Option<String>,
    /// Whether ownership has been verified.
    pub verified: bool,
    /// Time the contact point was verified.
    #[serde(default)]
    pub verified_at: Option<DateTime<Utc>>,
}

/// Contact projection attached to sessions and contact tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactRef {
    /// Contact identifier used by the agent runtime.
    pub contact_id: ContactId,
    /// Tenant/account boundary that owns the contact.
    pub tenant_id: Uuid,
    /// Workspace the contact belongs to.
    pub workspace_id: WorkspaceId,
    /// Assurance state for this contact.
    pub state: ContactVerificationState,
    /// Canonical verified contact when this contact was promoted.
    #[serde(default)]
    pub canonical_contact_id: Option<ContactId>,
    /// Linked anonymous or unverified contacts whose memory may be read.
    #[serde(default)]
    pub linked_contact_ids: Vec<ContactId>,
    /// Bounded scopes granted to this contact token or session.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Structured route/data permissions granted to this contact token.
    #[serde(default)]
    pub permissions: Value,
    /// Optional allowlist of agent ids the contact token may address.
    #[serde(default)]
    pub agent_ids: Vec<String>,
    /// Optional allowlist of session ids the contact token may continue.
    #[serde(default)]
    pub session_ids: Vec<SessionId>,
    /// Verified contact points represented by the current token.
    #[serde(default)]
    pub verified_contact_point_ids: Vec<ContactPointId>,
}

/// Principal that created or owns a session at the API boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionActorRef {
    /// Workspace-admin-or-higher MOA identity.
    Identity {
        /// Authenticated MOA identity UUID.
        id: Uuid,
    },
    /// Agent-facing contact identity.
    Contact {
        /// Contact identifier.
        id: ContactId,
    },
    /// Public anonymous caller before contact materialization.
    Anonymous,
}

/// JWT claims carried by MOA-issued contact tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactTokenClaims {
    /// Issuer.
    pub iss: String,
    /// Audience.
    pub aud: String,
    /// Subject contact id.
    pub sub: String,
    /// Expiration timestamp as seconds since epoch.
    pub exp: i64,
    /// Issued-at timestamp as seconds since epoch.
    pub iat: i64,
    /// Not-before timestamp as seconds since epoch.
    pub nbf: i64,
    /// Token id for audit and future revocation.
    pub jti: String,
    /// Tenant/account boundary the token is bounded to.
    pub tenant_id: Uuid,
    /// Workspace the token is bounded to.
    pub workspace_id: WorkspaceId,
    /// Contact assurance state at issuance.
    pub state: ContactVerificationState,
    /// Bounded contact scopes.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Structured route/data permissions for bounded contact access.
    #[serde(default)]
    pub permissions: Value,
    /// Optional allowlist of agent ids the token may address.
    #[serde(default)]
    pub agent_ids: Vec<String>,
    /// Optional allowlist of session ids the token may continue.
    #[serde(default)]
    pub session_ids: Vec<SessionId>,
    /// Verified contact points covered by this token.
    #[serde(default)]
    pub verified_contact_point_ids: Vec<ContactPointId>,
    /// Linked contact ids included for default promoted-memory retrieval.
    #[serde(default)]
    pub linked_contact_ids: Vec<ContactId>,
}

/// Request to issue an unverified or anonymous contact token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactTokenIssueRequest {
    /// Workspace in which the contact may interact with agents.
    pub workspace_id: WorkspaceId,
    /// Optional contact points to attach in an unverified state.
    #[serde(default)]
    pub contact_points: Vec<ContactPointInput>,
    /// Optional integration-provided display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Optional contact profile supplied by the authorized integration.
    #[serde(default)]
    pub profile: Value,
    /// Optional contact metadata supplied by the authorized integration.
    #[serde(default)]
    pub metadata: Value,
    /// Optional requested low-assurance scopes.
    #[serde(default)]
    pub requested_scopes: Vec<String>,
    /// Structured route/data permissions requested for the contact token.
    #[serde(default)]
    pub permissions: Value,
    /// Optional allowlist of agent ids the issued token may address.
    #[serde(default)]
    pub agent_ids: Vec<String>,
}

/// Response returned after contact token issuance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactTokenIssueResponse {
    /// Contact projection represented by the token.
    pub contact: ContactRef,
    /// Contact points attached during issuance.
    #[serde(default)]
    pub contact_points: Vec<ContactPointRef>,
    /// Signed MOA contact JWT.
    pub token: String,
    /// Token expiration timestamp.
    pub expires_at: DateTime<Utc>,
    /// Scopes granted in the returned token.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Structured permissions granted in the returned token.
    #[serde(default)]
    pub permissions: Value,
}

/// Request to start ownership verification for a contact point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactVerificationStartRequest {
    /// Workspace asserted by the public route.
    pub workspace_id: WorkspaceId,
    /// Optional session that triggered the verification workflow.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// Current contact token.
    pub contact_token: String,
    /// Optional explicit delivery channel. Defaults to email for email points and SMS for phone points.
    #[serde(default)]
    pub delivery_channel: Option<ContactDeliveryChannel>,
    /// Contact point to verify.
    pub contact_point: ContactPointInput,
}

/// Response returned after creating a verification challenge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactVerificationStartResponse {
    /// Verification challenge identifier.
    pub challenge_id: ContactVerificationChallengeId,
    /// Contact point being verified.
    pub contact_point: ContactPointRef,
    /// Delivery channel used for the verification code.
    pub delivery_channel: ContactDeliveryChannel,
    /// Challenge expiration timestamp.
    pub expires_at: DateTime<Utc>,
}

/// Request to complete a contact verification challenge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactVerificationCompleteRequest {
    /// Workspace asserted by the public route.
    pub workspace_id: WorkspaceId,
    /// Optional session that triggered the verification completion.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// Current contact token.
    pub contact_token: String,
    /// Verification challenge identifier.
    pub challenge_id: ContactVerificationChallengeId,
    /// One-time verification code.
    pub code: String,
}

/// Response returned after a contact is promoted to verified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactVerificationCompleteResponse {
    /// Canonical verified contact.
    pub contact: ContactRef,
    /// Upgraded signed MOA contact JWT.
    pub token: String,
    /// Token expiration timestamp.
    pub expires_at: DateTime<Utc>,
}

/// Request to initialize an agent session for a contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionInitRequest {
    /// Workspace asserted by the public route.
    pub workspace_id: WorkspaceId,
    /// Current contact token.
    pub contact_token: String,
    /// Optional session title.
    #[serde(default)]
    pub title: Option<String>,
    /// Optional platform channel.
    #[serde(default)]
    pub platform_channel: Option<String>,
    /// Model identifier for the session.
    pub model: String,
}

/// Response returned after initializing a contact session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionInitResponse {
    /// Created session id.
    pub session_id: SessionId,
    /// Contact attached to the session.
    pub contact: ContactRef,
}

/// Request to promote an active session after contact verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionPromotionRequest {
    /// Workspace asserted by the public route.
    pub workspace_id: WorkspaceId,
    /// Session to promote.
    pub session_id: SessionId,
    /// Upgraded verified contact token.
    pub contact_token: String,
}

/// Response returned after session contact promotion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionPromotionResponse {
    /// Promoted session id.
    pub session_id: SessionId,
    /// Contact now attached to the session.
    pub contact: ContactRef,
}
