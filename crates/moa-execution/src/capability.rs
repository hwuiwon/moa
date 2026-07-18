//! Immutable capability catalogs, authorization envelopes, estimates, and hashes.

use std::{fmt, str::FromStr};

use moa_artifacts::{
    execution_plan::{
        CapabilityReference, ExecutionPlanDefinition, PlanAmendment, PlanAmendmentOperation,
    },
    reference::ArtifactRef,
};
use moa_core::types::{
    action_policy::{ActionClass, ActionPolicyEffect, RiskLevel},
    identifiers::TenantId,
    tools::IdempotencyClass,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use serde_canonical_json::CanonicalFormatter;
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
    /// Capability-catalog schema version, which must be `1`.
    pub schema_version: u32,
    /// Sorted, duplicate-free governed capabilities.
    pub capabilities: Vec<ExecutionCapability>,
    /// Canonical hash of `{ schema_version, capabilities }`.
    pub catalog_hash: ExecutionHash,
}

impl ExecutionCapabilityCatalog {
    /// Builds a deterministic catalog from invocable capabilities.
    pub fn build(capabilities: Vec<ExecutionCapability>) -> Result<Self> {
        let mut keyed = capabilities
            .into_iter()
            .map(|capability| Ok((canonical_sort_key(&capability.reference)?, capability)))
            .collect::<Result<Vec<_>>>()?;
        keyed.sort_by(|left, right| left.0.cmp(&right.0));
        let capabilities = keyed
            .into_iter()
            .map(|(_, capability)| capability)
            .collect::<Vec<_>>();
        let catalog_hash = catalog_hash(1, &capabilities)?;
        Ok(Self {
            schema_version: 1,
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
    /// Required worst-case estimate; catalog capabilities must declare one task.
    pub estimate: ExecutionEstimate,
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
        /// MCP tool name.
        name: String,
    },
    /// Published standalone action artifact backed by a registered tool.
    ActionArtifact {
        /// Stable published action reference.
        action_ref: ArtifactRef,
        /// Exact published artifact revision.
        revision_uid: uuid::Uuid,
        /// Registered backing tool name.
        tool_name: String,
    },
    /// Published connector action backed by a registered tool.
    ConnectorAction {
        /// Stable connector action reference.
        connector_ref: ArtifactRef,
        /// Exact published connector revision.
        revision_uid: uuid::Uuid,
        /// Connector action identifier.
        action_id: String,
        /// Registered backing tool name.
        tool_name: String,
    },
    /// Action declared by a published skill.
    SkillAction {
        /// Published skill reference.
        skill_ref: ArtifactRef,
        /// Exact published skill revision.
        revision_uid: uuid::Uuid,
        /// Skill action identifier.
        action_id: String,
        /// Registered backing tool name.
        tool_name: String,
    },
    /// Code entrypoint declared by a published skill.
    SkillCode {
        /// Published skill reference.
        skill_ref: ArtifactRef,
        /// Exact published skill revision.
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
pub fn catalog_hash(
    schema_version: u32,
    capabilities: &[ExecutionCapability],
) -> Result<ExecutionHash> {
    #[derive(Serialize)]
    struct CatalogHashInput<'a> {
        schema_version: u32,
        capabilities: &'a [ExecutionCapability],
    }

    hash_serializable(
        CATALOG_HASH_DOMAIN,
        &CatalogHashInput {
            schema_version,
            capabilities,
        },
    )
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
        schema_version: u32,
        operations: &'a [PlanAmendmentOperation],
    }

    hash_serializable(
        AMENDMENT_OPERATIONS_HASH_DOMAIN,
        &AmendmentOperationsHashInput {
            schema_version: amendment.schema_version,
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

pub(crate) fn canonical_json_bytes<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    let mut serializer =
        serde_json::Serializer::with_formatter(Vec::new(), CanonicalFormatter::new());
    value.serialize(&mut serializer)?;
    Ok(serializer.into_inner())
}

pub(crate) fn canonical_sort_key<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    canonical_json_bytes(value)
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
    use moa_core::types::{
        action_policy::{ActionClass, ActionPolicyEffect, RiskLevel},
        tools::IdempotencyClass,
    };
    use serde_json::json;

    use super::{
        CapabilitySource, ExecutionCapability, ExecutionCapabilityCatalog, ExecutionClass,
        ExecutionEstimate, catalog_hash,
    };

    fn capability(name: &str, source: CapabilitySource) -> ExecutionCapability {
        ExecutionCapability {
            reference: CapabilityReference {
                name: name.to_string(),
                version: "implementation".to_string(),
            },
            description: format!("{name} description"),
            input_schema: json!({"type": "object"}),
            output_schema: json!({}),
            action_class: ActionClass::Read,
            risk_level: RiskLevel::Low,
            default_effect: ActionPolicyEffect::Allow,
            idempotency_class: IdempotencyClass::Idempotent,
            execution_class: ExecutionClass::Data,
            source,
            estimate: ExecutionEstimate {
                tool_calls: 1,
                tasks: 1,
                ..ExecutionEstimate::default()
            },
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

        assert_eq!(catalog.schema_version, 1);
        assert_eq!(catalog.capabilities[0].reference.name, "alpha");
        assert_eq!(catalog.capabilities[1].reference.name, "zeta");
        assert_eq!(
            catalog.catalog_hash,
            catalog_hash(catalog.schema_version, &catalog.capabilities)
                .expect("catalog hash should recompute")
        );
        assert!(
            catalog
                .capabilities
                .iter()
                .all(|entry| entry.estimate.tasks == 1 && entry.estimate.tool_calls == 1)
        );
    }
}
