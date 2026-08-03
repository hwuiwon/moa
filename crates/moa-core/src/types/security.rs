//! Shared sensitivity and prompt-injection-circuit vocabulary.
//!
//! Two vocabularies live here. [`SensitivityClass`] is the storage/retrieval/egress
//! data class. The rest of the module is the typed prompt-injection security
//! circuit: the assessment class a classified tool output carries, the exact
//! circuit owner, the canonical capability identity a circuit is keyed by, and
//! the replay-stable transition an owner journals when a capability's additive
//! score crosses a stage boundary.
//!
//! This module is deliberately pure vocabulary plus deterministic derivation. It
//! holds no detection policy: `moa-security` owns the single carrier-aware
//! classifier and the transition function, so nothing in `moa-core` depends back
//! on the detector.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::canonical_json::canonical_json_bytes;
use crate::error::MoaError;
use crate::types::identifiers::{ConnectorConnectionId, SessionId, ToolCallId};
use crate::types::worker::state::WorkerId;

/// Sensitivity class attached to data throughout MOA.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SensitivityClass {
    /// No sensitive data is known to be present.
    #[default]
    None,
    /// Personally identifiable information.
    Pii,
    /// Protected health information.
    Phi,
    /// Restricted data requiring explicit policy handling.
    Restricted,
}

impl SensitivityClass {
    /// Whether this classification requires content to be sealed at rest.
    ///
    /// Restricted and PHI rows store envelope ciphertext, leaving only a
    /// placeholder in the indexed plaintext columns. Every path that decides
    /// whether content can be read, embedded, or reconstructed asks this one
    /// question — the graph write path that refuses to embed sealed content and
    /// the read path that opens it. Any embedding caller must reject the
    /// placeholder rather than treat it as source text. A second copy of the
    /// rule would let those paths disagree about what is sealed.
    #[must_use]
    pub const fn is_sealed(self) -> bool {
        matches!(self, Self::Restricted | Self::Phi)
    }

    /// Returns the canonical lowercase representation used by SQL and providers.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pii => "pii",
            Self::Phi => "phi",
            Self::Restricted => "restricted",
        }
    }

    /// Returns the stable ordering rank used by sensitivity ceilings.
    #[must_use]
    pub const fn rank(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Pii => 1,
            Self::Phi => 2,
            Self::Restricted => 3,
        }
    }
}

impl std::fmt::Display for SensitivityClass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SensitivityClass {
    type Err = MoaError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "pii" => Ok(Self::Pii),
            "phi" => Ok(Self::Phi),
            "restricted" => Ok(Self::Restricted),
            other => Err(MoaError::ConfigError(format!(
                "unknown sensitivity class '{other}'"
            ))),
        }
    }
}

/// Mandatory versioned prompt-injection detector policy revision.
///
/// The policy has no opt-out and no compatibility mode: every classified output
/// is stamped with this revision so a stored assessment can always be attributed
/// to the exact detector that produced it.
pub const PROMPT_INJECTION_DETECTOR_REVISION: &str = "moa.prompt-injection.v1";

/// Schema version embedded in every circuit transition digest.
pub const PROMPT_INJECTION_CIRCUIT_SCHEMA_VERSION: &str = "v1";

/// Domain separator for the circuit transition digest.
const TRANSITION_DIGEST_DOMAIN: &str = "moa.prompt-injection-circuit.transition.v1";

/// Prefix of the replay-stable Session transition key.
const TRANSITION_KEY_PREFIX: &str = "prompt_injection_circuit:v1:";

/// Fixed UUIDv5 namespace for prompt-injection circuit security events.
///
/// Constant rather than generated so a replayed transition derives the identical
/// event UUID on every attempt.
pub const PROMPT_INJECTION_EVENT_NAMESPACE: Uuid =
    Uuid::from_u128(0x6d6f_615f_7069_6300_9e3a_41d5_b7c8_0001);

/// Typed classification assigned to one tool output by the injection detector.
///
/// The discriminants are the additive owner/capability score contributions, not
/// an ordering: two independent `SuspiciousInstruction` outputs sum to the same
/// score as one `ConfirmedInjection`, which is the intended escalation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputAssessmentClass {
    /// Nothing in the output resembles an instruction or protected material.
    Safe,
    /// The output contains instruction-shaped text addressed at the model.
    SuspiciousInstruction,
    /// The output is a recognizable prompt-injection attempt.
    ConfirmedInjection,
    /// The output leaked a protected canary token.
    CanaryLeak,
    /// The output carries restricted-class or secret-shaped material.
    RestrictedOrSecretOutput,
}

impl OutputAssessmentClass {
    /// Returns the additive score this class contributes to its capability.
    #[must_use]
    pub const fn score(self) -> u32 {
        match self {
            Self::Safe => 0,
            Self::SuspiciousInstruction => 1,
            Self::ConfirmedInjection => 2,
            Self::CanaryLeak | Self::RestrictedOrSecretOutput => 4,
        }
    }

    /// Returns the canonical lowercase label used by events, OCSF, and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::SuspiciousInstruction => "suspicious_instruction",
            Self::ConfirmedInjection => "confirmed_injection",
            Self::CanaryLeak => "canary_leak",
            Self::RestrictedOrSecretOutput => "restricted_or_secret_output",
        }
    }

    /// Returns whether this class requires clearing every raw output carrier.
    ///
    /// Suspicious spans are redacted in place because the surrounding output is
    /// still useful; the three higher classes destroy the output entirely
    /// regardless of the capability's current score, so a first-strike canary
    /// leak cannot reach the model just because the circuit was still clear.
    #[must_use]
    pub const fn clears_raw_carriers(self) -> bool {
        matches!(
            self,
            Self::ConfirmedInjection | Self::CanaryLeak | Self::RestrictedOrSecretOutput
        )
    }
}

impl fmt::Display for OutputAssessmentClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Stable low-cardinality detector signal recorded alongside an assessment.
///
/// Signals are an enum rather than free text so a persisted assessment, an OCSF
/// finding, and a metric label can never carry attacker-controlled bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionSignal {
    /// Text instructing the model to disregard prior instructions.
    IgnorePreviousInstructions,
    /// Text reassigning the model's identity or role.
    IdentityReassignment,
    /// A forged conversation-role prefix.
    SpoofedRole,
    /// A forged chat-template delimiter token.
    DelimiterToken,
    /// Text soliciting the hidden system prompt.
    PromptExfiltration,
    /// Text soliciting the protected canary token.
    CanaryExfiltration,
    /// A protected canary token appeared in the output.
    CanaryToken,
    /// Credential-shaped or secret-shaped material appeared in the output.
    SecretMaterial,
    /// The output claims restricted data-handling class.
    RestrictedClass,
    /// The untrusted-output boundary delimiter was forged.
    ForgedOutputBoundary,
}

impl InjectionSignal {
    /// Returns the canonical lowercase label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IgnorePreviousInstructions => "ignore_previous_instructions",
            Self::IdentityReassignment => "identity_reassignment",
            Self::SpoofedRole => "spoofed_role",
            Self::DelimiterToken => "delimiter_token",
            Self::PromptExfiltration => "prompt_exfiltration",
            Self::CanaryExfiltration => "canary_exfiltration",
            Self::CanaryToken => "canary_token",
            Self::SecretMaterial => "secret_material",
            Self::RestrictedClass => "restricted_class",
            Self::ForgedOutputBoundary => "forged_output_boundary",
        }
    }
}

impl fmt::Display for InjectionSignal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Complete security metadata attached to one classified tool output.
///
/// Never optional anywhere it appears: an output that reaches a durable surface
/// without an assessment would be an unclassified output, which the circuit has
/// no way to distinguish from a safe one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOutputAssessment {
    /// Typed class assigned by the detector.
    pub class: OutputAssessmentClass,
    /// Detector policy revision that produced this assessment.
    pub detector_revision: String,
    /// Stable signals that matched, sorted and deduplicated.
    pub signals: Vec<InjectionSignal>,
    /// Number of suspicious spans replaced in place.
    pub redacted_spans: u32,
    /// Number of duplicate carrier bodies collapsed before scoring.
    pub deduplicated_carriers: u32,
}

impl ToolOutputAssessment {
    /// Returns the assessment a benign, unmodified output carries.
    #[must_use]
    pub fn safe() -> Self {
        Self {
            class: OutputAssessmentClass::Safe,
            detector_revision: PROMPT_INJECTION_DETECTOR_REVISION.to_string(),
            signals: Vec::new(),
            redacted_spans: 0,
            deduplicated_carriers: 0,
        }
    }

    /// Returns whether this assessment contributes any score to its capability.
    #[must_use]
    pub const fn is_safe(&self) -> bool {
        matches!(self.class, OutputAssessmentClass::Safe)
    }
}

/// Canonical capability identity resolved by the tool router.
///
/// Resolved from the registry, never from a caller-supplied name, and stable
/// across Hand provider fallback: one logical Hand capability keeps one identity
/// whichever sandbox provider ultimately served it, so an attacker cannot reset a
/// tripped circuit by forcing a fallback.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolCapabilityId {
    /// A process-local built-in tool.
    BuiltIn {
        /// Registered tool name.
        tool: String,
    },
    /// A remote MCP tool, identified by its configured server and remote name.
    Mcp {
        /// Configured MCP server name.
        server: String,
        /// Remote tool name.
        tool: String,
    },
    /// An action exposed by one exact tenant-installed connector connection.
    InstalledConnectorAction {
        /// Tenant connection selected by the agent's typed connector binding.
        connection_id: ConnectorConnectionId,
        /// Definition-local canonical action identifier.
        action_id: String,
    },
    /// A sandbox-executed capability, independent of which provider served it.
    Hand {
        /// Registered tool name.
        tool: String,
    },
}

impl ToolCapabilityId {
    /// Names a built-in tool capability.
    #[must_use]
    pub fn builtin(tool: impl Into<String>) -> Self {
        Self::BuiltIn { tool: tool.into() }
    }

    /// Names one remote MCP server's tool capability.
    #[must_use]
    pub fn mcp(server: impl Into<String>, tool: impl Into<String>) -> Self {
        Self::Mcp {
            server: server.into(),
            tool: tool.into(),
        }
    }

    /// Names one action on an exact tenant-installed connector connection.
    #[must_use]
    pub fn installed_connector_action(
        connection_id: ConnectorConnectionId,
        action_id: impl Into<String>,
    ) -> Self {
        Self::InstalledConnectorAction {
            connection_id,
            action_id: action_id.into(),
        }
    }

    /// Names one logical sandbox capability, independent of serving provider.
    #[must_use]
    pub fn hand(tool: impl Into<String>) -> Self {
        Self::Hand { tool: tool.into() }
    }

    /// Returns the stable, injective identity used in keys, events, and findings.
    ///
    /// Every attacker-controlled coordinate is byte-length-framed. This keeps
    /// distinct MCP `(server, tool)` pairs distinct even when either name
    /// contains the `:` separator.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::BuiltIn { tool } => format!("builtin:{}:{tool}", tool.len()),
            Self::Mcp { server, tool } => {
                format!("mcp:{}:{server}:{}:{tool}", server.len(), tool.len())
            }
            Self::InstalledConnectorAction {
                connection_id,
                action_id,
            } => format!(
                "connector_action:{}:{}:{action_id}",
                connection_id.0.simple(),
                action_id.len()
            ),
            Self::Hand { tool } => format!("hand:{}:{tool}", tool.len()),
        }
    }
}

impl PartialOrd for ToolCapabilityId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ToolCapabilityId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let rank = |capability: &Self| match capability {
            Self::BuiltIn { .. } => 0_u8,
            Self::Mcp { .. } => 1,
            Self::InstalledConnectorAction { .. } => 2,
            Self::Hand { .. } => 3,
        };
        rank(self)
            .cmp(&rank(other))
            .then_with(|| match (self, other) {
                (Self::BuiltIn { tool: left }, Self::BuiltIn { tool: right })
                | (Self::Hand { tool: left }, Self::Hand { tool: right }) => left.cmp(right),
                (
                    Self::Mcp {
                        server: left_server,
                        tool: left_tool,
                    },
                    Self::Mcp {
                        server: right_server,
                        tool: right_tool,
                    },
                ) => left_server
                    .cmp(right_server)
                    .then(left_tool.cmp(right_tool)),
                (
                    Self::InstalledConnectorAction {
                        connection_id: left_connection,
                        action_id: left_action,
                    },
                    Self::InstalledConnectorAction {
                        connection_id: right_connection,
                        action_id: right_action,
                    },
                ) => left_connection
                    .0
                    .cmp(&right_connection.0)
                    .then(left_action.cmp(right_action)),
                _ => std::cmp::Ordering::Equal,
            })
    }
}

impl fmt::Display for ToolCapabilityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.render())
    }
}

/// Exact owner of one prompt-injection circuit.
///
/// Every variant carries the generation that fences it. State belongs to a
/// generation, so it resets only when a genuinely new owner generation starts —
/// never for a new input fingerprint, a new tool argument, a fallback Hand
/// provider, or a workflow replay.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SecurityCircuitOwner {
    /// The root coordinator turn.
    Coordinator {
        /// Owning `TurnExecution` workflow ID.
        turn_id: String,
        /// Owner generation fence.
        generation: u64,
    },
    /// One worker's turn.
    Worker {
        /// Durable worker identifier.
        worker_id: WorkerId,
        /// Owning worker turn workflow ID.
        turn_id: String,
        /// Owner generation fence.
        generation: u64,
    },
    /// One dynamic execution task's agent turn.
    ExecutionTask {
        /// Durable execution-run identifier.
        run_uid: Uuid,
        /// Durable task identifier.
        task_uid: Uuid,
        /// Task generation fence.
        generation: u64,
    },
}

impl SecurityCircuitOwner {
    /// Returns this owner's generation fence.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        match self {
            Self::Coordinator { generation, .. }
            | Self::Worker { generation, .. }
            | Self::ExecutionTask { generation, .. } => *generation,
        }
    }

    /// Returns the stable low-cardinality owner kind label.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Coordinator { .. } => "coordinator",
            Self::Worker { .. } => "worker",
            Self::ExecutionTask { .. } => "execution_task",
        }
    }

    /// Returns whether two owners are the same logical owner at the same generation.
    ///
    /// A delayed action-review continuation runs under a new workflow ID but
    /// retains the original logical owner, so identity is compared on the owner
    /// coordinates rather than on the workflow that happens to be executing.
    #[must_use]
    pub fn is_same_generation(&self, other: &Self) -> bool {
        self == other
    }
}

/// Stage one capability's circuit has reached under its owner.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SecurityCircuitStage {
    /// No assessment has scored against this capability.
    #[default]
    Clear,
    /// One warning was emitted; the capability still dispatches.
    Warned,
    /// The capability is disabled and cannot dispatch again under this owner.
    Disabled,
    /// The owner is suspended awaiting user input about the attack.
    SuspendedForInput,
    /// The owner is halted.
    Halted,
}

impl SecurityCircuitStage {
    /// Returns the stage an accumulated score has reached.
    #[must_use]
    pub const fn for_score(score: u32) -> Self {
        match score {
            0 => Self::Clear,
            1 => Self::Warned,
            2 => Self::Disabled,
            3 => Self::SuspendedForInput,
            _ => Self::Halted,
        }
    }

    /// Returns the canonical lowercase label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::Warned => "warned",
            Self::Disabled => "disabled",
            Self::SuspendedForInput => "suspended_for_input",
            Self::Halted => "halted",
        }
    }

    /// Returns whether a capability at this stage may still be dispatched.
    #[must_use]
    pub const fn permits_dispatch(self) -> bool {
        matches!(self, Self::Clear | Self::Warned)
    }
}

impl fmt::Display for SecurityCircuitStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Per-capability circuit state under one owner generation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityCircuitCapabilityState {
    /// Accumulated additive score.
    pub score: u32,
    /// Tool calls already applied, in sorted order.
    pub applied_tool_calls: Vec<ToolCallId>,
}

impl SecurityCircuitCapabilityState {
    /// Returns the stage derived from the accumulated score.
    #[must_use]
    pub const fn stage(&self) -> SecurityCircuitStage {
        SecurityCircuitStage::for_score(self.score)
    }
}

/// One owner's complete prompt-injection circuit.
///
/// Held on the Session and Worker virtual objects. The map belongs to exactly one
/// owner generation: it is cleared only when a genuinely new owner generation
/// takes over, never for a new input fingerprint, a new tool argument, a fallback
/// Hand provider, or a workflow replay. That is the property that makes the
/// circuit hold across an attacker varying its payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityCircuitState {
    /// Logical owner the current capability map belongs to.
    pub owner: Option<SecurityCircuitOwner>,
    /// Per-capability circuit state, keyed by canonical typed capability id.
    #[serde(with = "capability_state_map")]
    pub capabilities: std::collections::BTreeMap<ToolCapabilityId, SecurityCircuitCapabilityState>,
}

/// Serde support for a map whose typed enum keys cannot be JSON object keys.
///
/// The durable shape is an ordered list of `(capability, state)` pairs. Duplicate
/// capabilities are rejected instead of silently overwriting one circuit entry.
mod capability_state_map {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

    use super::{SecurityCircuitCapabilityState, ToolCapabilityId};

    pub(super) fn serialize<S>(
        value: &BTreeMap<ToolCapabilityId, SecurityCircuitCapabilityState>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<BTreeMap<ToolCapabilityId, SecurityCircuitCapabilityState>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let entries =
            Vec::<(ToolCapabilityId, SecurityCircuitCapabilityState)>::deserialize(deserializer)?;
        let mut value = BTreeMap::new();
        for (capability, state) in entries {
            if value.insert(capability, state).is_some() {
                return Err(D::Error::custom(
                    "duplicate capability in security circuit state",
                ));
            }
        }
        Ok(value)
    }
}

impl SecurityCircuitState {
    /// Returns whether this capability may still be dispatched under `owner`.
    ///
    /// An owner that does not match the stored one has no accumulated state yet,
    /// so it dispatches freely; that is a new generation starting clean, not a
    /// bypass, because [`Self::adopt_owner`] is what installs the new generation.
    #[must_use]
    pub fn permits_dispatch(
        &self,
        owner: &SecurityCircuitOwner,
        capability: &ToolCapabilityId,
    ) -> bool {
        if self.owner.as_ref() != Some(owner) {
            return true;
        }
        self.capabilities
            .get(capability)
            .is_none_or(|state| state.stage().permits_dispatch())
    }

    /// Returns the stage this capability has reached under `owner`.
    #[must_use]
    pub fn stage(
        &self,
        owner: &SecurityCircuitOwner,
        capability: &ToolCapabilityId,
    ) -> SecurityCircuitStage {
        if self.owner.as_ref() != Some(owner) {
            return SecurityCircuitStage::Clear;
        }
        self.capabilities
            .get(capability)
            .map(SecurityCircuitCapabilityState::stage)
            .unwrap_or_default()
    }

    /// Installs `owner` as the current generation, clearing state only if it changed.
    ///
    /// Idempotent for the same owner, which is what makes it safe to call on every
    /// replayed step of one turn.
    pub fn adopt_owner(&mut self, owner: &SecurityCircuitOwner) {
        if self.owner.as_ref() == Some(owner) {
            return;
        }
        self.owner = Some(owner.clone());
        self.capabilities.clear();
    }

    /// Returns the stored state for one capability under the current owner.
    #[must_use]
    pub fn capability_state(
        &self,
        capability: &ToolCapabilityId,
    ) -> SecurityCircuitCapabilityState {
        self.capabilities
            .get(capability)
            .cloned()
            .unwrap_or_default()
    }

    /// Stores the next state for one capability under the current owner.
    pub fn set_capability_state(
        &mut self,
        capability: &ToolCapabilityId,
        state: SecurityCircuitCapabilityState,
    ) {
        self.capabilities.insert(capability.clone(), state);
    }
}

/// Exact replay-stable transition produced by applying one assessment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityCircuitTransition {
    /// Owner whose circuit advanced.
    pub owner: SecurityCircuitOwner,
    /// Capability whose circuit advanced.
    pub capability: ToolCapabilityId,
    /// Tool call whose assessment caused the advance.
    pub tool_call_id: ToolCallId,
    /// Assessment class that caused the advance.
    pub class: OutputAssessmentClass,
    /// Detector revision that produced the assessment.
    pub detector_revision: String,
    /// Stage before the assessment was applied.
    pub prior_stage: SecurityCircuitStage,
    /// Stage reached after the assessment was applied.
    pub reached_stage: SecurityCircuitStage,
    /// Accumulated score before the assessment was applied.
    pub prior_score: u32,
    /// Accumulated score after the assessment was applied.
    pub reached_score: u32,
    /// Replay-stable Session transition key.
    pub key: String,
}

impl SecurityCircuitTransition {
    /// Returns the deterministic security-event UUID for this transition.
    ///
    /// Derived by UUIDv5 from a fixed namespace and the transition key so a
    /// replayed owner produces the identical event identity instead of
    /// generating a fresh one.
    #[must_use]
    pub fn event_uuid(&self) -> Uuid {
        Uuid::new_v5(&PROMPT_INJECTION_EVENT_NAMESPACE, self.key.as_bytes())
    }
}

/// Coordinates the replay-stable transition key is derived from.
#[derive(Debug, Clone, Copy)]
pub struct TransitionKeyInput<'a> {
    /// Session that owns the transition.
    pub session_id: SessionId,
    /// Exact circuit owner.
    pub owner: &'a SecurityCircuitOwner,
    /// Canonical capability identity.
    pub capability: &'a ToolCapabilityId,
    /// Triggering tool call.
    pub tool_call_id: ToolCallId,
    /// Stage before the transition.
    pub prior_stage: SecurityCircuitStage,
    /// Stage reached by the transition.
    pub reached_stage: SecurityCircuitStage,
}

/// Adds one unambiguous length-framed field to a transition digest.
fn hash_transition_field(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_be_bytes());
    hasher.update(value);
}

/// Derives the replay-stable Session transition key.
///
/// The digest uses the capability's canonical injective rendering followed by
/// length-framed canonical JSON for the remaining coordinates. It therefore
/// depends only on the logical transition — never wall-clock time, attempt
/// count, or field ordering. Two replays of one transition collapse onto one
/// Session fact and one security event.
#[must_use]
pub fn transition_key(input: TransitionKeyInput<'_>) -> String {
    let payload = serde_json::json!({
        "schema_version": PROMPT_INJECTION_CIRCUIT_SCHEMA_VERSION,
        "session_id": input.session_id.0,
        "owner": input.owner,
        "tool_call_id": input.tool_call_id.0,
        "prior_stage": input.prior_stage.as_str(),
        "reached_stage": input.reached_stage.as_str(),
    });
    let canonical = canonical_json_bytes(&payload)
        .expect("canonical serialization of owned JSON values cannot fail");

    let mut hasher = blake3::Hasher::new();
    hasher.update(TRANSITION_DIGEST_DOMAIN.as_bytes());
    hasher.update(b"\x00");
    hash_transition_field(&mut hasher, input.capability.render().as_bytes());
    hash_transition_field(&mut hasher, &canonical);
    format!("{TRANSITION_KEY_PREFIX}{}", hasher.finalize().to_hex())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::SensitivityClass;

    #[test]
    fn sensitivity_class_has_one_canonical_string_and_rank() {
        // Pins: SQL, graph, vector, classifier, and MCP policy code share exactly
        // one ordered none/pii/phi/restricted vocabulary.
        let expected = [
            (SensitivityClass::None, "none", 0),
            (SensitivityClass::Pii, "pii", 1),
            (SensitivityClass::Phi, "phi", 2),
            (SensitivityClass::Restricted, "restricted", 3),
        ];

        for (class, name, rank) in expected {
            assert_eq!(class.as_str(), name);
            assert_eq!(class.rank(), rank);
            assert_eq!(
                SensitivityClass::from_str(name).expect("canonical class"),
                class
            );
        }
    }

    mod circuit {
        use uuid::Uuid;

        use crate::types::identifiers::{SessionId, ToolCallId};
        use crate::types::security::{
            OutputAssessmentClass, SecurityCircuitCapabilityState, SecurityCircuitOwner,
            SecurityCircuitStage, SecurityCircuitState, ToolCapabilityId, TransitionKeyInput,
            transition_key,
        };

        fn owner() -> SecurityCircuitOwner {
            SecurityCircuitOwner::Coordinator {
                turn_id: "turn-alpha".to_string(),
                generation: 7,
            }
        }

        #[test]
        fn assessment_class_scores_match_the_additive_circuit_contract() {
            // Pins: the exact additive weights the stage thresholds are calibrated
            // against. Changing a weight silently re-tunes when every owner trips.
            assert_eq!(OutputAssessmentClass::Safe.score(), 0);
            assert_eq!(OutputAssessmentClass::SuspiciousInstruction.score(), 1);
            assert_eq!(OutputAssessmentClass::ConfirmedInjection.score(), 2);
            assert_eq!(OutputAssessmentClass::CanaryLeak.score(), 4);
            assert_eq!(OutputAssessmentClass::RestrictedOrSecretOutput.score(), 4);
        }

        #[test]
        fn only_suspicious_output_keeps_its_raw_carriers() {
            // Pins: suspicious spans are redacted in place, but the three higher
            // classes destroy every raw carrier regardless of the current score, so
            // a first-strike canary leak never reaches a durable surface intact.
            assert!(!OutputAssessmentClass::Safe.clears_raw_carriers());
            assert!(!OutputAssessmentClass::SuspiciousInstruction.clears_raw_carriers());
            assert!(OutputAssessmentClass::ConfirmedInjection.clears_raw_carriers());
            assert!(OutputAssessmentClass::CanaryLeak.clears_raw_carriers());
            assert!(OutputAssessmentClass::RestrictedOrSecretOutput.clears_raw_carriers());
        }

        #[test]
        fn stage_thresholds_and_dispatch_permission_are_exact() {
            // Pins: score 1 warns, 2 disables, 3 suspends, >=4 halts, and only the
            // first two stages may dispatch again.
            assert_eq!(
                SecurityCircuitStage::for_score(0),
                SecurityCircuitStage::Clear
            );
            assert_eq!(
                SecurityCircuitStage::for_score(1),
                SecurityCircuitStage::Warned
            );
            assert_eq!(
                SecurityCircuitStage::for_score(2),
                SecurityCircuitStage::Disabled
            );
            assert_eq!(
                SecurityCircuitStage::for_score(3),
                SecurityCircuitStage::SuspendedForInput
            );
            for score in [4_u32, 5, 9, u32::MAX] {
                assert_eq!(
                    SecurityCircuitStage::for_score(score),
                    SecurityCircuitStage::Halted,
                    "score {score} must halt"
                );
            }

            assert!(SecurityCircuitStage::Clear.permits_dispatch());
            assert!(SecurityCircuitStage::Warned.permits_dispatch());
            assert!(!SecurityCircuitStage::Disabled.permits_dispatch());
            assert!(!SecurityCircuitStage::SuspendedForInput.permits_dispatch());
            assert!(!SecurityCircuitStage::Halted.permits_dispatch());
        }

        #[test]
        fn hand_capability_identity_is_independent_of_the_serving_provider() {
            // Pins: one logical Hand capability renders one identity. The router
            // resolves it from the registry, so fallback between sandbox providers
            // cannot mint a second capability key and reset a tripped circuit.
            let capability = ToolCapabilityId::Hand {
                tool: "bash".to_string(),
            };
            assert_eq!(capability.render(), "hand:4:bash");
            assert_eq!(
                ToolCapabilityId::BuiltIn {
                    tool: "file_read".to_string()
                }
                .render(),
                "builtin:9:file_read"
            );
            assert_eq!(
                ToolCapabilityId::Mcp {
                    server: "search".to_string(),
                    tool: "query".to_string()
                }
                .render(),
                "mcp:6:search:5:query"
            );
        }

        #[test]
        fn circuit_state_round_trips_typed_capability_keys() {
            // Pins: persisted circuit state keeps the complete typed capability
            // identity. Serializing through JSON must not flatten MCP server and
            // tool coordinates into an unchecked rendered string.
            let owner = owner();
            let capability = ToolCapabilityId::mcp("search", "query");
            let mut circuit = SecurityCircuitState::default();
            circuit.adopt_owner(&owner);
            circuit.set_capability_state(
                &capability,
                SecurityCircuitCapabilityState {
                    score: 2,
                    applied_tool_calls: vec![ToolCallId(Uuid::from_u128(0x55))],
                },
            );

            let encoded = serde_json::to_string(&circuit).expect("serialize circuit");
            let decoded: SecurityCircuitState =
                serde_json::from_str(&encoded).expect("deserialize circuit");

            assert_eq!(decoded, circuit);
            assert_eq!(
                decoded.stage(&owner, &capability),
                SecurityCircuitStage::Disabled
            );
            assert!(!decoded.permits_dispatch(&owner, &capability));
        }

        #[test]
        fn transition_key_is_deterministic_and_coordinate_sensitive() {
            // Pins: the key depends only on the logical transition coordinates, so
            // replay reproduces it byte for byte, and every coordinate is load
            // bearing — dropping one would collapse distinct transitions onto one
            // Session fact and one security event.
            let session_id = SessionId(Uuid::from_u128(0x51));
            let capability = ToolCapabilityId::Mcp {
                server: "search".to_string(),
                tool: "query".to_string(),
            };
            let tool_call_id = ToolCallId(Uuid::from_u128(0x77));
            let base = TransitionKeyInput {
                session_id,
                owner: &owner(),
                capability: &capability,
                tool_call_id,
                prior_stage: SecurityCircuitStage::Clear,
                reached_stage: SecurityCircuitStage::Disabled,
            };

            let key = transition_key(base);
            assert_eq!(key, transition_key(base), "replay must reproduce the key");
            assert!(
                key.starts_with("prompt_injection_circuit:v1:"),
                "unexpected key shape: {key}"
            );
            let digest = key
                .strip_prefix("prompt_injection_circuit:v1:")
                .expect("prefixed key");
            assert_eq!(digest.len(), 64, "digest must be 64 hex characters");
            assert!(
                digest
                    .chars()
                    .all(|character| character.is_ascii_digit()
                        || ('a'..='f').contains(&character)),
                "digest must be lowercase hex: {digest}"
            );

            let other_generation = SecurityCircuitOwner::Coordinator {
                turn_id: "turn-alpha".to_string(),
                generation: 8,
            };
            let other_capability = ToolCapabilityId::Mcp {
                server: "search".to_string(),
                tool: "other".to_string(),
            };
            let variants = [
                TransitionKeyInput {
                    session_id: SessionId(Uuid::from_u128(0x52)),
                    ..base
                },
                TransitionKeyInput {
                    owner: &other_generation,
                    ..base
                },
                TransitionKeyInput {
                    capability: &other_capability,
                    ..base
                },
                TransitionKeyInput {
                    tool_call_id: ToolCallId(Uuid::from_u128(0x78)),
                    ..base
                },
                TransitionKeyInput {
                    prior_stage: SecurityCircuitStage::Warned,
                    ..base
                },
                TransitionKeyInput {
                    reached_stage: SecurityCircuitStage::Halted,
                    ..base
                },
            ];
            for variant in variants {
                assert_ne!(
                    transition_key(variant),
                    key,
                    "every transition coordinate must change the key"
                );
            }
        }

        #[test]
        fn capability_render_and_transition_key_avoid_delimiter_collisions() {
            // Pins: the canonical identity used by OCSF, dashboards, and
            // transition hashing keeps MCP server and tool coordinates distinct
            // even when either contains the separator.
            let session_id = SessionId(Uuid::from_u128(0x51));
            let owner = owner();
            let left = ToolCapabilityId::mcp("a:b", "c");
            let right = ToolCapabilityId::mcp("a", "b:c");
            assert_ne!(
                left.render(),
                right.render(),
                "canonical capability rendering must be injective"
            );
            let left_input = TransitionKeyInput {
                session_id,
                owner: &owner,
                capability: &left,
                tool_call_id: ToolCallId(Uuid::from_u128(0x77)),
                prior_stage: SecurityCircuitStage::Clear,
                reached_stage: SecurityCircuitStage::Disabled,
            };
            let right_input = TransitionKeyInput {
                capability: &right,
                ..left_input
            };

            assert_ne!(transition_key(left_input), transition_key(right_input));
        }

        #[test]
        fn event_uuid_is_derived_from_the_transition_key_not_generated() {
            // Pins: the security-event identity is UUIDv5 over the replay-stable
            // key, so a crashed-and-replayed owner inserts one row instead of a
            // second freshly generated one.
            use crate::types::security::{
                PROMPT_INJECTION_DETECTOR_REVISION, SecurityCircuitTransition,
            };

            let capability = ToolCapabilityId::Hand {
                tool: "bash".to_string(),
            };
            let tool_call_id = ToolCallId(Uuid::from_u128(0x77));
            let key = transition_key(TransitionKeyInput {
                session_id: SessionId(Uuid::from_u128(0x51)),
                owner: &owner(),
                capability: &capability,
                tool_call_id,
                prior_stage: SecurityCircuitStage::Clear,
                reached_stage: SecurityCircuitStage::Halted,
            });
            let transition = SecurityCircuitTransition {
                owner: owner(),
                capability,
                tool_call_id,
                class: OutputAssessmentClass::CanaryLeak,
                detector_revision: PROMPT_INJECTION_DETECTOR_REVISION.to_string(),
                prior_stage: SecurityCircuitStage::Clear,
                reached_stage: SecurityCircuitStage::Halted,
                prior_score: 0,
                reached_score: 4,
                key: key.clone(),
            };

            assert_eq!(transition.event_uuid(), transition.event_uuid());
            assert_eq!(
                transition.event_uuid(),
                Uuid::new_v5(
                    &crate::types::security::PROMPT_INJECTION_EVENT_NAMESPACE,
                    key.as_bytes()
                )
            );
        }
    }
}
