//! Connector domain, persistence, and replay failures.

use moa_core::types::credentials::CredentialSlotName;
use moa_core::types::identifiers::ConnectorConnectionId;
use uuid::Uuid;

use crate::domain::{
    ConnectionGeneration, ConnectionStatus, ConnectorInvocationId, ConnectorInvocationState,
    OperationContractHash,
};

/// Failure returned by connector domain and persistence operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A connection origin was not a fixed syntactically valid HTTP(S) origin.
    #[error("invalid connector connection origin: {reason}")]
    InvalidConnectionOrigin {
        /// Stable, non-sensitive rejection reason.
        reason: &'static str,
    },
    /// A persisted connection generation was zero.
    #[error("connector connection generation must be positive, got {value}")]
    InvalidGeneration {
        /// Invalid numeric generation.
        value: u64,
    },
    /// A generation could not be advanced without overflowing.
    #[error("connector connection generation is exhausted")]
    GenerationExhausted,
    /// A requested lifecycle edge is outside the closed transition graph.
    #[error("invalid connector lifecycle transition from {from} to {to}")]
    InvalidTransition {
        /// Current lifecycle state.
        from: ConnectionStatus,
        /// Requested lifecycle state.
        to: ConnectionStatus,
    },
    /// A generation-fenced write observed a different current generation.
    #[error("stale connector generation: expected {expected}, actual {actual}")]
    GenerationConflict {
        /// Generation supplied by the caller.
        expected: ConnectionGeneration,
        /// Generation currently persisted.
        actual: ConnectionGeneration,
    },
    /// The requested connection does not exist in the caller's tenant scope.
    #[error("connector connection {connection_id} was not found")]
    ConnectionNotFound {
        /// Missing connection identity.
        connection_id: ConnectorConnectionId,
    },
    /// A service actor attempted to create a new managed parent without an authenticated owner.
    #[error(
        "an authenticated owner is required to create managed connector parent {connection_id}"
    )]
    ManagedParentOwnerRequired {
        /// Parent identity whose first creation requires an owner tuple.
        connection_id: ConnectorConnectionId,
    },
    /// A managed-parent operation id was reused with a different hash or connection identity.
    #[error("managed connector parent claim conflicts for {connection_id}")]
    ManagedParentClaimConflict {
        /// Connection identity supplied by the conflicting replay.
        connection_id: ConnectorConnectionId,
    },
    /// An existing connection is not the exact managed parent requested by the caller.
    #[error("managed connector parent {connection_id} mismatches at {field}")]
    ManagedParentMismatch {
        /// Existing connection whose immutable managed identity did not match.
        connection_id: ConnectorConnectionId,
        /// Stable non-sensitive field that differed.
        field: &'static str,
    },
    /// A provider is outside the closed managed knowledge-parent set.
    #[error("knowledge provider has no supported managed connector parent")]
    UnsupportedManagedKnowledgeProvider,
    /// Knowledge-only activation would expose a parent that also has action bindings.
    #[error("managed connector parent {connection_id} has action binding dependents")]
    ManagedParentActionDependents {
        /// Shared connection requiring ordinary action-capable activation.
        connection_id: ConnectorConnectionId,
    },
    /// A direct `Use` grant targeted a connection already in teardown.
    #[error("connector connection {connection_id} cannot change Use grants while {status}")]
    UseGrantConnectionUnavailable {
        /// Connection whose lifecycle blocks new grants.
        connection_id: ConnectorConnectionId,
        /// Current teardown lifecycle state.
        status: ConnectionStatus,
    },
    /// A direct `Use` subject did not exist in the connection tenant.
    #[error("connector Use {subject_kind} subject {subject_id} was not found in the tenant")]
    UseGrantSubjectNotFound {
        /// Stable closed subject kind.
        subject_kind: &'static str,
        /// Missing subject identity.
        subject_id: Uuid,
    },
    /// A direct `Use` grant targeted an inactive subject.
    #[error("connector Use {subject_kind} subject {subject_id} is inactive")]
    UseGrantSubjectInactive {
        /// Stable closed subject kind.
        subject_kind: &'static str,
        /// Inactive subject identity.
        subject_id: Uuid,
    },
    /// A built-in definition reference or compiled contract was malformed.
    #[error("invalid connector contract: {message}")]
    InvalidContract {
        /// Stable validation detail without credential or response material.
        message: String,
    },
    /// Persisted rows could not form one internally consistent tenant catalog.
    #[error("connector catalog invariant failed: {message}")]
    CatalogInvariant {
        /// Stable integrity detail without tenant response or credential material.
        message: String,
    },
    /// A persisted binding hash does not match its canonical compiled contract.
    #[error("connector contract hash mismatch: expected {expected}, actual {actual}")]
    ContractHashMismatch {
        /// Hash stored alongside the binding.
        expected: OperationContractHash,
        /// Hash recomputed from canonical contract bytes.
        actual: OperationContractHash,
    },
    /// A required credential slot was absent at activation.
    #[error("required connector credential slot `{slot}` is missing")]
    CredentialSlotMissing {
        /// Missing logical credential slot.
        slot: CredentialSlotName,
    },
    /// One replay-stable tool call was reused with different invocation inputs.
    #[error("connector invocation conflict for tool call `{tool_call_id}`")]
    InvocationConflict {
        /// Stable model/provider tool-call identity.
        tool_call_id: String,
    },
    /// An invocation attempted an invalid state transition.
    #[error("invalid connector invocation {invocation_id} transition from {from} to {to}")]
    InvocationStateConflict {
        /// Invocation whose state conflicted.
        invocation_id: ConnectorInvocationId,
        /// Current invocation state.
        from: ConnectorInvocationState,
        /// Requested invocation state.
        to: ConnectorInvocationState,
    },
    /// The authenticated invocation pins no longer match durable connection state.
    #[error("connector action pin mismatch at {field}")]
    ActionPinMismatch {
        /// Stable pin field that changed; never contains caller or endpoint data.
        field: &'static str,
    },
    /// Schema validation rejected connector input or output.
    #[error("connector {direction} schema validation failed")]
    SchemaValidation {
        /// Whether the rejected instance was inbound or outbound.
        direction: &'static str,
    },
    /// A redacted constrained-HTTP stage failed.
    #[error("connector HTTP operation failed at {code}")]
    Http {
        /// Stable non-sensitive failure code.
        code: &'static str,
    },
    /// Cooperative cancellation stopped one redacted constrained-HTTP stage.
    #[error("connector HTTP operation cancelled at {stage}")]
    Cancelled {
        /// Stable stage name without request or destination data.
        stage: &'static str,
    },
    /// A replay-safe invocation already exists and cannot be retransmitted.
    #[error("connector invocation is already {state}")]
    InvocationUnavailable {
        /// Durable state blocking retransmission.
        state: ConnectorInvocationState,
    },
    /// A non-idempotent invocation may have reached the upstream system and needs operator review.
    #[error(
        "connector invocation {invocation_id} has unknown outcome; manual_reconciliation_required"
    )]
    ManualReconciliationRequired {
        /// Durable invocation whose external effect cannot be inferred safely.
        invocation_id: ConnectorInvocationId,
    },
    /// Credential selection, audit, or opening failed before transmission.
    #[error("connector credential resolution failed: {0}")]
    Credential(#[from] moa_core::types::credentials::CredentialError),
    /// Canonical JSON serialization failed.
    #[error("connector contract serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    /// A shared tenant-scoped database boundary rejected the operation.
    #[error("connector tenant scope operation failed: {0}")]
    DatabaseScope(#[from] moa_core::error::MoaError),
    /// Transactional authorization outbox construction or enqueue failed.
    #[error("connector authorization outbox operation failed: {0}")]
    Authorization(#[from] moa_authz::AuthzError),
    /// Delegated connector-use authorization denied the authenticated caller.
    #[error("connector use authorization denied")]
    AuthorizationDenied,
    /// Connector-use authorization could not produce a trustworthy decision.
    #[error("connector use authorization unavailable")]
    AuthorizationUnavailable,
    /// Tenant-scoped connector persistence failed.
    #[error("connector storage operation failed: {0}")]
    Storage(#[from] sqlx::Error),
}
