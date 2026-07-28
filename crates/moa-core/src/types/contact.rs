//! Contact identity types for agent-facing end users.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{
    agent::AgentSessionSelection, channel::Attachment, channel::Channel,
    channel::ChannelAccountRef, channel::ChannelRef, events_stream::EventRange,
    events_stream::SequenceNum, identifiers::SessionAttachmentId, identifiers::SessionId,
    identifiers::TenantId, worker::state::WorkerId,
};
use crate::error::{MoaError, Result};

/// Maximum UTF-8 byte length for one contact-authored session message.
pub const MAX_CONTACT_SESSION_MESSAGE_TEXT_BYTES: usize = 64 * 1024;

/// Maximum UTF-8 byte length for one caller-supplied [`ClientMessageId`].
pub const MAX_CLIENT_MESSAGE_ID_BYTES: usize = 256;

/// UUIDv5 namespace for deterministic session-attachment slot identities.
///
/// Fixed for the lifetime of the slot contract: changing it re-addresses every
/// attachment slot and breaks upload replay detection.
const SESSION_ATTACHMENT_SLOT_NAMESPACE: Uuid =
    Uuid::from_u128(0x9f2b_7c1e_5d4a_4c8f_9b3e_7a1d_6c8e_4f20);

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

/// Caller-owned identity for one submitted session message.
///
/// Every message-submitting caller — REST client, Slack adapter, experiment, load
/// generator — mints this value itself, so a retry after a lost response carries the
/// same identity and is recognized as the same admission instead of duplicating
/// attachments, queue entries, reply deliveries, and paid turns. MOA never
/// synthesizes one: a submission without a valid id is a typed rejection.
///
/// The value is 1–[`MAX_CLIENT_MESSAGE_ID_BYTES`] UTF-8 bytes with control
/// characters rejected, which admits both opaque REST identifiers (a UUID) and
/// platform event identifiers (a Slack `Ev…` event id). It is compared
/// byte-exactly, so callers must not pad or re-case it between attempts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ClientMessageId(String);

impl ClientMessageId {
    /// Validates and wraps one caller-supplied message identity.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(MoaError::ValidationError(
                "client message id must not be empty".to_string(),
            ));
        }
        if value.len() > MAX_CLIENT_MESSAGE_ID_BYTES {
            return Err(MoaError::ValidationError(format!(
                "client message id must be at most {MAX_CLIENT_MESSAGE_ID_BYTES} bytes"
            )));
        }
        if value.chars().any(char::is_control) {
            return Err(MoaError::ValidationError(
                "client message id must not contain control characters".to_string(),
            ));
        }
        Ok(Self(value))
    }

    /// Derives the identity for one internally submitted message.
    ///
    /// Internal submitters — experiment and evaluation workflows, the load generator,
    /// test fixtures — have no client-supplied identity, so they derive one from stable
    /// durable coordinates: a scope literal, the owning durable id (run, trial,
    /// session), and the message's ordinal within it. Deriving from a clock or from
    /// randomness would mint a fresh identity on every Restate replay and silently
    /// disable the admission fence for exactly the callers that retry most.
    pub fn internal(scope: &str, coordinate: Uuid, ordinal: u64) -> Result<Self> {
        Self::new(format!("{scope}:{coordinate}:{ordinal}"))
    }

    /// Returns the exact caller-supplied identity.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ClientMessageId {
    type Error = MoaError;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl std::fmt::Display for ClientMessageId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Exact user-addressed target a submitted message explicitly replies to.
///
/// Carries only the caller-verifiable coordinates of a pending request for user
/// input. The approved budget and plan hash needed to act on a confirmation stay
/// server-side, so a caller can address a target without being able to restate the
/// terms it was shown. A target that no longer matches the session's pending
/// request is a typed conflict, never a silent ordinary turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum MessageReplyTarget {
    /// Reply approving or rejecting one admitted run's displayed plan and budget.
    ExecutionConfirmation {
        /// Durable execution-run identifier.
        run_uid: Uuid,
    },
    /// Reply delivering one user-addressed execution task's generation-fenced input.
    ExecutionInput {
        /// Durable execution-run identifier.
        run_uid: Uuid,
        /// Stable logical task identifier.
        task_id: Uuid,
        /// Expected task generation fence.
        generation: u64,
    },
    /// Reply answering one conversational worker's input request.
    ///
    /// Fenced by the worker turn and admission generation that raised the request:
    /// a reply naming a superseded owner addresses work that has already moved on
    /// and must conflict rather than resolve a replacement round-trip.
    WorkerInput {
        /// Durable worker identifier.
        worker_id: WorkerId,
        /// Worker turn that raised the request.
        turn_id: String,
        /// Worker admission generation that owns the raising turn.
        generation: u64,
        /// Exact worker input request identifier.
        input_request_id: String,
    },
    /// Reply answering one root coordinator turn's own input request.
    ///
    /// Distinct from `WorkerInput`: the coordinator is not a child, so it has no
    /// worker id, and its request is fenced by the turn generation that raised
    /// it. A reply naming a superseded generation addresses work that has already
    /// moved on and must conflict rather than be delivered.
    CoordinatorInput {
        /// Coordinator turn that raised the request.
        turn_id: String,
        /// Session turn generation that admitted the owning turn.
        generation: u64,
        /// Exact coordinator input request identifier.
        input_request_id: String,
    },
}

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

/// Deterministic storage slot identity for one attachment of one client message.
///
/// A retried upload of the same message addresses exactly the same slot, so the
/// attachment store can detect a replay and reject a slot whose content or metadata
/// changed instead of minting a second row and a second object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAttachmentSlot {
    /// Tenant that owns the session.
    pub tenant_id: TenantId,
    /// Session receiving the attachment.
    pub session_id: SessionId,
    /// Caller-owned identity of the message carrying the attachment.
    pub client_message_id: ClientMessageId,
    /// Zero-based position of the attachment within that message.
    pub ordinal: u16,
}

impl SessionAttachmentSlot {
    /// Returns the stable attachment identifier this slot addresses.
    ///
    /// Derived as a UUIDv5 over the byte-length-prefixed slot coordinates, so the
    /// name can never be ambiguous for a client message id that itself contains the
    /// separator, and two different slots can never collide.
    #[must_use]
    pub fn attachment_id(&self) -> SessionAttachmentId {
        let client_message_id = self.client_message_id.as_str();
        let name = format!(
            "{}:{}:{}:{client_message_id}:{}",
            self.tenant_id,
            self.session_id,
            client_message_id.len(),
            self.ordinal
        );
        SessionAttachmentId(Uuid::new_v5(
            &SESSION_ATTACHMENT_SLOT_NAMESPACE,
            name.as_bytes(),
        ))
    }
}

/// Validated attachment bytes and metadata submitted for one slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAttachmentUpload {
    /// Contact that authored the message, when the session is contact-owned.
    pub contact_id: Option<ContactId>,
    /// Display name admitted by the upload boundary.
    pub name: String,
    /// Canonical MIME type admitted by the upload boundary.
    pub mime_type: String,
    /// Attachment bytes.
    pub content: Vec<u8>,
}

/// Whether one attachment slot write created storage or replayed an existing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAttachmentDisposition {
    /// This request created the metadata row and the stored object.
    Created,
    /// The slot already held byte-identical content with identical metadata.
    Replayed,
}

/// Durable attachment metadata plus the disposition of the write that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSessionAttachment {
    /// Durable attachment metadata safe to return to callers.
    pub attachment: Attachment,
    /// Whether this request created the stored attachment or replayed it.
    pub disposition: SessionAttachmentDisposition,
}

impl StoredSessionAttachment {
    /// Returns whether this request created the stored attachment.
    ///
    /// Rejection cleanup deletes only created attachments: deleting a replayed one
    /// would destroy the original message's durable attachment.
    #[must_use]
    pub fn was_created(&self) -> bool {
        matches!(self.disposition, SessionAttachmentDisposition::Created)
    }
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
    /// Caller-owned retry identity for this message. Required.
    pub client_message_id: ClientMessageId,
    /// Exact pending user-input target this message replies to, when the caller
    /// addresses one explicitly.
    #[serde(default)]
    pub reply_to: Option<MessageReplyTarget>,
    /// Event cursor the caller observed before submitting, retained by the session
    /// admission so a retry resumes the same stream position.
    ///
    /// Transport state only: it is deliberately excluded from the semantic
    /// admission hash, so reconnecting with a different cursor is not a conflict.
    #[serde(default)]
    pub stream_cursor: Option<SequenceNum>,
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
    /// Pre-admission event cursor retained for this client message id.
    ///
    /// The first admission stores the cursor the caller observed; every later retry
    /// of the same id returns that stored value rather than the newer stream head,
    /// so a reconnect cannot skip events published by the original submission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_cursor: Option<SequenceNum>,
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
        ChannelRef, ClientMessageId, ContactSessionChannelRequest, ContactSessionMessageRequest,
        MAX_CLIENT_MESSAGE_ID_BYTES, MAX_CONTACT_SESSION_ATTACHMENT_BYTES, MessageReplyTarget,
        SessionAttachmentSlot,
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

    #[test]
    fn client_message_id_rejects_missing_oversized_and_control_character_ids() {
        // Pins: the caller-owned retry identity is validated at the type boundary, so no
        // route can admit an empty, oversized, or control-character id and no code path can
        // synthesize one for a client that omitted it.
        assert!(ClientMessageId::new("Ev01J8ZY7M6QK").is_ok());
        assert!(ClientMessageId::new("x".repeat(MAX_CLIENT_MESSAGE_ID_BYTES)).is_ok());

        for invalid in [
            String::new(),
            "x".repeat(MAX_CLIENT_MESSAGE_ID_BYTES + 1),
            "abc\ndef".to_string(),
            "abc\u{0}".to_string(),
        ] {
            assert!(
                ClientMessageId::new(invalid.clone()).is_err(),
                "invalid client message id must be rejected: {invalid:?}"
            );
            assert!(
                serde_json::from_value::<ClientMessageId>(serde_json::json!(invalid)).is_err(),
                "deserialization must apply the same validation: {invalid:?}"
            );
        }

        let id = ClientMessageId::new("client-message-1").expect("valid id");
        assert_eq!(
            serde_json::to_value(&id).expect("serialize client message id"),
            serde_json::json!("client-message-1")
        );
    }

    #[test]
    fn attachment_slot_ids_are_deterministic_and_collision_free() {
        // Pins: attachment identity is a pure function of tenant/session/message/ordinal, so a
        // retried upload addresses the same row instead of minting a random second attachment,
        // and a message id containing the separator cannot collide with another slot.
        let tenant_id = TenantId(Uuid::from_u128(11));
        let session_id = SessionId(Uuid::from_u128(12));
        let slot = |client_message_id: &str, ordinal: u16| SessionAttachmentSlot {
            tenant_id,
            session_id,
            client_message_id: ClientMessageId::new(client_message_id).expect("valid id"),
            ordinal,
        };

        assert_eq!(
            slot("m-1", 0).attachment_id(),
            slot("m-1", 0).attachment_id()
        );

        let distinct = [
            slot("m-1", 0).attachment_id(),
            slot("m-1", 1).attachment_id(),
            slot("m-2", 0).attachment_id(),
            slot("m:1:0", 0).attachment_id(),
            slot("m", 0).attachment_id(),
            SessionAttachmentSlot {
                session_id: SessionId(Uuid::from_u128(13)),
                ..slot("m-1", 0)
            }
            .attachment_id(),
        ];
        let unique = distinct.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(
            unique.len(),
            distinct.len(),
            "distinct attachment slots must not share an id"
        );
    }

    #[test]
    fn message_reply_target_round_trips_exact_strict_variants() {
        // Pins: an explicit reply target carries only caller-verifiable coordinates and rejects
        // unknown fields, so a caller cannot smuggle approved-budget or plan-hash terms.
        let cases = [
            MessageReplyTarget::ExecutionConfirmation {
                run_uid: Uuid::from_u128(21),
            },
            MessageReplyTarget::ExecutionInput {
                run_uid: Uuid::from_u128(21),
                task_id: Uuid::from_u128(22),
                generation: 3,
            },
            MessageReplyTarget::WorkerInput {
                worker_id: "worker-3".to_string(),
                turn_id: "worker-turn-1".to_string(),
                generation: 2,
                input_request_id: "request-1".to_string(),
            },
        ];

        for target in cases {
            let encoded = serde_json::to_value(&target).expect("serialize reply target");
            assert_eq!(
                serde_json::from_value::<MessageReplyTarget>(encoded.clone())
                    .expect("deserialize reply target"),
                target
            );

            let mut malformed = encoded;
            malformed
                .as_object_mut()
                .and_then(|outer| outer.values_mut().next())
                .and_then(serde_json::Value::as_object_mut)
                .expect("reply target payload is an object")
                .insert("approved_budget".to_string(), serde_json::json!({}));
            assert!(
                serde_json::from_value::<MessageReplyTarget>(malformed).is_err(),
                "reply target variants must reject unknown fields"
            );
        }
    }

    #[test]
    fn contact_session_message_request_requires_a_client_message_id() {
        // Pins: a stale client that omits the retry identity gets a typed decode error instead
        // of a server-synthesized id that would silently lose retry protection.
        let mut body = serde_json::json!({
            "tenant_id": Uuid::nil(),
            "session_id": Uuid::nil(),
            "contact_token": "token",
            "user_message": "hello",
        });
        assert!(
            serde_json::from_value::<ContactSessionMessageRequest>(body.clone()).is_err(),
            "request without client_message_id must fail to decode"
        );

        body["client_message_id"] = serde_json::json!("client-message-1");
        let decoded = serde_json::from_value::<ContactSessionMessageRequest>(body)
            .expect("request with client_message_id decodes");
        assert_eq!(decoded.client_message_id.as_str(), "client-message-1");
        assert_eq!(decoded.reply_to, None);
        assert_eq!(decoded.stream_cursor, None);
    }

    fn base_message_request() -> ContactSessionMessageRequest {
        ContactSessionMessageRequest {
            tenant_id: TenantId(Uuid::nil()),
            session_id: SessionId(Uuid::nil()),
            contact_token: "token".to_string(),
            client_message_id: ClientMessageId::new("client-message-1")
                .expect("fixture client message id is valid"),
            reply_to: None,
            stream_cursor: None,
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
