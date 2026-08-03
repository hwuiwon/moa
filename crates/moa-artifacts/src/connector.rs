//! Versioned connector artifact definitions and secret-free operation contracts.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use moa_core::types::action_policy::{ActionClass, ActionPolicyEffect, RiskLevel};
use moa_core::types::credentials::CredentialSlotName;
use moa_core::types::identifiers::ConnectorConnectionId;
use moa_core::types::security::SensitivityClass;
use moa_core::types::tools::IdempotencyClass;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::document::empty_object;

/// Version-dispatched connector declaration.
///
/// Bodies without `definition_version` are accepted only when they have the
/// exact legacy `auth`/`actions`/`ui` shape. Runtime definitions must state the
/// literal `definition_version: "v1"`; an unknown discriminator never falls
/// back to the legacy decoder.
#[derive(Clone, Debug, PartialEq)]
pub enum ConnectorDefinition {
    /// Pre-runtime connector metadata and aliases.
    Legacy(LegacyConnectorDefinition),
    /// Connection-installable runtime connector definition.
    RuntimeV1(RuntimeConnectorDefinitionV1),
}

impl ConnectorDefinition {
    /// Returns the legacy connector body, when this revision uses the legacy shape.
    #[must_use]
    pub const fn legacy(&self) -> Option<&LegacyConnectorDefinition> {
        match self {
            Self::Legacy(definition) => Some(definition),
            Self::RuntimeV1(_) => None,
        }
    }

    /// Returns the runtime V1 connector body, when this revision is installable.
    #[must_use]
    pub const fn runtime_v1(&self) -> Option<&RuntimeConnectorDefinitionV1> {
        match self {
            Self::Legacy(_) => None,
            Self::RuntimeV1(definition) => Some(definition),
        }
    }

    /// Returns a typed iterator over the logical actions in either definition version.
    #[must_use]
    pub fn actions(&self) -> ConnectorActions<'_> {
        match self {
            Self::Legacy(definition) => ConnectorActions::legacy(&definition.actions),
            Self::RuntimeV1(definition) => ConnectorActions::runtime_v1(&definition.actions),
        }
    }

    /// Returns whether this definition can back a tenant connection.
    #[must_use]
    pub const fn is_connection_installable(&self) -> bool {
        matches!(self, Self::RuntimeV1(_))
    }
}

impl Serialize for ConnectorDefinition {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Legacy(definition) => definition.serialize(serializer),
            Self::RuntimeV1(definition) => definition.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ConnectorDefinition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ConnectorDefinitionVisitor)
    }
}

struct ConnectorDefinitionVisitor;

impl<'de> Visitor<'de> for ConnectorDefinitionVisitor {
    type Value = ConnectorDefinition;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a connector definition object with unique fields")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut fields = serde_json::Map::new();
        let mut seen = HashSet::new();
        while let Some(field) = map.next_key::<String>()? {
            if !seen.insert(field.clone()) {
                return Err(de::Error::custom(format!(
                    "duplicate connector definition field `{field}`"
                )));
            }
            let value = map.next_value::<DuplicateRejectingValue>()?;
            fields.insert(field, value.0);
        }
        decode_connector_definition(fields).map_err(de::Error::custom)
    }
}

struct DuplicateRejectingValue(Value);

impl<'de> Deserialize<'de> for DuplicateRejectingValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateRejectingValueVisitor)
    }
}

struct DuplicateRejectingValueVisitor;

impl<'de> Visitor<'de> for DuplicateRejectingValueVisitor {
    type Value = DuplicateRejectingValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON-compatible connector field value with unique object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(DuplicateRejectingValue)
            .ok_or_else(|| de::Error::custom("connector numbers must be finite"))
    }

    fn visit_char<E>(self, value: char) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingValue(Value::String(value.to_string())))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingValue(Value::String(value.to_string())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingValue(Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        DuplicateRejectingValue::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<DuplicateRejectingValue>()? {
            values.push(value.0);
        }
        Ok(DuplicateRejectingValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut fields = serde_json::Map::new();
        let mut seen = HashSet::new();
        while let Some(field) = map.next_key::<String>()? {
            if !seen.insert(field.clone()) {
                return Err(de::Error::custom(format!(
                    "duplicate connector definition field `{field}`"
                )));
            }
            let value = map.next_value::<DuplicateRejectingValue>()?;
            fields.insert(field, value.0);
        }
        Ok(DuplicateRejectingValue(Value::Object(fields)))
    }
}

fn decode_connector_definition(
    fields: serde_json::Map<String, Value>,
) -> Result<ConnectorDefinition, String> {
    let discriminator = fields.get("definition_version").cloned();
    let value = Value::Object(fields);
    match discriminator.as_ref() {
        Some(Value::String(version)) if version == "v1" => serde_json::from_value(value)
            .map(ConnectorDefinition::RuntimeV1)
            .map_err(|error| error.to_string()),
        Some(Value::String(version)) => Err(format!(
            "unsupported connector definition_version `{version}`"
        )),
        Some(_) => Err("connector definition_version must be the string `v1`".to_string()),
        None => serde_json::from_value(value)
            .map(ConnectorDefinition::Legacy)
            .map_err(|error| error.to_string()),
    }
}

/// Exact pre-runtime connector body retained for import/export compatibility.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyConnectorDefinition {
    /// Authentication or setup metadata.
    #[serde(default = "empty_object")]
    pub auth: Value,
    /// Callable aliases exposed by the connector.
    #[serde(default)]
    pub actions: Vec<ConnectorActionDefinition>,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

/// Callable action exposed by a legacy connector.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorActionDefinition {
    /// Stable action identifier within the connector.
    pub id: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: String,
    /// Optional internal tool name used to dispatch the action when present.
    ///
    /// An authored name must contain at least one non-whitespace character.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// JSON object schema for action inputs.
    #[serde(default = "empty_object")]
    pub input_schema: Value,
    /// JSON object schema for action outputs.
    #[serde(default = "empty_object")]
    pub output_schema: Value,
    /// Whether this action should be routed to tenant-admin review.
    #[serde(default)]
    pub admin_review_required: bool,
    /// Builder-owned UI metadata.
    #[serde(default = "empty_object")]
    pub ui: Value,
}

/// Connection-installable connector definition V1.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConnectorDefinitionV1 {
    /// Required wire discriminator whose type admits only literal `v1`.
    pub definition_version: ConnectorDefinitionVersionV1,
    /// Human-readable label that does not participate in artifact identity.
    pub display_name: String,
    /// Optional human-readable connector description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Closed runtime transport class.
    pub runtime: RuntimeConnectorKindV1,
    /// Named, secret-free credential requirements.
    pub auth: Vec<RuntimeConnectorAuthRequirementV1>,
    /// Logical operations exposed by this connector.
    pub actions: Vec<RuntimeConnectorActionV1>,
}

/// Single valid discriminator for a runtime connector V1 body.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ConnectorDefinitionVersionV1 {
    /// Runtime connector definition version one.
    #[serde(rename = "v1")]
    V1,
}

/// Closed runtime class for a V1 connector definition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeConnectorKindV1 {
    /// Operations execute against one admitted HTTP origin stored on the connection.
    ConstrainedHttp,
    /// Operations dispatch to a platform-owned managed-provider implementation.
    BuiltInManaged {
        /// Stable code-owned provider key.
        provider: String,
    },
}

/// Secret-free authentication requirement declared by a runtime connector.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeConnectorAuthRequirementV1 {
    /// The connection requires no credential material.
    None,
    /// An HTTP bearer token is resolved from the named slot.
    Bearer {
        /// Credential slot holding the bearer token.
        slot: CredentialSlotName,
    },
    /// An API key is attached through one fixed, safe header.
    ApiKeyHeader {
        /// Credential slot holding the API key.
        slot: CredentialSlotName,
        /// Fixed header name selected by the definition, never by model input.
        header: ApiKeyHeaderName,
    },
    /// OAuth material is brokered for the named managed slot.
    ManagedOauth {
        /// Credential slot holding the managed OAuth series.
        slot: CredentialSlotName,
    },
}

impl RuntimeConnectorAuthRequirementV1 {
    /// Returns the credential slot selected by this requirement, when any.
    #[must_use]
    pub const fn slot(&self) -> Option<&CredentialSlotName> {
        match self {
            Self::None => None,
            Self::Bearer { slot }
            | Self::ApiKeyHeader { slot, .. }
            | Self::ManagedOauth { slot } => Some(slot),
        }
    }
}

/// Logical action declared by a runtime connector.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConnectorActionV1 {
    /// Stable action identifier within the connector.
    pub id: String,
    /// Human-readable operation description.
    #[serde(default)]
    pub description: String,
    /// Fixed transport operation and governed contract.
    pub binding: RuntimeOperationBindingV1,
}

impl RuntimeConnectorActionV1 {
    /// Returns the governed model-facing and policy contract for this action.
    #[must_use]
    pub const fn policy(&self) -> &RuntimeOperationPolicyV1 {
        self.binding.policy()
    }
}

/// Tagged transport binding for one runtime connector action.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeOperationBindingV1 {
    /// A fixed constrained HTTP operation.
    Http {
        /// Complete HTTP operation contract.
        contract: HttpOperationContract,
    },
    /// A fixed operation implemented by a platform-owned managed provider.
    BuiltInManaged {
        /// Exact code-owned operation key.
        operation: String,
        /// Governed schema, data, risk, review, and retry contract.
        contract: RuntimeOperationPolicyV1,
    },
}

impl RuntimeOperationBindingV1 {
    /// Returns the governed contract shared by all operation transports.
    #[must_use]
    pub const fn policy(&self) -> &RuntimeOperationPolicyV1 {
        match self {
            Self::Http { contract } => &contract.policy,
            Self::BuiltInManaged { contract, .. } => contract,
        }
    }

    /// Returns the credential slot selected for this operation, when present.
    #[must_use]
    pub const fn credential_slot(&self) -> Option<&CredentialSlotName> {
        match self {
            Self::Http { contract } => contract.credential_slot.as_ref(),
            Self::BuiltInManaged { .. } => None,
        }
    }
}

/// Governed schemas, data classes, policy floor, and retry semantics for an operation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeOperationPolicyV1 {
    /// JSON object schema offered to the model for operation inputs.
    pub input_schema: Value,
    /// JSON object schema enforced for operation outputs.
    pub output_schema: Value,
    /// Data sensitivity classes this operation may transmit.
    pub data_classes: Vec<SensitivityClass>,
    /// Policy/audit class for every invocation.
    pub action_class: ActionClass,
    /// Intrinsic risk floor for every invocation.
    pub risk_level: RiskLevel,
    /// Least permissive effect the runtime may return.
    pub minimum_effect: ActionPolicyEffect,
    /// Declared retry semantics for the underlying operation.
    pub idempotency: IdempotencyClass,
}

/// Fixed constrained HTTP operation compiled from a runtime connector definition.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpOperationContract {
    /// Fixed HTTP method.
    pub method: HttpMethodV1,
    /// Fixed origin-relative path template.
    pub path_template: String,
    /// Complete-segment path placeholder mappings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_inputs: Vec<HttpPathInputV1>,
    /// Fixed query parameter mappings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_inputs: Vec<HttpQueryInputV1>,
    /// Optional JSON request-body mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_input: Option<HttpBodyInputV1>,
    /// Optional declared credential slot attached by trusted host code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_slot: Option<CredentialSlotName>,
    /// Optional reviewed header that receives the durable tool-call ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_idempotency_header: Option<UpstreamIdempotencyHeaderName>,
    /// Optional RFC 6901 pointer extracting the model-visible JSON response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_pointer: Option<String>,
    /// Maximum serialized request-body bytes.
    pub max_request_bytes: u32,
    /// Maximum response bytes read from the server.
    pub max_response_bytes: u32,
    /// TCP/TLS connection timeout in milliseconds.
    pub connect_timeout_ms: u32,
    /// Whole-operation timeout in milliseconds.
    pub total_timeout_ms: u32,
    /// Governed model-facing and policy contract.
    pub policy: RuntimeOperationPolicyV1,
}

/// Closed set of HTTP methods accepted by custom connector V1.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethodV1 {
    /// HTTP GET.
    Get,
    /// HTTP POST.
    Post,
    /// HTTP PUT.
    Put,
    /// HTTP PATCH.
    Patch,
    /// HTTP DELETE.
    Delete,
}

/// Maps one complete path-template placeholder to model input.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpPathInputV1 {
    /// Placeholder name without braces.
    pub placeholder: String,
    /// RFC 6901 pointer into the validated operation input.
    pub input_pointer: String,
}

/// Maps one fixed query parameter to model input.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpQueryInputV1 {
    /// Fixed query parameter name.
    pub parameter: String,
    /// RFC 6901 pointer into the validated operation input.
    pub input_pointer: String,
}

/// Maps a JSON request body from model input.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpBodyInputV1 {
    /// RFC 6901 pointer into the validated operation input.
    pub input_pointer: String,
}

/// Validated API-key header name stored in a secret-free auth contract.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ApiKeyHeaderName(String);

impl ApiKeyHeaderName {
    /// Returns the canonical lowercase header name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ApiKeyHeaderName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ApiKeyHeaderName {
    type Err = ApiKeyHeaderNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_safe_header_name(value)
            .map(Self)
            .ok_or_else(|| ApiKeyHeaderNameError(value.to_string()))
    }
}

impl Serialize for ApiKeyHeaderName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ApiKeyHeaderName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// Failure returned when an API-key header is invalid or security-sensitive.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("API-key header `{0}` is invalid or reserved")]
pub struct ApiKeyHeaderNameError(String);

/// Validated upstream idempotency header stored in one reviewed HTTP contract.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UpstreamIdempotencyHeaderName(String);

impl UpstreamIdempotencyHeaderName {
    /// Returns the canonical lowercase header name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UpstreamIdempotencyHeaderName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for UpstreamIdempotencyHeaderName {
    type Err = UpstreamIdempotencyHeaderNameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_safe_header_name(value)
            .map(Self)
            .ok_or_else(|| UpstreamIdempotencyHeaderNameError(value.to_string()))
    }
}

impl Serialize for UpstreamIdempotencyHeaderName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for UpstreamIdempotencyHeaderName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

/// Failure returned when an upstream idempotency header is invalid or reserved.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("upstream idempotency header `{0}` is invalid or reserved")]
pub struct UpstreamIdempotencyHeaderNameError(String);

fn parse_safe_header_name(value: &str) -> Option<String> {
    let canonical = value.to_ascii_lowercase();
    (!value.is_empty()
        && value.len() <= 128
        && value.is_ascii()
        && value.bytes().all(is_header_token_byte)
        && !is_reserved_header(&canonical))
    .then_some(canonical)
}

fn is_header_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn is_reserved_header(name: &str) -> bool {
    matches!(
        name,
        "authorization"
            | "connection"
            | "content-length"
            | "content-type"
            | "cookie"
            | "forwarded"
            | "host"
            | "keep-alive"
            | "origin"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "referer"
            | "set-cookie"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "via"
            | "x-forwarded-for"
            | "x-forwarded-host"
            | "x-forwarded-port"
            | "x-forwarded-proto"
            | "x-http-method"
            | "x-http-method-override"
            | "x-method-override"
            | "x-original-url"
            | "x-real-ip"
            | "x-rewrite-url"
    ) || name.starts_with("proxy-")
        || name.starts_with("sec-")
        || name.starts_with("x-forwarded-")
}

/// One borrowed action from either the legacy or runtime definition shape.
#[derive(Clone, Copy, Debug)]
pub enum ConnectorActionRef<'a> {
    /// Legacy tool alias.
    Legacy(&'a ConnectorActionDefinition),
    /// Runtime connection-bound action.
    RuntimeV1(&'a RuntimeConnectorActionV1),
}

impl<'a> ConnectorActionRef<'a> {
    /// Returns the stable logical action identifier.
    #[must_use]
    pub fn id(self) -> &'a str {
        match self {
            Self::Legacy(action) => &action.id,
            Self::RuntimeV1(action) => &action.id,
        }
    }
}

/// Zero-allocation iterator over connector actions independent of definition version.
pub struct ConnectorActions<'a> {
    inner: ConnectorActionsInner<'a>,
}

enum ConnectorActionsInner<'a> {
    Legacy(std::slice::Iter<'a, ConnectorActionDefinition>),
    RuntimeV1(std::slice::Iter<'a, RuntimeConnectorActionV1>),
}

impl<'a> ConnectorActions<'a> {
    fn legacy(actions: &'a [ConnectorActionDefinition]) -> Self {
        Self {
            inner: ConnectorActionsInner::Legacy(actions.iter()),
        }
    }

    fn runtime_v1(actions: &'a [RuntimeConnectorActionV1]) -> Self {
        Self {
            inner: ConnectorActionsInner::RuntimeV1(actions.iter()),
        }
    }
}

impl<'a> Iterator for ConnectorActions<'a> {
    type Item = ConnectorActionRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            ConnectorActionsInner::Legacy(actions) => {
                actions.next().map(ConnectorActionRef::Legacy)
            }
            ConnectorActionsInner::RuntimeV1(actions) => {
                actions.next().map(ConnectorActionRef::RuntimeV1)
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.inner {
            ConnectorActionsInner::Legacy(actions) => actions.size_hint(),
            ConnectorActionsInner::RuntimeV1(actions) => actions.size_hint(),
        }
    }
}

impl ExactSizeIterator for ConnectorActions<'_> {}

/// Derives the only model-visible tool name for one installed connector action.
///
/// The `uuid-simple` connection component is always 32 ASCII bytes, leaving
/// exactly 24 bytes for a valid action identifier under the 64-byte tool limit.
pub fn connection_action_tool_reference(
    connection_id: ConnectorConnectionId,
    action_id: &str,
) -> Result<String, ConnectorActionReferenceError> {
    if !is_runtime_action_id(action_id) {
        return Err(ConnectorActionReferenceError(action_id.to_string()));
    }
    Ok(format!("conn__{}__{action_id}", connection_id.0.simple()))
}

/// Failure returned when a connection-qualified tool name receives an invalid action ID.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("connector action id `{0}` must match [A-Za-z][A-Za-z0-9_-]{{0,23}}")]
pub struct ConnectorActionReferenceError(String);

pub(crate) fn is_runtime_action_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=24).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}
