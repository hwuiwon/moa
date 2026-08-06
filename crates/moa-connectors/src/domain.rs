//! Connection lifecycle, immutable binding, and replay domain contracts.

use std::collections::HashSet;
use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use moa_artifacts::connector::{
    ConnectorDefinition, HttpOperationContract, RuntimeConnectorAction,
    RuntimeConnectorAuthRequirement, validate_connector_action_id,
};
use moa_core::canonical_json::canonical_json_bytes;
use moa_core::types::action_policy::ActionPolicyEffect;
use moa_core::types::credentials::CredentialSlotName;
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use url::{Origin, Url};
use uuid::Uuid;

use crate::{Error, Result};

/// Domain separator for generation-pinned compiled operation-contract hashes.
pub const OPERATION_CONTRACT_HASH_DOMAIN: &str = "moa.connector.operation-contract.v1";

/// Positive generation fencing all mutable connection state and installed bindings.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ConnectionGeneration(NonZeroU64);

impl ConnectionGeneration {
    /// Builds a positive connection generation.
    pub fn new(value: u64) -> Result<Self> {
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(Error::InvalidGeneration { value })
    }

    /// Returns the numeric generation.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Returns the next generation or fails before overflow can erase the fence.
    pub fn next(self) -> Result<Self> {
        self.get()
            .checked_add(1)
            .ok_or(Error::GenerationExhausted)
            .and_then(Self::new)
    }
}

impl<'de> Deserialize<'de> for ConnectionGeneration {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

impl fmt::Display for ConnectionGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.get().fmt(formatter)
    }
}

/// Tenant connection lifecycle, independent from the latest health observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    /// Installed but waiting for credentials or authorization completion.
    PendingAuth,
    /// Eligible for catalog projection when its binding and health checks allow it.
    Active,
    /// Intentionally unavailable while retaining configuration and credentials.
    Suspended,
    /// Teardown is in progress and no new action may start.
    Disconnecting,
    /// Terminal lifecycle state retained for audit and replay boundaries.
    Deleted,
}

impl ConnectionStatus {
    /// Returns the stable database and audit representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PendingAuth => "pending_auth",
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Disconnecting => "disconnecting",
            Self::Deleted => "deleted",
        }
    }

    /// Returns whether the exact requested lifecycle edge is allowed.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::PendingAuth, Self::Active | Self::Disconnecting)
                | (Self::Active, Self::Suspended | Self::Disconnecting)
                | (Self::Suspended, Self::Active | Self::Disconnecting)
                | (Self::Disconnecting, Self::Deleted)
        )
    }

    /// Returns whether this lifecycle state may contribute to a fresh catalog snapshot.
    #[must_use]
    pub const fn is_catalog_visible(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Applies one exact lifecycle edge or returns a typed transition failure.
    pub fn transition(self, next: Self) -> Result<Self> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(Error::InvalidTransition {
                from: self,
                to: next,
            })
        }
    }
}

impl fmt::Display for ConnectionStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Latest connection health observation, independent from lifecycle state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionHealth {
    /// No conclusive health observation is available yet.
    Pending,
    /// The remote or managed backend passed its latest health check.
    Ready,
    /// The backend is usable with a known impairment.
    Degraded,
    /// The backend cannot currently serve calls.
    Unavailable,
    /// Security or operator policy has isolated the backend.
    Quarantined,
}

impl ConnectionHealth {
    /// Returns the stable database and audit representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Unavailable => "unavailable",
            Self::Quarantined => "quarantined",
        }
    }
}

impl fmt::Display for ConnectionHealth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Syntactically validated fixed HTTP(S) origin stored on a connection.
///
/// This type intentionally does not perform DNS/IP, HTTPS-environment, port,
/// redirect, or address-pinning admission. The HTTP runtime owns those checks.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ConnectionOrigin(String);

impl ConnectionOrigin {
    /// Parses one fixed origin with no path, userinfo, query, fragment, wildcard, or template.
    pub fn parse(value: &str) -> Result<Self> {
        if value.is_empty() || value.trim() != value {
            return Err(invalid_origin(
                "origin must be non-empty and contain no surrounding whitespace",
            ));
        }
        let lowercase = value.to_ascii_lowercase();
        if value.contains(['*', '{', '}'])
            || lowercase.contains("%2a")
            || lowercase.contains("%7b")
            || lowercase.contains("%7d")
        {
            return Err(invalid_origin("wildcards and templates are not allowed"));
        }
        let parsed = Url::parse(value).map_err(|_| invalid_origin("origin is not a valid URL"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(invalid_origin("scheme must be http or https"));
        }
        let suffix = value
            .split_once("://")
            .map(|(_, suffix)| suffix)
            .unwrap_or_default();
        let authority_end = suffix.find(['/', '?', '#']).unwrap_or(suffix.len());
        let authority = &suffix[..authority_end];
        if authority.contains('@') || !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(invalid_origin("userinfo is not allowed"));
        }
        if parsed.host().is_none() {
            return Err(invalid_origin("host is required"));
        }
        let path_end = suffix.find(['?', '#']).unwrap_or(suffix.len());
        let raw_path = &suffix[authority_end..path_end];
        if !matches!(raw_path, "" | "/") || parsed.path() != "/" {
            return Err(invalid_origin("path is not allowed"));
        }
        if parsed.query().is_some() {
            return Err(invalid_origin("query is not allowed"));
        }
        if parsed.fragment().is_some() {
            return Err(invalid_origin("fragment is not allowed"));
        }
        let Origin::Tuple(_, _, _) = parsed.origin() else {
            return Err(invalid_origin("origin must have a network authority"));
        };
        Ok(Self(parsed.origin().ascii_serialization()))
    }

    /// Returns the canonical ASCII origin without a trailing slash.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ConnectionOrigin {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

impl fmt::Display for ConnectionOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for ConnectionOrigin {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ConnectionOrigin {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

fn invalid_origin(reason: &'static str) -> Error {
    Error::InvalidConnectionOrigin { reason }
}

/// Exact immutable connector definition selected by one connection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConnectionDefinitionRef {
    /// Published artifact revision selected at installation.
    Artifact {
        /// Stable artifact family identity.
        artifact_uid: Uuid,
        /// Exact immutable revision identity.
        revision_uid: Uuid,
    },
    /// Code-owned managed connector definition.
    BuiltIn {
        /// Stable managed connector key.
        key: String,
        /// Positive code-owned definition version.
        version: NonZeroU64,
    },
}

impl ConnectionDefinitionRef {
    /// Builds a validated code-owned definition reference.
    pub fn built_in(key: impl Into<String>, version: u64) -> Result<Self> {
        let key = key.into();
        if key.trim().is_empty() || key.trim() != key {
            return Err(Error::InvalidContract {
                message: "built-in connector key must be non-empty and trimmed".to_string(),
            });
        }
        let version = NonZeroU64::new(version).ok_or_else(|| Error::InvalidContract {
            message: "built-in connector version must be positive".to_string(),
        })?;
        Ok(Self::BuiltIn { key, version })
    }
}

/// Closed code-owned connector definitions that may back a managed knowledge parent.
///
/// Keeping this as an enum prevents a knowledge link from turning an arbitrary
/// built-in key into a tenant connection without artifact release governance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedParentDefinition {
    /// Nango-linked knowledge account using the first managed contract version.
    KnowledgeNango,
    /// Merge-linked knowledge account using the first managed contract version.
    KnowledgeMerge,
}

impl ManagedParentDefinition {
    /// Resolves the only knowledge providers admitted by the managed-parent boundary.
    pub fn for_knowledge_provider(provider: &str) -> Result<Self> {
        match provider {
            "nango" => Ok(Self::KnowledgeNango),
            "merge" => Ok(Self::KnowledgeMerge),
            _ => Err(Error::UnsupportedManagedKnowledgeProvider),
        }
    }

    /// Returns the exact immutable built-in definition reference persisted on the parent.
    #[must_use]
    pub fn definition_ref(self) -> ConnectionDefinitionRef {
        ConnectionDefinitionRef::BuiltIn {
            key: self.key().to_string(),
            version: NonZeroU64::MIN,
        }
    }

    /// Returns the exact code-owned definition key.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::KnowledgeNango => "knowledge:nango",
            Self::KnowledgeMerge => "knowledge:merge",
        }
    }

    /// Returns the closed credential requirements for this knowledge parent.
    #[must_use]
    pub fn credential_requirements(self) -> Vec<RuntimeConnectorAuthRequirement> {
        match self {
            Self::KnowledgeNango => vec![RuntimeConnectorAuthRequirement::None],
            Self::KnowledgeMerge => {
                vec![RuntimeConnectorAuthRequirement::Bearer {
                    slot: CredentialSlotName::PRIMARY,
                }]
            }
        }
    }
}

/// One tenant-installed connector connection and its current generation-fenced state.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConnection {
    /// Stable tenant connection identity.
    pub connection_id: ConnectorConnectionId,
    /// Owning tenant isolation boundary.
    pub tenant_id: TenantId,
    /// Operator-visible connection label.
    pub display_name: String,
    /// Exact artifact or code-owned definition backing the connection.
    pub definition: ConnectionDefinitionRef,
    /// Fixed canonical HTTP origin for artifact-backed actions.
    ///
    /// Closed managed knowledge parents have no HTTP origin because they never
    /// expose connector actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ConnectionOrigin>,
    /// Secret-free connection configuration. Credential values are never represented here.
    pub non_secret_config: Value,
    /// Current optimistic-concurrency and binding generation.
    pub generation: ConnectionGeneration,
    /// Current lifecycle state.
    pub status: ConnectionStatus,
    /// Latest independent health observation.
    pub health: ConnectionHealth,
    /// Bounded secret-free health detail, when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_reason: Option<String>,
    /// Identity that created the connection, when the ingress has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_identity_id: Option<Uuid>,
    /// Owner identity granted by the transactional authorization outbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_identity_id: Option<Uuid>,
    /// Durable creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Timestamp of the latest generation-fenced state write.
    pub updated_at: DateTime<Utc>,
}

/// Durable result of claiming one replay-safe managed connector parent.
#[derive(Clone, Debug, PartialEq)]
pub struct ManagedParentClaim {
    /// Exact tenant connector parent selected by the claim.
    pub connection: ConnectorConnection,
    /// Whether this operation inserted the parent and therefore owns compensation.
    pub parent_created_by_claim: bool,
}

/// Why compensation preserved a managed connector parent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedParentPreservationReason {
    /// The claim attached to a parent that existed before this operation.
    PreExisting,
    /// A current action, knowledge projection, or direct grant still depends on the parent.
    DependentCapability,
}

/// Result of replay-safe managed-parent compensation.
#[derive(Clone, Debug, PartialEq)]
pub enum ManagedParentDeleteOutcome {
    /// This call marked the exact claim-created, unused parent deleted.
    Deleted(ConnectorConnection),
    /// The exact claim-created parent was already deleted by an earlier replay.
    AlreadyDeleted(ConnectorConnection),
    /// Deletion was intentionally skipped and the parent remains available.
    Preserved {
        /// Parent that was preserved.
        connection: ConnectorConnection,
        /// Durable reason deletion was not owned or safe.
        reason: ManagedParentPreservationReason,
    },
}

/// A 32-byte BLAKE3 digest serialized as 64 lowercase hexadecimal characters.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OperationContractHash([u8; 32]);

impl OperationContractHash {
    /// Builds a contract hash from raw digest bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for OperationContractHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for OperationContractHash {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        if value.len() != 64
            || value
                .bytes()
                .any(|byte| !matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(Error::InvalidContract {
                message: "contract hash must be 64 lowercase hexadecimal characters".to_string(),
            });
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = decode_hex_nibble(pair[0]);
            let low = decode_hex_nibble(pair[1]);
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for OperationContractHash {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for OperationContractHash {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

const fn decode_hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => 0,
    }
}

/// Canonical persisted operation contract compiled from one definition action.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledOperationContract {
    /// Stable action identifier from the connector definition.
    pub action_id: String,
    /// Complete secret-free connection authentication contract.
    pub auth: Vec<RuntimeConnectorAuthRequirement>,
    /// Fixed constrained-HTTP transport and governed policy contract.
    pub operation: HttpOperationContract,
}

impl CompiledOperationContract {
    /// Compiles the normalized persisted contract for one validated runtime action.
    pub fn compile(
        definition: &ConnectorDefinition,
        action: &RuntimeConnectorAction,
    ) -> Result<Self> {
        validate_connector_action_id(&action.id).map_err(|_| Error::InvalidContract {
            message: "connector action id must match [A-Za-z][A-Za-z0-9_-]{0,23}".to_string(),
        })?;
        if definition.auth.is_empty() {
            return Err(Error::InvalidContract {
                message: "connector auth requirements must not be empty".to_string(),
            });
        }
        if definition
            .auth
            .iter()
            .any(|requirement| matches!(requirement, RuntimeConnectorAuthRequirement::None))
            && !matches!(
                definition.auth.as_slice(),
                [RuntimeConnectorAuthRequirement::None]
            )
        {
            return Err(Error::InvalidContract {
                message: "none must be the sole connector auth requirement".to_string(),
            });
        }
        let mut declared_slots = HashSet::new();
        for slot in definition
            .auth
            .iter()
            .filter_map(RuntimeConnectorAuthRequirement::slot)
        {
            if !declared_slots.insert(slot.as_str()) {
                return Err(Error::InvalidContract {
                    message: format!("duplicate connector credential slot `{slot}`"),
                });
            }
        }
        if let Some(slot) = action.contract.credential_slot.as_ref()
            && !declared_slots.contains(slot.as_str())
        {
            return Err(Error::CredentialSlotMissing { slot: slot.clone() });
        }
        let mut auth = definition.auth.clone();
        auth.sort_by_key(auth_requirement_sort_key);
        Ok(Self {
            action_id: action.id.clone(),
            auth,
            operation: action.contract.clone(),
        })
    }

    /// Serializes the contract with deterministic recursive JSON object-key ordering.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        canonical_json_bytes(self).map_err(Error::from)
    }

    /// Returns the domain-separated hash of the canonical contract bytes.
    pub fn hash(&self) -> Result<OperationContractHash> {
        let canonical = self.canonical_bytes()?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(OPERATION_CONTRACT_HASH_DOMAIN.as_bytes());
        hasher.update(&[0]);
        hasher.update(&canonical);
        Ok(OperationContractHash::from_bytes(
            *hasher.finalize().as_bytes(),
        ))
    }
}

fn auth_requirement_sort_key(requirement: &RuntimeConnectorAuthRequirement) -> String {
    match requirement {
        RuntimeConnectorAuthRequirement::None => "0:none".to_string(),
        RuntimeConnectorAuthRequirement::Bearer { slot } => format!("1:{slot}"),
        RuntimeConnectorAuthRequirement::ApiKeyHeader { slot, header } => {
            format!("2:{slot}:{header}")
        }
        RuntimeConnectorAuthRequirement::ManagedOauth { slot } => format!("3:{slot}"),
    }
}

macro_rules! uuid_domain_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }
    };
}

uuid_domain_id!(
    /// Stable identity of one installed action binding.
    InstalledActionBindingId
);
uuid_domain_id!(
    /// Stable identity of one replay-safe connector invocation.
    ConnectorInvocationId
);

/// Immutable action binding compiled for one exact connection generation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledActionBinding {
    /// Stable binding row identity.
    pub binding_id: InstalledActionBindingId,
    /// Owning tenant isolation boundary.
    pub tenant_id: TenantId,
    /// Owning installed connection.
    pub connection_id: ConnectorConnectionId,
    /// Exact connection generation that produced this binding.
    pub connection_generation: ConnectionGeneration,
    /// Stable definition-local action identifier.
    pub action_id: String,
    /// Canonical fixed operation and governed policy contract.
    pub compiled_contract: CompiledOperationContract,
    /// Hash of the canonical compiled contract bytes.
    pub contract_hash: OperationContractHash,
    /// Policy-facing immutable capability revision.
    pub governed_contract_revision: String,
    /// Definition-enforced minimum action-policy effect.
    pub minimum_effect: ActionPolicyEffect,
    /// Whether this immutable generation binding is catalog-visible.
    pub enabled: bool,
}

impl InstalledActionBinding {
    /// Verifies that identity and stored hash agree with the compiled contract.
    pub fn validate(&self) -> Result<()> {
        if self.action_id != self.compiled_contract.action_id {
            return Err(Error::InvalidContract {
                message: "binding action id differs from compiled contract action id".to_string(),
            });
        }
        if self.governed_contract_revision.trim().is_empty() {
            return Err(Error::InvalidContract {
                message: "governed contract revision must be non-empty".to_string(),
            });
        }
        if self.minimum_effect != ActionPolicyEffect::AdminReview {
            return Err(Error::InvalidContract {
                message: "HTTP connector binding minimum effect must require admin review"
                    .to_string(),
            });
        }
        let actual = self.compiled_contract.hash()?;
        if actual != self.contract_hash {
            return Err(Error::ContractHashMismatch {
                expected: self.contract_hash,
                actual,
            });
        }
        Ok(())
    }
}

/// Durable state of a replay-safe connector invocation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorInvocationState {
    /// The durable replay key is owned but no request bytes may be transmitted yet.
    Reserved,
    /// Transport has taken ownership and request transmission may have begun.
    Transmitting,
    /// The operation completed and produced secret-free output metadata.
    Succeeded,
    /// The operation failed before any request bytes were transmitted.
    FailedBeforeSend,
    /// The operation completed with a known upstream failure.
    Failed,
    /// Transmission may have occurred but no authoritative outcome is known.
    UnknownOutcome,
}

impl ConnectorInvocationState {
    /// Returns the stable database and audit representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Transmitting => "transmitting",
            Self::Succeeded => "succeeded",
            Self::FailedBeforeSend => "failed_before_send",
            Self::Failed => "failed",
            Self::UnknownOutcome => "unknown_outcome",
        }
    }

    /// Returns whether this state is terminal and replayable without redispatch.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Reserved | Self::Transmitting)
    }

    /// Returns whether the requested invocation edge preserves send uncertainty.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Reserved, Self::Transmitting | Self::FailedBeforeSend)
                | (
                    Self::Transmitting,
                    Self::Succeeded | Self::Failed | Self::UnknownOutcome
                )
        )
    }

    /// Applies one exact invocation edge or returns a typed state conflict.
    pub fn transition(self, invocation_id: ConnectorInvocationId, next: Self) -> Result<Self> {
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(Error::InvocationStateConflict {
                invocation_id,
                from: self,
                to: next,
            })
        }
    }
}

impl fmt::Display for ConnectorInvocationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Secret-free terminal result persisted for replay.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConnectorInvocationTerminal {
    /// Successful invocation output metadata.
    Succeeded {
        /// Bounded secret-free structured result metadata.
        output_metadata: Value,
    },
    /// Failure known to have occurred before transmission.
    FailedBeforeSend {
        /// Structured redacted failure metadata.
        error_metadata: Value,
    },
    /// Known terminal upstream or runtime failure.
    Failed {
        /// Structured redacted failure metadata.
        error_metadata: Value,
    },
    /// Unsafe-to-retry uncertainty after possible transmission.
    UnknownOutcome {
        /// Structured redacted uncertainty metadata.
        error_metadata: Value,
    },
}

impl ConnectorInvocationTerminal {
    /// Returns the persisted terminal state represented by this result.
    #[must_use]
    pub const fn state(&self) -> ConnectorInvocationState {
        match self {
            Self::Succeeded { .. } => ConnectorInvocationState::Succeeded,
            Self::FailedBeforeSend { .. } => ConnectorInvocationState::FailedBeforeSend,
            Self::Failed { .. } => ConnectorInvocationState::Failed,
            Self::UnknownOutcome { .. } => ConnectorInvocationState::UnknownOutcome,
        }
    }
}

/// Durable secret-free connector invocation record used for replay decisions.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorInvocationRecord {
    /// Stable invocation row identity.
    pub invocation_id: ConnectorInvocationId,
    /// Owning tenant isolation boundary.
    pub tenant_id: TenantId,
    /// Pinned connection identity.
    pub connection_id: ConnectorConnectionId,
    /// Pinned installed binding identity.
    pub binding_id: InstalledActionBindingId,
    /// Pinned connection generation.
    pub connection_generation: ConnectionGeneration,
    /// Stable model/provider tool-call identity and replay key.
    pub tool_call_id: String,
    /// Hash of the canonical invocation request.
    pub request_hash: OperationContractHash,
    /// Optional upstream idempotency key derived by trusted host code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_idempotency_key: Option<String>,
    /// Current durable invocation state.
    pub state: ConnectorInvocationState,
    /// Structured redacted failure metadata for failure terminal states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_metadata: Option<Value>,
    /// Structured secret-free result metadata for successful terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_metadata: Option<Value>,
    /// Durable start timestamp.
    pub started_at: DateTime<Utc>,
    /// Durable terminal timestamp, absent while reserved or transmitting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// Timestamp of the latest state write.
    pub updated_at: DateTime<Utc>,
}
