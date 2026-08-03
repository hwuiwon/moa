//! Connector artifact definitions and secret-free operation contracts.

use std::fmt;
use std::str::FromStr;

use moa_core::types::credentials::CredentialSlotName;
use moa_core::types::security::SensitivityClass;
use moa_core::types::tools::IdempotencyClass;
use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Immutable reviewed constrained-HTTP connector definition.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorDefinition {
    /// Human-readable label that does not participate in artifact identity.
    pub display_name: String,
    /// Optional human-readable connector description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Named, secret-free credential requirements.
    pub auth: Vec<RuntimeConnectorAuthRequirement>,
    /// Logical operations exposed by this connector.
    pub actions: Vec<RuntimeConnectorAction>,
}

/// Secret-free authentication requirement declared by a runtime connector.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeConnectorAuthRequirement {
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

impl RuntimeConnectorAuthRequirement {
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
pub struct RuntimeConnectorAction {
    /// Stable action identifier within the connector.
    pub id: String,
    /// Human-readable operation description.
    #[serde(default)]
    pub description: String,
    /// Fixed constrained-HTTP operation and governed contract.
    pub contract: HttpOperationContract,
}

/// Governed schemas, data classes, policy floor, and retry semantics for an operation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeOperationPolicy {
    /// JSON object schema offered to the model for operation inputs.
    pub input_schema: Value,
    /// JSON object schema enforced for operation outputs.
    pub output_schema: Value,
    /// Data sensitivity classes this operation may transmit.
    pub data_classes: Vec<SensitivityClass>,
    /// Declared retry semantics for the underlying operation.
    pub idempotency: IdempotencyClass,
}

/// Fixed constrained HTTP operation compiled from a runtime connector definition.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpOperationContract {
    /// Fixed HTTP method.
    pub method: HttpMethod,
    /// Fixed origin-relative path template.
    pub path_template: String,
    /// Complete-segment path placeholder mappings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_inputs: Vec<HttpPathInput>,
    /// Fixed query parameter mappings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_inputs: Vec<HttpQueryInput>,
    /// Optional JSON request-body mapping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_input: Option<HttpBodyInput>,
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
    pub policy: RuntimeOperationPolicy,
}

/// Closed set of HTTP methods accepted by custom connectors.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
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
pub struct HttpPathInput {
    /// Placeholder name without braces.
    pub placeholder: String,
    /// RFC 6901 pointer into the validated operation input.
    pub input_pointer: String,
}

/// Maps one fixed query parameter to model input.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpQueryInput {
    /// Fixed query parameter name.
    pub parameter: String,
    /// RFC 6901 pointer into the validated operation input.
    pub input_pointer: String,
}

/// Maps a JSON request body from model input.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpBodyInput {
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

pub(crate) fn is_runtime_action_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=24).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Validates a definition-local connector action identifier.
pub fn validate_connector_action_id(value: &str) -> Result<(), ConnectorActionIdError> {
    is_runtime_action_id(value)
        .then_some(())
        .ok_or_else(|| ConnectorActionIdError(value.to_string()))
}

/// Failure returned when a connector action identifier violates the artifact grammar.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("connector action id `{0}` must match [A-Za-z][A-Za-z0-9_-]{{0,23}}")]
pub struct ConnectorActionIdError(String);
