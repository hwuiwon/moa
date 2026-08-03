//! Secret-free wire contracts for tenant connector connection management.

use std::num::NonZeroU64;

use chrono::{DateTime, Utc};
use moa_core::types::credentials::{CredentialKind, CredentialSlotName};
use moa_core::types::identifiers::ConnectorConnectionId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Exact private orchestrator path for opaque connector credential writes.
pub const CONNECTOR_CREDENTIAL_INGRESS_PATH: &str = "/internal/v1/connectors/credentials/write";

/// Edge-injected selector header that must match credential request metadata.
pub const CONNECTOR_CONNECTION_ID_HEADER: &str = "x-moa-connector-connection-id";

/// Edge-injected credential-slot header that must match request metadata.
pub const CONNECTOR_CREDENTIAL_SLOT_HEADER: &str = "x-moa-connector-credential-slot";

/// Exact immutable connector definition selected for one connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConnectorDefinitionReference {
    /// One exact published connector artifact revision.
    Artifact {
        /// Stable connector artifact identity.
        artifact_uid: Uuid,
        /// Exact immutable artifact revision identity.
        revision_uid: Uuid,
    },
    /// One exact code-owned managed connector definition.
    BuiltIn {
        /// Stable code-owned connector key.
        key: String,
        /// Positive code-owned definition version.
        version: NonZeroU64,
    },
}

/// Request to create one tenant connector connection.
///
/// The tenant and caller identity are intentionally absent. Ingress derives
/// both from the authenticated request context before authorizing definition
/// lookup or constructing a domain command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConnectionCreateRequest {
    /// Replay-stable connection identity selected by the caller.
    pub connection_id: ConnectorConnectionId,
    /// Operator-visible connection label.
    pub display_name: String,
    /// Exact immutable artifact or built-in definition reference.
    pub definition_ref: ConnectorDefinitionReference,
    /// Fixed HTTP(S) origin for runtimes whose reviewed definition requires one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Definition-validated configuration that contains no credential material.
    #[serde(default = "empty_json_object")]
    pub non_secret_config: Value,
}

/// Generation-fenced request for one connection lifecycle operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConnectionMutationRequest {
    /// Generation observed before the requested mutation.
    pub expected_generation: u64,
}

/// Stable tenant connection lifecycle exposed by the management API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorConnectionStatus {
    /// Installed but waiting for credentials or authorization completion.
    PendingAuth,
    /// Eligible for governed catalog projection.
    Active,
    /// Intentionally unavailable while retaining configuration and credentials.
    Suspended,
    /// Teardown has started and no new action may begin.
    Disconnecting,
    /// Terminal retained record.
    Deleted,
}

/// Sanitized health state exposed by the management API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorConnectionHealth {
    /// No conclusive health observation is available.
    Pending,
    /// Local admission and any reviewed remote verification passed.
    Ready,
    /// The connection is usable with a known sanitized impairment.
    Degraded,
    /// The connection cannot currently serve calls.
    Unavailable,
    /// Security or operator policy isolated the connection.
    Quarantined,
}

/// Whether connector verification included a reviewed remote authentication probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorVerificationState {
    /// Verification has not reached a conclusive result.
    Pending,
    /// Local destination admission and credential readiness ran, but no reviewed
    /// remote verification contract exists.
    Unverified,
    /// A reviewed connector-level remote verification contract passed.
    Verified,
}

/// Secret-free readiness for one exact credential slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorCredentialSlotResponse {
    /// Logical connector credential slot.
    pub slot_name: CredentialSlotName,
    /// Material kind required by the connector definition.
    pub kind: CredentialKind,
    /// Whether an active usable version is available for this exact slot.
    pub ready: bool,
}

/// Public metadata for one tenant connector connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConnectionResponse {
    /// Stable tenant connection identity.
    pub connection_id: ConnectorConnectionId,
    /// Operator-visible connection label.
    pub display_name: String,
    /// Exact immutable definition backing the connection.
    pub definition_ref: ConnectorDefinitionReference,
    /// Definition-validated configuration containing no credential material.
    pub non_secret_config: Value,
    /// Current optimistic-concurrency and binding generation.
    pub generation: u64,
    /// Current lifecycle state.
    pub status: ConnectorConnectionStatus,
    /// Latest sanitized health observation.
    pub health: ConnectorConnectionHealth,
    /// Bounded sanitized health reason, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_reason: Option<String>,
    /// Required credential slots and availability only.
    #[serde(default)]
    pub credential_slots: Vec<ConnectorCredentialSlotResponse>,
    /// Durable creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Timestamp of the latest generation-fenced state write.
    pub updated_at: DateTime<Utc>,
}

/// Public list response for one already-authorized tenant scope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConnectionListResponse {
    /// Connections in deterministic connection-identity order.
    pub connections: Vec<ConnectorConnectionResponse>,
}

/// Sanitized result of connector destination and credential verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConnectionVerificationResponse {
    /// Generation whose destination and slot state was verified.
    pub generation: u64,
    /// Whether a reviewed remote authentication probe ran successfully.
    pub verification: ConnectorVerificationState,
    /// Sanitized resulting connection health.
    pub health: ConnectorConnectionHealth,
    /// Bounded stable reason code or explanation without upstream response data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Required credential slots and availability only.
    pub credential_slots: Vec<ConnectorCredentialSlotResponse>,
}

/// Closed same-tenant subject eligible for direct connector `Use` access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConnectorUseSubject {
    /// Tenant operator identity.
    Operator {
        /// Existing same-tenant operator identity.
        id: Uuid,
    },
    /// Tenant agent identity.
    Agent {
        /// Existing same-tenant agent identity.
        id: Uuid,
    },
    /// Tenant contact identity.
    Contact {
        /// Existing same-tenant contact identity.
        id: Uuid,
    },
}

/// Request to grant or revoke one direct connector `Use` relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorConnectionUseRequest {
    /// Exact same-tenant subject whose direct relationship changes.
    pub subject: ConnectorUseSubject,
}

/// Secret-free metadata accompanying a private credential-ingress write.
///
/// Credential plaintext is deliberately absent from the shared wire crate. The
/// private orchestrator ingress combines this metadata with its own bounded,
/// deserialize-only secret wrapper after the public edge has authenticated and
/// body-limited the opaque request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorCredentialWriteMetadata {
    /// Connection receiving the credential slot.
    pub connection_id: ConnectorConnectionId,
    /// Generation observed before the credential write began.
    pub expected_generation: u64,
    /// Exact logical credential slot declared by the definition.
    pub slot_name: CredentialSlotName,
    /// Material kind declared for the slot.
    pub kind: CredentialKind,
    /// Replay-stable operation identifier containing no caller-controlled text.
    pub operation_id: Uuid,
}

fn empty_json_object() -> Value {
    Value::Object(serde_json::Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FIXTURE_SECRET: &str = "fixture_secret_must_never_leave_private_ingress";
    const FIXTURE_CREDENTIAL_REF: &str = "credential_ref_must_never_reach_wire";

    #[test]
    fn connector_connection_response_serializes_only_secret_free_slot_readiness() {
        // Pins: public connection metadata can expose slot names/readiness but
        // has no field capable of carrying a credential ref, version, or plaintext.
        let connection_id = ConnectorConnectionId::new();
        let artifact_uid = Uuid::new_v4();
        let revision_uid = Uuid::new_v4();
        let timestamp = DateTime::parse_from_rfc3339("2026-08-02T12:00:00Z")
            .expect("fixture timestamp should parse")
            .with_timezone(&Utc);
        let response = ConnectorConnectionResponse {
            connection_id,
            display_name: "Billing API".to_string(),
            definition_ref: ConnectorDefinitionReference::Artifact {
                artifact_uid,
                revision_uid,
            },
            non_secret_config: json!({"origin": "https://billing.example.com"}),
            generation: 4,
            status: ConnectorConnectionStatus::Suspended,
            health: ConnectorConnectionHealth::Pending,
            health_reason: None,
            credential_slots: vec![ConnectorCredentialSlotResponse {
                slot_name: CredentialSlotName::PRIMARY,
                kind: CredentialKind::ProviderApiKey,
                ready: true,
            }],
            created_at: timestamp,
            updated_at: timestamp,
        };

        let value = serde_json::to_value(response)
            .expect("secret-free connector response should serialize");
        assert_eq!(
            value,
            json!({
                "connection_id": connection_id,
                "display_name": "Billing API",
                "definition_ref": {
                    "kind": "artifact",
                    "artifact_uid": artifact_uid,
                    "revision_uid": revision_uid,
                },
                "non_secret_config": {"origin": "https://billing.example.com"},
                "generation": 4,
                "status": "suspended",
                "health": "pending",
                "credential_slots": [{
                    "slot_name": "primary",
                    "kind": "provider_api_key",
                    "ready": true,
                }],
                "created_at": "2026-08-02T12:00:00Z",
                "updated_at": "2026-08-02T12:00:00Z",
            }),
            "public response must remain an exact secret-free contract"
        );
        let encoded =
            serde_json::to_string(&value).expect("connector response JSON value should serialize");
        assert!(!encoded.contains(FIXTURE_SECRET));
        assert!(!encoded.contains(FIXTURE_CREDENTIAL_REF));
        assert!(!encoded.contains("credential_ref"));
        assert!(!encoded.contains("credential_version"));
    }

    #[test]
    fn connector_create_request_rejects_tenant_definition_body_and_credential_fields() {
        // Pins: tenant identity is derived from authentication, definitions are
        // referenced rather than supplied, and credentials use private ingress.
        let base = json!({
            "connection_id": ConnectorConnectionId::new(),
            "display_name": "Billing API",
            "definition_ref": {
                "kind": "artifact",
                "artifact_uid": Uuid::new_v4(),
                "revision_uid": Uuid::new_v4(),
            },
            "origin": "https://billing.example.com",
            "non_secret_config": {"region": "us-east-1"},
        });

        serde_json::from_value::<ConnectorConnectionCreateRequest>(base.clone())
            .expect("exact secret-free create shape should deserialize");
        for forbidden in ["tenant_id", "definition", "credential", "credential_ref"] {
            let mut invalid = base.clone();
            invalid
                .as_object_mut()
                .expect("fixture must be an object")
                .insert(forbidden.to_string(), json!(FIXTURE_SECRET));
            assert!(
                serde_json::from_value::<ConnectorConnectionCreateRequest>(invalid).is_err(),
                "create request must reject forbidden field `{forbidden}`"
            );
        }
    }

    #[test]
    fn credential_write_metadata_validates_slot_and_cannot_deserialize_material() {
        // Pins: shared metadata validates the exact slot grammar and rejects any
        // attempt to smuggle credential material onto a Restate-facing DTO.
        let metadata = json!({
            "connection_id": ConnectorConnectionId::new(),
            "expected_generation": 7,
            "slot_name": "secondary_key",
            "kind": "provider_api_key",
            "operation_id": Uuid::new_v4(),
        });
        serde_json::from_value::<ConnectorCredentialWriteMetadata>(metadata.clone())
            .expect("valid secret-free credential metadata should deserialize");

        let mut with_material = metadata.clone();
        with_material
            .as_object_mut()
            .expect("fixture must be an object")
            .insert("material".to_string(), json!(FIXTURE_SECRET));
        assert!(
            serde_json::from_value::<ConnectorCredentialWriteMetadata>(with_material).is_err(),
            "shared wire metadata must reject credential material"
        );

        let mut invalid_slot = metadata;
        invalid_slot
            .as_object_mut()
            .expect("fixture must be an object")
            .insert("slot_name".to_string(), json!("Invalid-Slot"));
        assert!(
            serde_json::from_value::<ConnectorCredentialWriteMetadata>(invalid_slot).is_err(),
            "credential slot grammar must fail closed at deserialization"
        );
    }

    #[test]
    fn connector_use_subject_is_closed_and_tenantless() {
        // Pins: grant/revoke input names one closed subject kind but cannot pick
        // a tenant or construct an arbitrary OpenFGA user string.
        let subject_id = Uuid::new_v4();
        let request: ConnectorConnectionUseRequest = serde_json::from_value(json!({
            "subject": {"type": "agent", "id": subject_id}
        }))
        .expect("closed agent subject should deserialize");
        assert_eq!(
            request.subject,
            ConnectorUseSubject::Agent { id: subject_id }
        );

        for invalid in [
            json!({"subject": {"type": "group", "id": Uuid::new_v4()}}),
            json!({
                "subject": {"type": "agent", "id": subject_id},
                "tenant_id": Uuid::new_v4(),
            }),
            json!({"subject": "agent:anything"}),
        ] {
            assert!(
                serde_json::from_value::<ConnectorConnectionUseRequest>(invalid).is_err(),
                "grant/revoke input must reject open or tenant-selected subjects"
            );
        }
    }
}
