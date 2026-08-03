//! Operation-fenced link claim types.
//!
//! Linking a connection has four durable effects — a generic connector parent,
//! a credential version, a knowledge projection, and an initial provider sync —
//! that a crash or a concurrent link can interleave. The claim turns them into
//! one compare-and-swap state machine keyed by `(tenant, operation_id)` so a
//! replayed or concurrent link cannot orphan a credential version, attach one
//! to a connection the upsert did not create, or overwrite a newer claim.

use chrono::{DateTime, Utc};
use moa_core::types::identifiers::TenantId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// State of one operation-fenced link claim.
///
/// `Compensated` is terminal: a compensated operation is never resumed, and a
/// later link for the same connection is a different claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkClaimState {
    /// The claim exists and owns the connection identifier; nothing else is written.
    Reserved,
    /// The exact generic connector parent was claimed and its ownership is durable.
    ParentClaimed,
    /// The candidate credential version exists and is recorded on the claim.
    CredentialWritten,
    /// A post-write failure is being undone.
    Compensating,
    /// Compensation finished; this operation can never succeed.
    Compensated,
    /// The connection and its initial provider sync are durable.
    Finalized,
}

impl LinkClaimState {
    /// Returns the stable database state identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::ParentClaimed => "parent_claimed",
            Self::CredentialWritten => "credential_written",
            Self::Compensating => "compensating",
            Self::Compensated => "compensated",
            Self::Finalized => "finalized",
        }
    }

    /// Parses a stored state, rejecting anything outside the closed set.
    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        match value {
            "reserved" => Some(Self::Reserved),
            "parent_claimed" => Some(Self::ParentClaimed),
            "credential_written" => Some(Self::CredentialWritten),
            "compensating" => Some(Self::Compensating),
            "compensated" => Some(Self::Compensated),
            "finalized" => Some(Self::Finalized),
            _ => None,
        }
    }
}

/// Closed owner of credential material for one managed knowledge definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeCredentialOwnership {
    /// The provider adapter owns deployment authentication; no tenant credential is stored.
    ProviderNative,
    /// MOA owns the tenant credential series addressed only by connection identity.
    MoaManaged,
}

impl KnowledgeCredentialOwnership {
    /// Returns the stable database representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderNative => "provider_native",
            Self::MoaManaged => "moa_managed",
        }
    }

    /// Parses one exact stored representation.
    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        match value {
            "provider_native" => Some(Self::ProviderNative),
            "moa_managed" => Some(Self::MoaManaged),
            _ => None,
        }
    }
}

/// One durable link claim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkClaim {
    /// Tenant that owns the link.
    pub tenant_id: TenantId,
    /// Caller-supplied, replay-stable operation identifier.
    pub operation_id: String,
    /// Canonical hash of the operation's selector and inputs.
    pub request_hash: String,
    /// Principal recorded as the credential owner, when a caller performed the link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_identity_id: Option<Uuid>,
    /// Connection this link expects to own.
    pub connection_uid: Uuid,
    /// Whether this claim created the generic connector parent it now owns.
    #[serde(default)]
    pub parent_created_by_claim: bool,
    /// Parent generation observed before this link staged its credential.
    ///
    /// Persisting the original fence prevents a replay after generation advance
    /// from treating the newer generation as a fresh rotation and advancing it
    /// a second time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_expected_generation: Option<u64>,
    /// Closed owner of the credential material selected by the managed definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ownership: Option<KnowledgeCredentialOwnership>,
    /// Exact MOA vault candidate this link activated, when MOA manages the credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_credential_ref: Option<String>,
    /// Exact prior MOA vault active version observed while staging, for rollback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_vault_credential_ref: Option<String>,
    /// Current claim state.
    pub state: LinkClaimState,
    /// Sync run whose durable provider trigger proves the link may finalize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_run_uid: Option<Uuid>,
    /// Claim creation time.
    pub created_at: DateTime<Utc>,
    /// Last transition time.
    pub updated_at: DateTime<Utc>,
}

/// Claim identity and immutable inputs supplied when reserving a link.
#[derive(Debug, Clone, PartialEq)]
pub struct NewLinkClaim {
    /// Tenant that owns the link.
    pub tenant_id: TenantId,
    /// Caller-supplied, replay-stable operation identifier.
    pub operation_id: String,
    /// Canonical hash of the operation's selector and inputs.
    pub request_hash: String,
    /// Authenticated operator recorded as the credential owner for a new claim.
    ///
    /// `None` is accepted only when resuming an exact existing claim.
    pub owner_identity_id: Option<Uuid>,
    /// Connection this link will own, resolved before any credential is written.
    pub connection_uid: Uuid,
}

/// Outcome of reserving a link claim.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkClaimReservation {
    /// This caller inserted the claim and owns the link from here.
    Reserved(LinkClaim),
    /// An identical claim already exists; resume from the state it records.
    Existing(LinkClaim),
    /// The operation id was reused with a different selector or connection.
    Conflict,
    /// Another non-terminal link already owns this connection.
    ConnectionBusy,
    /// No existing claim matched and a service actor cannot create an ownerless claim.
    OwnerRequired,
}

/// One compare-and-swap transition of a link claim.
///
/// Each variant names both the states it may advance from and what it records,
/// so a resumed link cannot skip a state or record a reference it did not write.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkClaimTransition {
    /// `reserved` to `parent_claimed`, recording whether this claim created the parent.
    ParentClaimed {
        /// Whether the managed-parent claim inserted the generic connector parent.
        parent_created_by_claim: bool,
        /// Exact parent generation to fence for the staged credential write.
        credential_expected_generation: u64,
    },
    /// `parent_claimed` to `credential_written`, recording every exact credential receipt.
    CredentialWritten {
        /// Closed owner selected by the exact managed definition.
        credential_ownership: KnowledgeCredentialOwnership,
        /// Exact candidate activated in MOA's vault, for a managed credential.
        candidate_credential_ref: Option<String>,
        /// Exact active vault predecessor observed by the staged transaction.
        previous_vault_credential_ref: Option<String>,
    },
    /// Records the sync run this link owns, without changing state.
    ///
    /// Written immediately after the run is claimed and *before* any provider
    /// dispatch, so a crash in between is recoverable: the replay knows which run
    /// is its own and re-dispatches that exact trigger, instead of either
    /// starting a second run or finalizing on some other run's evidence.
    SyncRunClaimed {
        /// Run this link claimed for its initial provider sync.
        sync_run_uid: Uuid,
    },
    /// Any non-final success phase to `compensating`.
    Compensating,
    /// `compensating` to `compensated`, the terminal failure state.
    Compensated,
    /// `credential_written` to `finalized`, recording the durably triggered sync run.
    Finalized {
        /// Sync run whose provider trigger completed before finalization.
        sync_run_uid: Uuid,
    },
}

impl LinkClaimTransition {
    /// Returns the state this transition moves the claim into.
    #[must_use]
    pub fn target_state(&self) -> LinkClaimState {
        match self {
            Self::ParentClaimed { .. } => LinkClaimState::ParentClaimed,
            // Recording the owned run is a self-transition: it adds durable
            // knowledge without advancing the link's progress.
            Self::CredentialWritten { .. } | Self::SyncRunClaimed { .. } => {
                LinkClaimState::CredentialWritten
            }
            Self::Compensating => LinkClaimState::Compensating,
            Self::Compensated => LinkClaimState::Compensated,
            Self::Finalized { .. } => LinkClaimState::Finalized,
        }
    }

    /// Returns the states this transition may advance from.
    #[must_use]
    pub fn permitted_source_states(&self) -> &'static [LinkClaimState] {
        match self {
            Self::ParentClaimed { .. } => &[LinkClaimState::Reserved],
            Self::CredentialWritten { .. } => &[LinkClaimState::ParentClaimed],
            Self::SyncRunClaimed { .. } => &[LinkClaimState::CredentialWritten],
            // A failure before the credential exists still has to pass through
            // `compensating`, so every failed link ends in one terminal state.
            Self::Compensating => &[
                LinkClaimState::Reserved,
                LinkClaimState::ParentClaimed,
                LinkClaimState::CredentialWritten,
            ],
            Self::Compensated => &[LinkClaimState::Compensating],
            Self::Finalized { .. } => &[LinkClaimState::CredentialWritten],
        }
    }
}
