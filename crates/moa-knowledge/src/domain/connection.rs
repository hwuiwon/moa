//! Tenant knowledge connection domain types.

use chrono::{DateTime, Utc};
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use moa_core::types::memory::InformationBarrierId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{LinkedProviderKind, SyncRunStatus};

/// One linked external account for one tenant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeConnection {
    /// Tenant-owned connection identifier shared exactly with the generic connector parent.
    pub connection_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Linked-account provider.
    pub provider: LinkedProviderKind,
    /// Provider connector identifier.
    pub connector: String,
    /// Provider account identifier.
    pub provider_account_id: String,
    /// Safe provider metadata.
    #[serde(default)]
    pub metadata: Value,
    /// Provider-native selected source state. Empty means provider default/all.
    #[serde(default)]
    pub source_selection: Value,
    /// Source-owned information barrier applied to every record from this connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub information_barrier: Option<InformationBarrierId>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Last successful sync timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_at: Option<DateTime<Utc>>,
}

impl KnowledgeConnection {
    /// Returns the exact generic connector parent identity for this sync projection.
    #[must_use]
    pub const fn connector_connection_id(&self) -> ConnectorConnectionId {
        ConnectorConnectionId(self.connection_uid)
    }
}

/// Durable state of one provider-side knowledge-connection revocation.
///
/// `transmitting` is the no-return boundary: once persisted, an automatic
/// replay must never send the provider delete again. Transport uncertainty is
/// therefore terminal as `unknown_outcome` until an operator reconciles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeDisconnectState {
    /// The operation is durably reserved but no provider request was sent.
    Reserved,
    /// The provider request may have been transmitted and must not be replayed.
    Transmitting,
    /// The provider confirmed that it deleted the linked account.
    Deleted,
    /// The provider confirmed that the linked account was already absent.
    AlreadyAbsent,
    /// A local failure occurred before the provider send boundary.
    FailedBeforeSend,
    /// The provider request outcome cannot be determined safely.
    UnknownOutcome,
}

impl KnowledgeDisconnectState {
    /// Returns the stable database identifier for this state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Transmitting => "transmitting",
            Self::Deleted => "deleted",
            Self::AlreadyAbsent => "already_absent",
            Self::FailedBeforeSend => "failed_before_send",
            Self::UnknownOutcome => "unknown_outcome",
        }
    }

    /// Parses one exact database state identifier.
    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        match value {
            "reserved" => Some(Self::Reserved),
            "transmitting" => Some(Self::Transmitting),
            "deleted" => Some(Self::Deleted),
            "already_absent" => Some(Self::AlreadyAbsent),
            "failed_before_send" => Some(Self::FailedBeforeSend),
            "unknown_outcome" => Some(Self::UnknownOutcome),
            _ => None,
        }
    }

    /// Returns whether no automatic transition may follow this state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Deleted | Self::AlreadyAbsent | Self::FailedBeforeSend | Self::UnknownOutcome
        )
    }
}

/// Durable ledger row for one knowledge-connection remote revocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeConnectionDisconnectProgress {
    /// Tenant that owns the connection and ledger row.
    pub tenant_id: TenantId,
    /// Shared knowledge and generic connector connection identifier.
    pub connection_uid: Uuid,
    /// Replay-stable service operation that first reserved the disconnect.
    pub operation_id: String,
    /// Canonical secret-free hash of the connection selector.
    pub request_hash: String,
    /// Secret-free operation key used to correlate provider transmission.
    pub provider_operation_id: Uuid,
    /// Current send-boundary state.
    pub state: KnowledgeDisconnectState,
    /// Stable secret-free failure classification, when the outcome records one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Reservation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last successful state transition timestamp.
    pub updated_at: DateTime<Utc>,
    /// Terminal transition timestamp, when one has been recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
}

/// Input that reserves the one remote-revocation operation for a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewKnowledgeConnectionDisconnect {
    /// Tenant that owns the connection.
    pub tenant_id: TenantId,
    /// Connection being disconnected.
    pub connection_uid: Uuid,
    /// Replay-stable caller operation identifier.
    pub operation_id: String,
    /// Canonical secret-free hash of the connection selector.
    pub request_hash: String,
    /// Replay-stable, secret-free provider correlation key.
    pub provider_operation_id: Uuid,
}

/// Outcome of reserving a connection's durable disconnect operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnowledgeDisconnectReservation {
    /// This caller inserted the operation and may perform pre-send work.
    Reserved(KnowledgeConnectionDisconnectProgress),
    /// This connection already has an operation; resume its exact state.
    Existing(KnowledgeConnectionDisconnectProgress),
    /// The operation identifier already belongs to another connection.
    OperationConflict,
}

/// One compare-and-swap transition in the disconnect send-boundary ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KnowledgeDisconnectTransition {
    /// Move `reserved` to the provider send boundary.
    Transmitting,
    /// Record a provider-confirmed deletion after transmission.
    Deleted,
    /// Record provider-confirmed prior absence after transmission.
    AlreadyAbsent,
    /// Record a local failure that occurred before transmission.
    FailedBeforeSend {
        /// Stable secret-free local failure classification.
        error_code: String,
    },
    /// Record an uncertain outcome after transmission.
    UnknownOutcome {
        /// Stable secret-free provider/transport failure classification.
        error_code: String,
    },
}

impl KnowledgeDisconnectTransition {
    /// Returns the only state from which this transition is permitted.
    #[must_use]
    pub const fn source_state(&self) -> KnowledgeDisconnectState {
        match self {
            Self::Transmitting | Self::FailedBeforeSend { .. } => {
                KnowledgeDisconnectState::Reserved
            }
            Self::Deleted | Self::AlreadyAbsent | Self::UnknownOutcome { .. } => {
                KnowledgeDisconnectState::Transmitting
            }
        }
    }

    /// Returns the state produced by this transition.
    #[must_use]
    pub const fn target_state(&self) -> KnowledgeDisconnectState {
        match self {
            Self::Transmitting => KnowledgeDisconnectState::Transmitting,
            Self::Deleted => KnowledgeDisconnectState::Deleted,
            Self::AlreadyAbsent => KnowledgeDisconnectState::AlreadyAbsent,
            Self::FailedBeforeSend { .. } => KnowledgeDisconnectState::FailedBeforeSend,
            Self::UnknownOutcome { .. } => KnowledgeDisconnectState::UnknownOutcome,
        }
    }

    /// Returns the stable failure code carried by a terminal error transition.
    #[must_use]
    pub fn error_code(&self) -> Option<&str> {
        match self {
            Self::FailedBeforeSend { error_code } | Self::UnknownOutcome { error_code } => {
                Some(error_code)
            }
            Self::Transmitting | Self::Deleted | Self::AlreadyAbsent => None,
        }
    }
}

/// Linked connection plus latest sync-run status for service projections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeConnectionProjection {
    /// Linked connection.
    pub connection: KnowledgeConnection,
    /// Lifecycle status read through from the generic connector parent.
    pub parent_lifecycle_status: String,
    /// Most recent sync-run status, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_status: Option<SyncRunStatus>,
}
