//! Linked-provider request, response, and webhook domain types.

use std::fmt;

use chrono::{DateTime, Utc};
use moa_core::types::credentials::RedactedSecret;
use moa_core::types::identifiers::TenantId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{KnowledgeConnection, ProviderRecordAcl};

/// Linked-account provider identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkedProviderKind {
    /// Nango linked-account provider.
    Nango,
    /// Merge linked-account provider.
    Merge,
}

impl LinkedProviderKind {
    /// Returns the stable provider identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nango => "nango",
            Self::Merge => "merge",
        }
    }

    /// Parses one exact linked-provider identifier.
    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        match value {
            "nango" => Some(Self::Nango),
            "merge" => Some(Self::Merge),
            _ => None,
        }
    }
}

impl fmt::Display for LinkedProviderKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Parser provider identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParserKind {
    /// Local native parser backed by deterministic MOA parsing and liteparse.
    Native,
    /// LlamaParse cloud parser.
    LlamaParse,
    /// Unstructured partitioning parser.
    Unstructured,
    /// Reducto parser.
    Reducto,
}

impl ParserKind {
    /// Returns the stable parser identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::LlamaParse => "llamaparse",
            Self::Unstructured => "unstructured",
            Self::Reducto => "reducto",
        }
    }
}

/// Stored provider webhook event used for idempotent delivery handling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeProviderEventRecord {
    /// Tenant-owned provider-event row identifier.
    pub provider_event_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional linked connection associated with the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_uid: Option<Uuid>,
    /// Linked-account provider that emitted the event.
    pub provider: String,
    /// Provider event identifier used for idempotency.
    pub provider_event_id: String,
    /// Provider event type.
    pub event_type: String,
    /// Local event status.
    pub status: String,
    /// Redacted provider payload.
    #[serde(default)]
    pub payload: Value,
    /// Whether this delivery duplicated an already recorded event.
    pub duplicate: bool,
}

/// Request to create a provider link token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateLinkTokenRequest {
    /// Tenant that will own the connection.
    pub tenant_id: TenantId,
    /// Connector identifier.
    pub connector: String,
    /// Optional caller-facing account reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_account_id: Option<String>,
    /// Optional end-user email address required by some link-token providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_user_email_address: Option<String>,
    /// Optional redirect URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_url: Option<String>,
    /// Provider-native selected source state requested before link creation.
    #[serde(default)]
    pub source_selection: Value,
}

/// Provider link token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkToken {
    /// Provider identifier.
    pub provider: LinkedProviderKind,
    /// Short-lived token.
    pub token: String,
    /// Optional hosted link URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_url: Option<String>,
    /// Token expiration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

/// Request to exchange a provider public token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangePublicTokenRequest {
    /// Tenant that owns the link.
    pub tenant_id: TenantId,
    /// Connector the link operation selected.
    ///
    /// For Merge this is the unified-API product category (for example
    /// `knowledgebase`), and it is the category every later request for this
    /// connection must use. Carrying it through the exchange is what stops a
    /// connection linked for one category from being synced against another.
    pub connector: String,
    /// Token returned by provider-hosted UI.
    pub public_token: String,
    /// Provider-native selected source state collected by the frontend.
    #[serde(default)]
    pub source_selection: Value,
}

/// Request to apply provider-native selected source state to a linked account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplySourceSelectionRequest {
    /// Connection whose provider-native selected sources should be applied.
    pub connection: KnowledgeConnection,
}

/// Linked account returned by a provider after token exchange.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkedAccount {
    /// Provider identifier.
    pub provider: LinkedProviderKind,
    /// Provider connector.
    pub connector: String,
    /// Provider account identifier.
    pub provider_account_id: String,
    /// Raw credential material returned by the provider, kept in memory only until stored.
    #[serde(skip)]
    pub credential_material: Option<String>,
    /// Safe account metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Request to trigger a provider sync.
///
/// Deliberately not `Clone`, `Serialize`, or `Deserialize`: it carries resolved
/// credential material, so it must be impossible to persist into the durable
/// sync journal, an event, Restate state, or a model payload. The plaintext
/// lives only between vault resolution and the outbound provider request.
#[derive(Debug)]
pub struct TriggerSyncRequest {
    /// Connection to sync.
    pub connection: KnowledgeConnection,
    /// Resolved tenant credential, when the provider requires one.
    pub credential: Option<RedactedSecret>,
    /// Provider model or collection to sync.
    pub model: Option<String>,
    /// Provider sync variant or partition name.
    pub variant: Option<String>,
}

/// Request to start (or re-confirm) a newly linked connection's initial sync.
///
/// Separate from [`TriggerSyncRequest`] because the two have different
/// contracts: an operator-requested re-sync may use a provider's one-off,
/// credit-consuming endpoints, while the initial link runs inside a durable
/// claim that can replay it after a crash. Implementations of the initial start
/// must therefore be naturally idempotent or read-only.
///
/// Carries resolved credential material, so it is deliberately not `Clone`,
/// `Serialize`, or `Deserialize`. See [`TriggerSyncRequest`].
#[derive(Debug)]
pub struct StartInitialSyncRequest {
    /// Newly linked connection whose first provider sync should be running.
    pub connection: KnowledgeConnection,
    /// Resolved tenant credential, when the provider requires one.
    pub credential: Option<RedactedSecret>,
}

/// Request to revoke one linked account at its remote provider.
///
/// This request crosses the final secret-bearing boundary before an outbound
/// provider deletion. It deliberately implements neither `Clone`, `Serialize`,
/// nor `Deserialize`, so resolved primary credential material cannot enter a
/// durable disconnect journal, event, or model payload. Its custom `Debug`
/// implementation includes only the provider-native non-secret selector and a
/// fixed credential redaction.
pub struct RemoteRevokeRequest {
    /// Connection whose provider-native linked account must be revoked.
    pub connection: KnowledgeConnection,
    /// Resolved active credential from this connection's `primary` slot, when required.
    pub credential: Option<RedactedSecret>,
}

impl fmt::Debug for RemoteRevokeRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemoteRevokeRequest")
            .field("connection_uid", &self.connection.connection_uid)
            .field("tenant_id", &self.connection.tenant_id)
            .field("provider", &self.connection.provider)
            .field("connector", &self.connection.connector)
            .field("provider_account_id", &self.connection.provider_account_id)
            .field("credential", &"<redacted>")
            .finish()
    }
}

/// Outcome of an idempotent initial-sync start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitialSyncStarted {
    /// Provider identifier.
    pub provider: LinkedProviderKind,
    /// Provider sync identifier, when the provider reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_sync_id: Option<String>,
    /// Whether the provider reports the initial sync already finished.
    ///
    /// `false` means the sync is genuinely running. Providers must return an
    /// error rather than `false` for failed, paused, or unrecognized states, so
    /// an ambiguous provider answer never reads as "still working".
    pub completed: bool,
    /// Redacted provider metadata for observability.
    #[serde(default)]
    pub metadata: Value,
}

/// Provider sync trigger acknowledgement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggeredSync {
    /// Provider identifier.
    pub provider: LinkedProviderKind,
    /// Provider sync identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_sync_id: Option<String>,
    /// Provider status.
    pub status: String,
    /// Safe metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Request to list changed provider records.
///
/// Carries resolved credential material, so it is deliberately not `Clone`,
/// `Serialize`, or `Deserialize`. See [`TriggerSyncRequest`].
#[derive(Debug)]
pub struct ListChangedRecordsRequest {
    /// Connection to inspect.
    pub connection: KnowledgeConnection,
    /// Resolved tenant credential, when the provider requires one.
    pub credential: Option<RedactedSecret>,
    /// The tenant's current ACL fingerprint key.
    ///
    /// Adapters key each provider principal as they normalize a record, inside
    /// the same call that reads it, so a raw identity never survives the listing
    /// step — which is what lets the resulting page be journaled durably.
    pub acl_key: std::sync::Arc<crate::acl_key::SourceAclKey>,
    /// Provider cursor.
    pub cursor: Option<String>,
    /// Lower bound for provider-side modified timestamps.
    pub modified_after: Option<DateTime<Utc>>,
    /// Maximum records to return.
    pub limit: Option<u32>,
    /// Provider sync variant or partition name.
    pub variant: Option<String>,
}

/// Page of normalized provider records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordPage {
    /// Normalized records.
    #[serde(default)]
    pub records: Vec<ProviderRecord>,
    /// Cursor for the next page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Explicit content-materialization contract chosen by a provider adapter.
///
/// Provider payloads are normalized into this type before they enter the
/// ingestion pipeline. Ingestion therefore never guesses which arbitrary JSON
/// field contains content and never substitutes a display title for content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderRecordMaterialization {
    /// Text already returned by the provider record listing.
    InlineText {
        /// Normalized document text.
        text: String,
        /// Provider-reported MIME type, when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    /// Content that must be fetched through the linked provider's authenticated hook.
    ProviderFetch {
        /// Provider-reported MIME type used when the fetch response omits one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    /// Content available at a reviewed, directly fetchable URL.
    FetchableUrl {
        /// URL handed to the configured document parser.
        url: String,
        /// Provider-reported MIME type, when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    /// Metadata and ACL state only; this record intentionally has no indexable content.
    MetadataOnly,
}

impl ProviderRecordMaterialization {
    /// Returns whether this record intentionally carries no indexable content.
    #[must_use]
    pub fn is_metadata_only(&self) -> bool {
        matches!(self, Self::MetadataOnly)
    }

    /// Returns whether content must be fetched through the authenticated provider hook.
    #[must_use]
    pub fn requires_provider_fetch(&self) -> bool {
        matches!(self, Self::ProviderFetch { .. })
    }

    /// Returns text already materialized by the provider record listing.
    #[must_use]
    pub fn inline_text(&self) -> Option<&str> {
        match self {
            Self::InlineText { text, .. } => Some(text),
            Self::ProviderFetch { .. } | Self::FetchableUrl { .. } | Self::MetadataOnly => None,
        }
    }

    /// Returns a reviewed URL that can be handed directly to the document parser.
    #[must_use]
    pub fn fetchable_url(&self) -> Option<&str> {
        match self {
            Self::FetchableUrl { url, .. } => Some(url),
            Self::InlineText { .. } | Self::ProviderFetch { .. } | Self::MetadataOnly => None,
        }
    }

    /// Returns the provider-reported MIME type attached to this materialization intent.
    #[must_use]
    pub fn mime_type(&self) -> Option<&str> {
        match self {
            Self::InlineText { mime_type, .. }
            | Self::ProviderFetch { mime_type }
            | Self::FetchableUrl { mime_type, .. } => mime_type.as_deref(),
            Self::MetadataOnly => None,
        }
    }
}

/// Provider record before normalization into a knowledge object.
///
/// Serializable — and safely so. [`ProviderRecord::acl`] holds principals that
/// the adapter already keyed during normalization, so the durable Restate
/// journal entry for a listed page contains opaque fingerprints and never a
/// readable provider identity. That is the reason the keying happens at the
/// adapter boundary rather than later in the pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderRecord {
    /// Provider source identifier.
    pub source_id: String,
    /// Source object type.
    pub object_type: String,
    /// Optional title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional URI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// Optional change token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_token: Option<String>,
    /// Whether the provider reports this record as deleted.
    #[serde(default)]
    pub deleted: bool,
    /// Source update timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_updated_at: Option<DateTime<Utc>>,
    /// Provider-normalized content materialization intent.
    pub materialization: ProviderRecordMaterialization,
    /// Safe metadata.
    #[serde(default)]
    pub metadata: Value,
    /// Raw record payload kept in memory for normalization only.
    #[serde(default)]
    pub payload: Value,
    /// The source permissions governing this record.
    ///
    /// Required with no default: a record whose ACL was never stated cannot be
    /// distinguished from one the provider said was public, and the ingestion
    /// pipeline refuses to guess.
    pub acl: ProviderRecordAcl,
}

/// Request to fetch the byte content of one provider record.
///
/// The connection carries the provider account identity and connector needed to
/// authorize the fetch, the resolved credential authenticates it, and the
/// record identifies the specific source object whose content should be
/// downloaded. The credential is borrowed so one non-cloneable secret can
/// authorize the bounded requests in one page without being serialized or
/// copied once per record.
#[derive(Debug)]
pub struct FetchRecordContentRequest<'credential> {
    /// Connection whose provider account authorizes the fetch.
    pub connection: KnowledgeConnection,
    /// Resolved tenant credential, when the provider requires one.
    pub credential: Option<&'credential RedactedSecret>,
    /// Normalized record whose byte content should be downloaded.
    pub record: ProviderRecord,
}

/// Byte content downloaded for one provider record.
///
/// Kept in memory only for the duration of one ingestion pass; the bytes are
/// handed straight to a document parser and never persisted or serialized into
/// the durable sync journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedRecordContent {
    /// Raw downloaded bytes, subject to the provider size cap.
    pub bytes: Vec<u8>,
    /// Reported content MIME type, when the provider supplies one.
    pub mime_type: Option<String>,
}

/// One integration a linked-account provider can connect for a tenant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderIntegration {
    /// Stable integration identifier passed as `connector` in the link flow.
    pub id: String,
    /// Human-readable name for connect UIs.
    pub display_name: String,
    /// Optional logo URL supplied by the provider.
    pub logo_url: Option<String>,
}

/// Verified provider webhook event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebhookEvent {
    /// Provider identifier.
    pub provider: String,
    /// Event identifier.
    pub event_id: String,
    /// Event type.
    pub event_type: String,
    /// Safe metadata.
    #[serde(default)]
    pub metadata: Value,
}
