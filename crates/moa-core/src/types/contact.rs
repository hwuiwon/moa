//! Contact identity types for agent-facing end users.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{
    agent::AgentSessionSelection, channel::Attachment, channel::Channel,
    channel::ChannelAccountRef, channel::ChannelRef, events_stream::EventRange,
    identifiers::SessionId, identifiers::TenantId,
};

/// Maximum UTF-8 byte length for one contact-authored session message.
pub const MAX_CONTACT_SESSION_MESSAGE_TEXT_BYTES: usize = 64 * 1024;

/// Maximum number of attachments admitted with one contact-authored session message.
pub const MAX_CONTACT_SESSION_ATTACHMENTS_PER_MESSAGE: usize = 4;

/// Maximum bytes for one contact-authored session attachment.
pub const MAX_CONTACT_SESSION_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;

/// Maximum total attachment bytes for one contact-authored session message.
pub const MAX_CONTACT_SESSION_ATTACHMENT_TOTAL_BYTES: usize = 10 * 1024 * 1024;

/// Maximum UTF-8 byte length for one contact-authored attachment display name.
pub const MAX_CONTACT_SESSION_ATTACHMENT_NAME_BYTES: usize = 255;

uuid_id!(
    /// Identifier for an agent-facing contact.
    pub struct ContactId
);

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
    pub tenant_id: TenantId,
    /// Assurance state for this contact.
    pub state: ContactVerificationState,
    /// Canonical verified contact when this contact was promoted.
    #[serde(default)]
    pub canonical_contact_id: Option<ContactId>,
    /// Linked anonymous or unverified contacts available to explicit promotion operations.
    #[serde(default)]
    pub linked_contact_ids: Vec<ContactId>,
    /// Bounded scopes granted to this contact token or session.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Structured route/data permissions granted to this contact token.
    #[serde(default)]
    pub permissions: Value,
    /// Explicit allowlist of agent ids the contact token may address.
    ///
    /// Empty token allowlists cannot create contact sessions.
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
    /// Tenant-admin-or-higher MOA identity.
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
    pub tenant_id: TenantId,
    /// Contact assurance state at issuance.
    pub state: ContactVerificationState,
    /// Bounded contact scopes.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Structured route/data permissions for bounded contact access.
    #[serde(default)]
    pub permissions: Value,
    /// Explicit allowlist of agent ids the token may address.
    ///
    /// Empty token allowlists cannot create contact sessions.
    #[serde(default)]
    pub agent_ids: Vec<String>,
    /// Optional allowlist of session ids the token may continue.
    #[serde(default)]
    pub session_ids: Vec<SessionId>,
    /// Verified contact points covered by this token.
    #[serde(default)]
    pub verified_contact_point_ids: Vec<ContactPointId>,
    /// Linked contact ids included for explicit promotion metadata, not memory inheritance.
    #[serde(default)]
    pub linked_contact_ids: Vec<ContactId>,
}

/// Request to issue an unverified or anonymous contact token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactTokenIssueRequest {
    /// Tenant in which the contact may interact with agents.
    pub tenant_id: TenantId,
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
    /// Explicit requested low-assurance scopes.
    ///
    /// Token issuance rejects an empty list.
    #[serde(default)]
    pub requested_scopes: Vec<String>,
    /// Structured route/data permissions requested for the contact token.
    #[serde(default)]
    pub permissions: Value,
    /// Explicit allowlist of agent ids the issued token may address.
    ///
    /// Token issuance rejects an empty list.
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
    /// Tenant asserted by the public route.
    pub tenant_id: TenantId,
    /// Optional session that triggered the verification workflow.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// Current contact token.
    pub contact_token: String,
    /// Optional explicit delivery channel. Defaults to email for email points and SMS for phone points.
    #[serde(default)]
    pub delivery_channel: Option<Channel>,
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
    pub delivery_channel: Channel,
    /// Challenge expiration timestamp.
    pub expires_at: DateTime<Utc>,
}

/// Request to complete a contact verification challenge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactVerificationCompleteRequest {
    /// Tenant asserted by the public route.
    pub tenant_id: TenantId,
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

/// Requested channel route for a contact session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionChannelRequest {
    /// Route reference for the active channel.
    pub channel_ref: ChannelRef,
    /// Optional caller-supplied reason for choosing this route.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Request to initialize an agent session for a contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionInitRequest {
    /// Tenant asserted by the public route.
    pub tenant_id: TenantId,
    /// Current contact token.
    pub contact_token: String,
    /// Optional session title.
    #[serde(default)]
    pub title: Option<String>,
    /// Initial delivery channel and route.
    pub channel: ContactSessionChannelRequest,
    /// Model identifier for the session.
    pub model: String,
    /// Installed deployment or exact revision to pin onto the created session.
    pub agent: AgentSessionSelection,
}

/// Response returned after initializing a contact session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionInitResponse {
    /// Created session id.
    pub session_id: SessionId,
    /// Contact attached to the session.
    pub contact: ContactRef,
}

/// Request to change a contact session's active communication channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionChannelChangeRequest {
    /// Tenant asserted by the public route.
    pub tenant_id: TenantId,
    /// Session whose active channel should change.
    pub session_id: SessionId,
    /// Current contact token.
    pub contact_token: String,
    /// New active route reference.
    pub channel_ref: ChannelRef,
    /// Optional caller-supplied reason for the route change.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Response returned after changing a contact session's active channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionChannelChangeResponse {
    /// Session whose channel changed.
    pub session_id: SessionId,
    /// Contact attached to the session.
    pub contact: ContactRef,
    /// Active route reference after the change.
    pub channel_ref: ChannelRef,
    /// Channel account used by the route, when applicable.
    #[serde(default)]
    pub channel_account: Option<ChannelAccountRef>,
}

/// Request to send one user message to an existing contact-owned session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionMessageRequest {
    /// Tenant asserted by the public route.
    pub tenant_id: TenantId,
    /// Session receiving the user message.
    pub session_id: SessionId,
    /// Current contact token.
    pub contact_token: String,
    /// User message text to enqueue or start immediately.
    #[serde(default)]
    pub user_message: String,
    /// Attachments included with the user message.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Optional per-turn model override.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional turn-iteration cap for this request.
    #[serde(default)]
    pub max_turns: Option<u32>,
}

impl ContactSessionMessageRequest {
    /// Validates size and presence invariants for an admitted contact session message.
    pub fn validate_admitted_payload(&self) -> std::result::Result<(), &'static str> {
        validate_contact_session_message_text(&self.user_message)?;
        if self.attachments.len() > MAX_CONTACT_SESSION_ATTACHMENTS_PER_MESSAGE {
            return Err("too many message attachments");
        }
        validate_contact_session_attachments(&self.attachments)?;
        if self.user_message.trim().is_empty() && self.attachments.is_empty() {
            return Err("contact session message requires text or an attachment");
        }
        Ok(())
    }
}

/// Validates only the text portion of a contact-authored session message.
pub fn validate_contact_session_message_text(text: &str) -> std::result::Result<(), &'static str> {
    if text.len() > MAX_CONTACT_SESSION_MESSAGE_TEXT_BYTES {
        return Err("session message text is too long");
    }
    Ok(())
}

/// Returns the canonical MIME type for a contact-session photo upload.
#[must_use]
pub fn normalize_contact_session_photo_mime(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/png" => Some("image/png"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

fn validate_contact_session_attachments(
    attachments: &[Attachment],
) -> std::result::Result<(), &'static str> {
    let mut total_bytes = 0_usize;
    for attachment in attachments {
        if attachment.id.is_none() {
            return Err("message attachment is not stored");
        }
        if attachment.name.is_empty()
            || attachment.name.len() > MAX_CONTACT_SESSION_ATTACHMENT_NAME_BYTES
        {
            return Err("message attachment name is invalid");
        }
        if attachment.name.chars().any(char::is_control) {
            return Err("message attachment name is invalid");
        }
        if attachment.path.is_some() {
            return Err("message attachment path is not allowed");
        }
        if attachment.url.as_deref().is_none_or(str::is_empty) {
            return Err("message attachment URL is required");
        }
        let Some(mime_type) = attachment.mime_type.as_deref() else {
            return Err("message attachment MIME type is required");
        };
        if normalize_contact_session_photo_mime(mime_type).is_none() {
            return Err("only photo attachments are supported");
        }
        let Some(size_bytes) = attachment.size_bytes else {
            return Err("message attachment size is required");
        };
        let size_bytes =
            usize::try_from(size_bytes).map_err(|_| "message attachment is too large")?;
        if size_bytes > MAX_CONTACT_SESSION_ATTACHMENT_BYTES {
            return Err("message attachment is too large");
        }
        total_bytes = total_bytes.saturating_add(size_bytes);
        if total_bytes > MAX_CONTACT_SESSION_ATTACHMENT_TOTAL_BYTES {
            return Err("message attachments are too large");
        }
        let Some(sha256) = attachment.sha256.as_deref() else {
            return Err("message attachment digest is required");
        };
        if !is_sha256_hex_digest(sha256) {
            return Err("message attachment digest is invalid");
        }
    }
    Ok(())
}

fn is_sha256_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Response returned after admitting a contact session message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionMessageResponse {
    /// Session that accepted the message.
    pub session_id: SessionId,
    /// Whether the message was queued behind an active turn.
    pub queued: bool,
    /// Turn ID when the message started a workflow immediately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_turn_id: Option<String>,
}

/// Request to authorize access to a contact-owned session without loading progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionAuthorizationRequest {
    /// Tenant asserted by the public route.
    pub tenant_id: TenantId,
    /// Session being accessed.
    pub session_id: SessionId,
    /// Current contact token.
    pub contact_token: String,
}

/// Response returned after authorizing a contact-owned session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionAuthorizationResponse {
    /// Session the contact token can access.
    pub session_id: SessionId,
    /// Contact bound to the session.
    pub contact: ContactRef,
}

/// Request to read progress for a contact-owned session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionProgressRequest {
    /// Tenant asserted by the public route.
    pub tenant_id: TenantId,
    /// Session being observed.
    pub session_id: SessionId,
    /// Current contact token.
    pub contact_token: String,
    /// Event range to include alongside hot workflow progress.
    #[serde(default)]
    pub event_range: EventRange,
}

/// Request to promote an active session after contact verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSessionPromotionRequest {
    /// Tenant asserted by the public route.
    pub tenant_id: TenantId,
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use uuid::Uuid;

    use super::{
        ChannelRef, ContactSessionChannelRequest, ContactSessionMessageRequest,
        MAX_CONTACT_SESSION_ATTACHMENT_BYTES,
    };
    use crate::types::{
        channel::Attachment, identifiers::SessionAttachmentId, identifiers::SessionId,
        identifiers::TenantId,
    };

    #[test]
    fn contact_session_channel_request_supports_chat_delivery_channel() {
        // Pins: API ingress must carry a real delivery channel, not an API channel.
        let request = ContactSessionChannelRequest {
            channel_ref: ChannelRef::Chat {
                conversation_id: "chat-123".to_string(),
                user_id: Some("user-123".to_string()),
                client_session_id: Some("client-123".to_string()),
            },
            reason: None,
        };

        assert_eq!(request.channel_ref.channel(), super::Channel::Chat);
    }

    #[test]
    fn contact_session_message_rejects_oversized_text() {
        // Pins: every service path enforces the public contact message text limit.
        let mut request = base_message_request();
        request.user_message = "x".repeat(super::MAX_CONTACT_SESSION_MESSAGE_TEXT_BYTES + 1);

        assert_eq!(
            request.validate_admitted_payload(),
            Err("session message text is too long")
        );
    }

    #[test]
    fn contact_session_message_accepts_stored_photo_only_body() {
        // Pins: photo-only contact messages are valid after edge storage produces durable metadata.
        let mut request = base_message_request();
        request.user_message.clear();
        request.attachments = vec![stored_photo_attachment()];

        assert_eq!(request.validate_admitted_payload(), Ok(()));
    }

    #[test]
    fn contact_session_message_rejects_unstored_attachment() {
        // Pins: callers cannot bypass edge byte validation with JSON-only attachment metadata.
        let mut request = base_message_request();
        let mut attachment = stored_photo_attachment();
        attachment.id = None;
        request.attachments = vec![attachment];

        assert_eq!(
            request.validate_admitted_payload(),
            Err("message attachment is not stored")
        );
    }

    #[test]
    fn contact_session_message_rejects_non_photo_attachment() {
        // Pins: contact messages only admit stored photo attachments until other file types have validators.
        let mut request = base_message_request();
        let mut attachment = stored_photo_attachment();
        attachment.mime_type = Some("application/zip".to_string());
        request.attachments = vec![attachment];

        assert_eq!(
            request.validate_admitted_payload(),
            Err("only photo attachments are supported")
        );
    }

    #[test]
    fn contact_session_message_rejects_unsafe_attachment_metadata() {
        // Pins: stored attachment metadata must remain bounded and digest-addressed.
        let mut request = base_message_request();
        let mut attachment = stored_photo_attachment();
        attachment.size_bytes = Some((MAX_CONTACT_SESSION_ATTACHMENT_BYTES + 1) as u64);
        request.attachments = vec![attachment];
        assert_eq!(
            request.validate_admitted_payload(),
            Err("message attachment is too large")
        );

        request.attachments = vec![stored_photo_attachment()];
        request.attachments[0].path = Some(PathBuf::from("/tmp/photo.png"));
        assert_eq!(
            request.validate_admitted_payload(),
            Err("message attachment path is not allowed")
        );

        request.attachments = vec![stored_photo_attachment()];
        request.attachments[0].sha256 = Some("ABC".to_string());
        assert_eq!(
            request.validate_admitted_payload(),
            Err("message attachment digest is invalid")
        );
    }

    fn base_message_request() -> ContactSessionMessageRequest {
        ContactSessionMessageRequest {
            tenant_id: TenantId(Uuid::nil()),
            session_id: SessionId(Uuid::nil()),
            contact_token: "token".to_string(),
            user_message: "hello".to_string(),
            attachments: Vec::new(),
            model: None,
            max_turns: None,
        }
    }

    fn stored_photo_attachment() -> Attachment {
        Attachment {
            id: Some(SessionAttachmentId(Uuid::nil())),
            name: "photo.png".to_string(),
            mime_type: Some("image/png".to_string()),
            sha256: Some("a".repeat(64)),
            url: Some("/v1/sessions/session/attachments/attachment".to_string()),
            path: None,
            size_bytes: Some(1024),
        }
    }
}
