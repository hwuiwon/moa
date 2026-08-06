//! Immutable capability catalogs, authorization envelopes, estimates, and hashes.

use std::{fmt, str::FromStr};

use moa_artifacts::{
    document::ArtifactKind,
    execution_plan::{
        CapabilityReference, CompensationInputMapping, ExecutionCompensation,
        ExecutionPlanDefinition, PlanAmendment, PlanAmendmentOperation,
    },
    reference::ArtifactRef,
};
use moa_core::{
    canonical_json::canonical_json_bytes,
    types::{
        action_policy::{ActionClass, ActionPolicyEffect, RiskLevel},
        identifiers::{ConnectorConnectionId, TenantId},
        tools::IdempotencyClass,
    },
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use serde_json::Value;

use crate::{Error, Result};

/// Domain separator for immutable capability-catalog hashes.
pub const CATALOG_HASH_DOMAIN: &str = "moa.execution.catalog";
/// Domain separator for immutable plan hashes.
pub const PLAN_HASH_DOMAIN: &str = "moa.execution.plan";
/// Domain separator for plan-amendment hashes.
pub const AMENDMENT_HASH_DOMAIN: &str = "moa.execution.amendment";
/// Domain separator for semantic plan-amendment operation fingerprints.
pub const AMENDMENT_OPERATIONS_HASH_DOMAIN: &str = "moa.execution.amendment-operations";
/// Domain separator for normalized execution-failure hashes.
pub const FAILURE_HASH_DOMAIN: &str = "moa.execution.failure";
/// Domain separator for structured task-output hashes.
pub const TASK_OUTPUT_HASH_DOMAIN: &str = "moa.execution.task-output";

/// A 32-byte BLAKE3 digest serialized as 64 lowercase hexadecimal characters.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ExecutionHash([u8; 32]);

impl ExecutionHash {
    /// Builds an execution hash from its raw digest bytes.
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

impl fmt::Display for ExecutionHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&encode_hex(&self.0))
    }
}

impl FromStr for ExecutionHash {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        if value.len() != 64
            || value
                .bytes()
                .any(|byte| !matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(Error::InvalidHash {
                message: "expected 64 lowercase hexadecimal characters".to_string(),
            });
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = decode_hex_nibble(pair[0]).ok_or_else(|| Error::InvalidHash {
                message: "invalid high hexadecimal nibble".to_string(),
            })?;
            let low = decode_hex_nibble(pair[1]).ok_or_else(|| Error::InvalidHash {
                message: "invalid low hexadecimal nibble".to_string(),
            })?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

impl Serialize for ExecutionHash {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ExecutionHash {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

/// Immutable exact capability and skill allowlist captured before compilation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionAuthorizationEnvelope {
    /// Sorted, duplicate-free capability references authorized for the run.
    pub capability_refs: Vec<CapabilityReference>,
    /// Sorted, duplicate-free skill references authorized for the run.
    pub skill_refs: Vec<ArtifactRef>,
}

/// Immutable tenant- and policy-filtered capability catalog.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCapabilityCatalog {
    /// Sorted, duplicate-free governed capabilities.
    pub capabilities: Vec<ExecutionCapability>,
    /// Canonical hash of the capabilities.
    pub catalog_hash: ExecutionHash,
}

impl ExecutionCapabilityCatalog {
    /// Builds a deterministic catalog from invocable capabilities.
    ///
    /// Two entries that share a capability reference are rejected here rather
    /// than left for downstream validation. A reference is the identity a plan
    /// cites and an authorization envelope pins, so a catalog holding the same
    /// reference twice makes "which capability did this run invoke" answerable
    /// only by construction order — and the answer would change with it.
    pub fn build(capabilities: Vec<ExecutionCapability>) -> Result<Self> {
        let mut keyed = capabilities
            .into_iter()
            .map(|capability| {
                capability.validate_policy_context()?;
                Ok((canonical_sort_key(&capability.reference)?, capability))
            })
            .collect::<Result<Vec<_>>>()?;
        keyed.sort_by(|left, right| left.0.cmp(&right.0));
        if let Some(window) = keyed.windows(2).find(|window| window[0].0 == window[1].0) {
            return Err(Error::DuplicateCapabilityReference {
                reference: window[0].1.reference.name.clone(),
                version: window[0].1.reference.version.clone(),
            });
        }
        let capabilities = keyed
            .into_iter()
            .map(|(_, capability)| capability)
            .collect::<Vec<_>>();
        let catalog_hash = catalog_hash(&capabilities)?;
        Ok(Self {
            capabilities,
            catalog_hash,
        })
    }
}

/// Request payload for `Execution/list_capabilities`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesListRequest {
    /// Tenant whose invocable capabilities should be listed.
    pub tenant_id: TenantId,
}

/// Response payload for `Execution/list_capabilities`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesListResponse {
    /// Immutable compiler-ready catalog.
    pub catalog: ExecutionCapabilityCatalog,
    /// Structured reasons why declarations were not admitted to the catalog.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<CapabilityCatalogDiagnostic>,
}

/// Structured diagnostic for a declaration omitted from a capability catalog.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityCatalogDiagnostic {
    /// Stable machine-readable omission code.
    pub code: CapabilityCatalogDiagnosticCode,
    /// Stable reference to the omitted declaration.
    pub reference: String,
    /// Human-readable omission reason.
    pub message: String,
}

/// Stable reasons a declaration is not an invocable execution capability.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityCatalogDiagnosticCode {
    /// A knowledge connection is data configuration, not an invocation target.
    ConnectionOnlyDataSource,
    /// An action declaration has no registered backing tool.
    UnresolvedActionTool,
    /// A skill code declaration has no typed execution owner.
    UnownedSkillCode,
}

/// One governed capability available to the execution compiler.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCapability {
    /// Stable capability reference.
    pub reference: CapabilityReference,
    /// Exact governed runtime contract admitted for dispatch.
    pub contract_revision: String,
    /// Human-readable capability description.
    pub description: String,
    /// Draft 2020-12 input schema.
    pub input_schema: Value,
    /// Draft 2020-12 output schema.
    pub output_schema: Value,
    /// Action-policy class used at invocation time.
    pub action_class: ActionClass,
    /// Risk level used at invocation and review time.
    pub risk_level: RiskLevel,
    /// Default action-policy effect.
    pub default_effect: ActionPolicyEffect,
    /// Replay and retry safety classification.
    pub idempotency_class: IdempotencyClass,
    /// Resource-execution class.
    pub execution_class: ExecutionClass,
    /// Source provenance for this catalog entry.
    pub source: CapabilitySource,
    /// Canonical policy floor and artifact identity carried to durable dispatch.
    pub policy_context: CapabilityPolicyContext,
    /// Required worst-case estimate; catalog capabilities must declare one task.
    pub estimate: ExecutionEstimate,
    /// Exact compensator and mapping this capability promises will undo its effect.
    pub rollback: Option<CapabilityRollbackContract>,
}

/// Catalog-owned promise that one governed capability exactly undoes another.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRollbackContract {
    /// Exact governed capability version that performs the undo.
    pub compensator: CapabilityReference,
    /// Bounded mapping from committed forward input/output to compensator input.
    pub input_mapping: CompensationInputMapping,
}

impl CapabilityRollbackContract {
    /// Returns whether this catalog promise exactly matches a node's opt-in contract.
    #[must_use]
    pub fn matches(&self, compensation: &ExecutionCompensation) -> bool {
        self.compensator == compensation.compensator
            && self.input_mapping == compensation.input_mapping
    }
}

impl ExecutionCapability {
    pub(crate) fn validate_policy_context(&self) -> Result<()> {
        if self.policy_context.source != self.source {
            return Err(Error::InvalidProjection {
                message: format!(
                    "capability {} policy source does not match its dispatch source",
                    self.reference.name
                ),
            });
        }
        if let CapabilitySource::InstalledConnectorAction {
            governed_contract_revision,
            ..
        } = &self.source
            && self.contract_revision != *governed_contract_revision
        {
            return Err(Error::InvalidProjection {
                message: format!(
                    "capability {} installed connector governed revision does not match its contract revision",
                    self.reference.name
                ),
            });
        }
        self.policy_context
            .validate_for_source(&self.reference.name, &self.source)
    }
}

/// Canonical action-policy identity and minimum effect pinned with one capability.
///
/// The context is part of the immutable catalog so durable dispatch never has to
/// infer artifact governance from a display name, version string, or live tool
/// registration. `minimum_effect` is a floor: live tool and tenant policy may make
/// an invocation stricter, but may not make it weaker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityPolicyContext {
    /// Exact capability source whose policy this context governs.
    pub source: CapabilitySource,
    /// Canonical action reference used by artifact-aware policy, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_action_ref: Option<ArtifactRef>,
    /// Artifact row pinned by the capability, when the source is artifact-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_uid: Option<uuid::Uuid>,
    /// Artifact revision pinned by the capability, when the source is artifact-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_uid: Option<uuid::Uuid>,
    /// Least permissive effect that policy evaluation may return.
    pub minimum_effect: ActionPolicyEffect,
}

impl CapabilityPolicyContext {
    /// Builds policy context for a non-artifact registered capability.
    #[must_use]
    pub fn registered(source: CapabilitySource) -> Self {
        Self {
            source,
            canonical_action_ref: None,
            artifact_uid: None,
            revision_uid: None,
            minimum_effect: ActionPolicyEffect::Allow,
        }
    }

    /// Builds policy context for an artifact-backed capability.
    #[must_use]
    pub fn artifact(
        source: CapabilitySource,
        canonical_action_ref: Option<ArtifactRef>,
        artifact_uid: uuid::Uuid,
        revision_uid: uuid::Uuid,
        minimum_effect: ActionPolicyEffect,
    ) -> Self {
        Self {
            source,
            canonical_action_ref,
            artifact_uid: Some(artifact_uid),
            revision_uid: Some(revision_uid),
            minimum_effect,
        }
    }

    fn validate_for_source(&self, reference: &str, source: &CapabilitySource) -> Result<()> {
        let invalid = |message: String| Error::InvalidProjection {
            message: format!("capability {reference} has invalid policy context: {message}"),
        };
        match source {
            CapabilitySource::ActionArtifact {
                action_ref,
                revision_uid,
                ..
            } => {
                if self.canonical_action_ref.as_ref() != Some(action_ref) {
                    return Err(invalid(
                        "standalone action reference does not match its source".to_string(),
                    ));
                }
                if self.artifact_uid.is_none() || self.revision_uid != Some(*revision_uid) {
                    return Err(invalid(
                        "standalone action artifact and revision IDs must be pinned".to_string(),
                    ));
                }
            }
            CapabilitySource::ConnectorAction {
                connector_ref,
                revision_uid,
                action_id,
                ..
            } => {
                let expected = ArtifactRef::action(connector_ref.target_name(), action_id);
                if self.canonical_action_ref.as_ref() != Some(&expected) {
                    return Err(invalid(
                        "connector action reference does not match its source".to_string(),
                    ));
                }
                if self.artifact_uid.is_none() || self.revision_uid != Some(*revision_uid) {
                    return Err(invalid(
                        "connector artifact and revision IDs must be pinned".to_string(),
                    ));
                }
            }
            CapabilitySource::InstalledConnectorAction {
                connector_ref,
                connection_id,
                binding_id,
                connection_generation,
                definition_artifact_uid,
                definition_revision_uid,
                action_id,
                contract_hash,
                governed_contract_revision,
                minimum_effect,
                tool_name,
            } => {
                if connector_ref.artifact_kind() != Some(&ArtifactKind::Connector) {
                    return Err(invalid(
                        "installed connector reference must identify a connector artifact"
                            .to_string(),
                    ));
                }
                let expected = ArtifactRef::action(connector_ref.target_name(), action_id);
                if self.canonical_action_ref.as_ref() != Some(&expected) {
                    return Err(invalid(
                        "installed connector action reference does not match its source"
                            .to_string(),
                    ));
                }
                if self.artifact_uid != Some(*definition_artifact_uid)
                    || self.revision_uid != Some(*definition_revision_uid)
                {
                    return Err(invalid(
                        "installed connector definition artifact and revision IDs must match its source"
                            .to_string(),
                    ));
                }
                if self.minimum_effect != *minimum_effect {
                    return Err(invalid(
                        "installed connector minimum effect must match its source".to_string(),
                    ));
                }
                if connection_id.0.is_nil()
                    || binding_id.is_nil()
                    || *connection_generation == 0
                    || definition_artifact_uid.is_nil()
                    || definition_revision_uid.is_nil()
                    || !is_trimmed_nonempty(action_id)
                    || !is_canonical_digest(contract_hash)
                    || !is_trimmed_nonempty(governed_contract_revision)
                    || !is_trimmed_nonempty(tool_name)
                {
                    return Err(invalid(
                        "installed connector dispatch pins must be complete and non-empty"
                            .to_string(),
                    ));
                }
            }
            CapabilitySource::SkillAction { revision_uid, .. }
            | CapabilitySource::SkillCode { revision_uid, .. } => {
                if self.artifact_uid.is_none() || self.revision_uid != Some(*revision_uid) {
                    return Err(invalid(
                        "skill artifact and revision IDs must be pinned".to_string(),
                    ));
                }
            }
            CapabilitySource::BuiltInTool { .. }
            | CapabilitySource::HandTool { .. }
            | CapabilitySource::McpTool { .. }
            | CapabilitySource::Memory { .. }
            | CapabilitySource::Knowledge { .. }
            | CapabilitySource::Model => {
                if self.canonical_action_ref.is_some()
                    || self.artifact_uid.is_some()
                    || self.revision_uid.is_some()
                    || self.minimum_effect != ActionPolicyEffect::Allow
                {
                    return Err(invalid(
                        "registered capabilities cannot claim artifact policy identity".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }
}

fn is_trimmed_nonempty(value: &str) -> bool {
    !value.is_empty() && value.trim() == value
}

fn is_canonical_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Resource execution class for one capability.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionClass {
    /// Data retrieval or transformation.
    Data,
    /// Local or sandboxed compute.
    Compute,
    /// Model inference.
    Model,
    /// External side effect or service call.
    External,
}

/// Provenance for one capability-catalog entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilitySource {
    /// Built-in tool registered by MOA.
    BuiltInTool {
        /// Built-in tool name.
        name: String,
    },
    /// Tool routed to an owned hand or sandbox implementation.
    HandTool {
        /// Hand-routed tool name.
        name: String,
    },
    /// Tool exposed by one connected MCP server.
    McpTool {
        /// MCP server name.
        server: String,
        /// Registered tool name this capability dispatches through.
        ///
        /// Named `tool_name` like every other tool-backed variant, and for the
        /// same reason: it is the name the router resolves. For a connector tool
        /// that is the server-qualified reference, never the name the server
        /// publishes. The distinction is not cosmetic — dispatching the
        /// published name fails with `unknown tool`, and it fails only at
        /// runtime, which is exactly how this field's predecessor broke.
        tool_name: String,
        /// Tool name as the connector itself publishes it.
        ///
        /// Provenance only. It is deliberately NOT called `name`, so a consumer
        /// reaching for something to dispatch cannot pick it up by pattern
        /// matching alongside the built-in and hand variants, which is the
        /// mistake this shape exists to prevent.
        remote_name: String,
    },
    /// Serving standalone action artifact backed by a registered tool.
    ActionArtifact {
        /// Stable serving action reference.
        action_ref: ArtifactRef,
        /// Exact serving artifact revision.
        revision_uid: uuid::Uuid,
        /// Registered backing tool name.
        tool_name: String,
    },
    /// Validated connector action backed by a registered tool.
    ConnectorAction {
        /// Stable connector action reference.
        connector_ref: ArtifactRef,
        /// Exact validated connector revision.
        revision_uid: uuid::Uuid,
        /// Connector action identifier.
        action_id: String,
        /// Registered backing tool name.
        tool_name: String,
    },
    /// Tenant-installed connector action with durable connection and contract provenance.
    ///
    /// The model-visible `tool_name` is only a lookup key. Dispatch authority is
    /// carried by the remaining typed and generation-fenced fields and must never
    /// be reconstructed by parsing that name.
    InstalledConnectorAction {
        /// Canonical logical connector reference selected by the agent policy.
        connector_ref: ArtifactRef,
        /// Exact tenant-installed connection selected for this logical connector.
        connection_id: ConnectorConnectionId,
        /// Exact immutable installed-action binding row.
        binding_id: uuid::Uuid,
        /// Positive connection generation that produced the binding.
        connection_generation: u64,
        /// Stable connector definition artifact row.
        definition_artifact_uid: uuid::Uuid,
        /// Exact immutable connector definition revision.
        definition_revision_uid: uuid::Uuid,
        /// Canonical definition-local action identifier.
        action_id: String,
        /// Canonical hash of the installed binding's compiled operation contract.
        contract_hash: String,
        /// Policy-facing governed contract revision checked again at dispatch.
        governed_contract_revision: String,
        /// Definition-enforced action-policy floor.
        minimum_effect: ActionPolicyEffect,
        /// Model-visible overlay tool name used only for registry lookup.
        tool_name: String,
    },
    /// Action declared by an activated skill.
    SkillAction {
        /// Activated skill reference.
        skill_ref: ArtifactRef,
        /// Exact activated skill revision.
        revision_uid: uuid::Uuid,
        /// Skill action identifier.
        action_id: String,
        /// Registered backing tool name.
        tool_name: String,
    },
    /// Code entrypoint declared by an activated skill.
    SkillCode {
        /// Activated skill reference.
        skill_ref: ArtifactRef,
        /// Exact activated skill revision.
        revision_uid: uuid::Uuid,
        /// Skill code entrypoint.
        entrypoint: String,
    },
    /// Graph-memory capability.
    Memory {
        /// Stable graph-memory operation name.
        operation: String,
        /// Registered backing tool name.
        tool_name: String,
    },
    /// Tenant knowledge-base capability.
    Knowledge {
        /// Stable typed knowledge operation.
        operation: String,
    },
    /// Model capability.
    Model,
}

impl CapabilitySource {
    /// Returns the model-visible registered tool name used for governed dispatch.
    ///
    /// Task-local agents expose and receive tool calls by this name. Multiple
    /// canonical capability references may therefore share it, which callers
    /// must treat as ambiguous rather than resolving by declaration order.
    #[must_use]
    pub fn model_visible_tool_name(&self) -> Option<&str> {
        match self {
            Self::BuiltInTool { name } | Self::HandTool { name } => Some(name),
            Self::McpTool { tool_name, .. }
            | Self::ActionArtifact { tool_name, .. }
            | Self::ConnectorAction { tool_name, .. }
            | Self::InstalledConnectorAction { tool_name, .. }
            | Self::SkillAction { tool_name, .. }
            | Self::Memory { tool_name, .. } => Some(tool_name),
            Self::SkillCode { .. } | Self::Knowledge { .. } | Self::Model => None,
        }
    }
}

/// Integer worst-case or actual resource estimate.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEstimate {
    /// Cost in integer micro-US-dollars.
    pub cost_microusd: u64,
    /// Model tokens.
    pub tokens: u64,
    /// Governed tool calls.
    pub tool_calls: u64,
    /// Retrieved bytes.
    pub retrieved_bytes: u64,
    /// Logical persisted tasks.
    pub tasks: u64,
}

impl ExecutionEstimate {
    /// Adds two estimates with checked arithmetic.
    pub fn checked_add(self, other: Self, context: &str) -> Result<Self> {
        Ok(Self {
            cost_microusd: checked_add(self.cost_microusd, other.cost_microusd, context)?,
            tokens: checked_add(self.tokens, other.tokens, context)?,
            tool_calls: checked_add(self.tool_calls, other.tool_calls, context)?,
            retrieved_bytes: checked_add(self.retrieved_bytes, other.retrieved_bytes, context)?,
            tasks: checked_add(self.tasks, other.tasks, context)?,
        })
    }

    /// Multiplies only resource dimensions, preserving logical task count.
    pub fn checked_multiply_resources(self, factor: u64, context: &str) -> Result<Self> {
        Ok(Self {
            cost_microusd: checked_mul(self.cost_microusd, factor, context)?,
            tokens: checked_mul(self.tokens, factor, context)?,
            tool_calls: checked_mul(self.tool_calls, factor, context)?,
            retrieved_bytes: checked_mul(self.retrieved_bytes, factor, context)?,
            tasks: self.tasks,
        })
    }

    /// Multiplies all estimate dimensions with checked arithmetic.
    pub fn checked_multiply_all(self, factor: u64, context: &str) -> Result<Self> {
        Ok(Self {
            cost_microusd: checked_mul(self.cost_microusd, factor, context)?,
            tokens: checked_mul(self.tokens, factor, context)?,
            tool_calls: checked_mul(self.tool_calls, factor, context)?,
            retrieved_bytes: checked_mul(self.retrieved_bytes, factor, context)?,
            tasks: checked_mul(self.tasks, factor, context)?,
        })
    }
}

/// Computes the canonical catalog hash, excluding the `catalog_hash` field.
pub fn catalog_hash(capabilities: &[ExecutionCapability]) -> Result<ExecutionHash> {
    hash_serializable(CATALOG_HASH_DOMAIN, capabilities)
}

/// Computes the canonical hash of one execution-plan DAG.
///
/// Node declaration order is not execution semantics, so hashes sort nodes by stable ID before
/// canonical JSON encoding. This lets loop detection recognize an equivalent amended DAG even
/// when remove/add operations changed array position.
pub fn plan_hash(plan: &ExecutionPlanDefinition) -> Result<ExecutionHash> {
    let mut canonical = plan.clone();
    canonical
        .nodes
        .sort_by(|left, right| left.id.cmp(&right.id));
    hash_serializable(PLAN_HASH_DOMAIN, &canonical)
}

/// Computes the canonical hash of exactly one plan amendment.
pub fn amendment_hash(amendment: &PlanAmendment) -> Result<ExecutionHash> {
    hash_serializable(AMENDMENT_HASH_DOMAIN, amendment)
}

/// Computes the canonical loop-detection fingerprint of an amendment's operation semantics.
///
/// Unlike [`amendment_hash`], this deliberately excludes the base revision and free-form
/// reason/evidence so changing planner prose cannot evade repeated-operation detection.
pub fn amendment_operations_fingerprint(amendment: &PlanAmendment) -> Result<ExecutionHash> {
    #[derive(Serialize)]
    struct AmendmentOperationsHashInput<'a> {
        operations: &'a [PlanAmendmentOperation],
    }

    hash_serializable(
        AMENDMENT_OPERATIONS_HASH_DOMAIN,
        &AmendmentOperationsHashInput {
            operations: &amendment.operations,
        },
    )
}

/// Computes a domain-separated hash of one structured task output.
pub fn task_output_hash(output: &Value) -> Result<ExecutionHash> {
    hash_serializable(TASK_OUTPUT_HASH_DOMAIN, output)
}

/// Computes a deterministic version string from owned capability metadata.
pub fn capability_version(domain: &str, metadata: &Value) -> Result<String> {
    Ok(hash_serializable(domain, metadata)?.to_string())
}

pub(crate) fn hash_serializable<T: Serialize + ?Sized>(
    domain: &str,
    value: &T,
) -> Result<ExecutionHash> {
    let bytes = canonical_json_bytes(value)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(&[0]);
    hasher.update(&bytes);
    Ok(ExecutionHash::from_bytes(*hasher.finalize().as_bytes()))
}

pub(crate) fn canonical_sort_key<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    canonical_json_bytes(value).map_err(Into::into)
}

fn checked_add(left: u64, right: u64, context: &str) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: context.to_string(),
        })
}

fn checked_mul(left: u64, right: u64, context: &str) -> Result<u64> {
    left.checked_mul(right)
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: context.to_string(),
        })
}

fn encode_hex(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use moa_artifacts::execution_plan::CapabilityReference;
    use moa_artifacts::reference::ArtifactRef;
    use moa_core::types::{
        action_policy::{ActionClass, ActionPolicyEffect, RiskLevel},
        tools::IdempotencyClass,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        CapabilityPolicyContext, CapabilitySource, ExecutionCapability, ExecutionCapabilityCatalog,
        ExecutionClass, ExecutionEstimate, catalog_hash,
    };

    fn capability(name: &str, source: CapabilitySource) -> ExecutionCapability {
        let policy_context = CapabilityPolicyContext::registered(source.clone());
        ExecutionCapability {
            reference: CapabilityReference {
                name: name.to_string(),
                version: "implementation".to_string(),
            },
            contract_revision: "contract-v1".to_string(),
            description: format!("{name} description"),
            input_schema: json!({"type": "object"}),
            output_schema: json!({}),
            action_class: ActionClass::Read,
            risk_level: RiskLevel::Low,
            default_effect: ActionPolicyEffect::Allow,
            idempotency_class: IdempotencyClass::Idempotent,
            execution_class: ExecutionClass::Data,
            source,
            policy_context,
            estimate: ExecutionEstimate {
                tool_calls: 1,
                tasks: 1,
                ..ExecutionEstimate::default()
            },
            rollback: None,
        }
    }

    #[test]
    fn capability_catalog_build_sorts_and_hashes_canonical_entries() {
        // Pins: the API and compiler receive one deterministic catalog regardless of registry order.
        let catalog = ExecutionCapabilityCatalog::build(vec![
            capability(
                "zeta",
                CapabilitySource::HandTool {
                    name: "zeta".to_string(),
                },
            ),
            capability(
                "alpha",
                CapabilitySource::BuiltInTool {
                    name: "alpha".to_string(),
                },
            ),
        ])
        .expect("catalog should build");

        assert_eq!(catalog.capabilities[0].reference.name, "alpha");
        assert_eq!(catalog.capabilities[1].reference.name, "zeta");
        let encoded = serde_json::to_value(&catalog).expect("catalog should serialize");
        assert!(encoded.get("schema_version").is_none());
        assert_eq!(
            catalog.catalog_hash,
            catalog_hash(&catalog.capabilities).expect("catalog hash should recompute")
        );
        assert!(
            catalog
                .capabilities
                .iter()
                .all(|entry| entry.estimate.tasks == 1 && entry.estimate.tool_calls == 1)
        );
    }

    #[test]
    fn capability_catalog_build_rejects_a_reference_claimed_twice() {
        // Pins: a reference identifies exactly one capability. Two entries under
        // one reference would make "which capability did this run invoke"
        // answerable only by construction order, and a plan pinning that
        // reference would resolve to whichever entry happened to sort first.
        let error = ExecutionCapabilityCatalog::build(vec![
            capability(
                "search",
                CapabilitySource::McpTool {
                    server: "first".to_string(),
                    tool_name: "mcp__first__search".to_string(),
                    remote_name: "search".to_string(),
                },
            ),
            capability(
                "search",
                CapabilitySource::McpTool {
                    server: "second".to_string(),
                    tool_name: "mcp__second__search".to_string(),
                    remote_name: "search".to_string(),
                },
            ),
        ])
        .expect_err("a duplicated capability reference must be rejected");

        assert!(
            matches!(
                error,
                crate::Error::DuplicateCapabilityReference { ref reference, .. }
                    if reference == "search"
            ),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn capability_policy_context_survives_catalog_serialization_with_exact_identity() {
        // Pins: the immutable catalog carries canonical action identity, exact
        // artifact/revision IDs, source provenance, and the review floor through replay.
        let action_ref = ArtifactRef::action_artifact("publish-note");
        let artifact_uid = Uuid::from_u128(91);
        let revision_uid = Uuid::from_u128(92);
        let source = CapabilitySource::ActionArtifact {
            action_ref: action_ref.clone(),
            revision_uid,
            tool_name: "file_read".to_string(),
        };
        let mut action = capability(action_ref.to_string().as_str(), source.clone());
        action.default_effect = ActionPolicyEffect::AdminReview;
        action.policy_context = CapabilityPolicyContext::artifact(
            source,
            Some(action_ref.clone()),
            artifact_uid,
            revision_uid,
            ActionPolicyEffect::AdminReview,
        );
        let catalog = ExecutionCapabilityCatalog::build(vec![action])
            .expect("artifact capability context should validate");

        let encoded = serde_json::to_vec(&catalog).expect("serialize capability catalog");
        let decoded: ExecutionCapabilityCatalog =
            serde_json::from_slice(&encoded).expect("deserialize capability catalog");
        assert_eq!(decoded, catalog);
        assert_eq!(
            decoded.capabilities[0].policy_context,
            CapabilityPolicyContext {
                source: CapabilitySource::ActionArtifact {
                    action_ref: action_ref.clone(),
                    revision_uid,
                    tool_name: "file_read".to_string(),
                },
                canonical_action_ref: Some(action_ref),
                artifact_uid: Some(artifact_uid),
                revision_uid: Some(revision_uid),
                minimum_effect: ActionPolicyEffect::AdminReview,
            }
        );
    }

    #[test]
    fn capability_policy_context_rejects_source_identity_drift() {
        // Pins: catalog construction fails closed when policy provenance does
        // not describe the same runtime source as the capability dispatch path.
        let mut capability = capability(
            "file_read",
            CapabilitySource::BuiltInTool {
                name: "file_read".to_string(),
            },
        );
        capability.policy_context.source = CapabilitySource::HandTool {
            name: "file_read".to_string(),
        };

        let error = ExecutionCapabilityCatalog::build(vec![capability])
            .expect_err("mismatched policy provenance must fail catalog construction");
        assert!(
            matches!(error, crate::Error::InvalidProjection { ref message }
                if message.contains("policy source does not match")),
            "unexpected error: {error:?}"
        );
    }
}
