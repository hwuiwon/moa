//! Connector domain-to-wire translation helpers.

use moa_connectors::domain::{
    ConnectionDefinitionRef, ConnectionHealth, ConnectionStatus, ConnectorConnection,
};
use moa_connectors::repository::ConnectorUseSubject;
use moa_connectors::service::CredentialSlotReadiness;
use moa_wire::connectors::{
    ConnectorArtifactReference, ConnectorConnectionHealth, ConnectorConnectionResponse,
    ConnectorConnectionStatus, ConnectorCredentialSlotResponse, ConnectorDefinitionReference,
    ConnectorUseSubject as WireUseSubject,
};

pub(super) fn wire_artifact_definition_to_domain(
    reference: &ConnectorArtifactReference,
) -> ConnectionDefinitionRef {
    ConnectionDefinitionRef::Artifact {
        artifact_uid: reference.artifact_uid,
        revision_uid: reference.revision_uid,
    }
}

pub(super) fn domain_definition_to_wire(
    reference: &ConnectionDefinitionRef,
) -> ConnectorDefinitionReference {
    match reference {
        ConnectionDefinitionRef::Artifact {
            artifact_uid,
            revision_uid,
        } => ConnectorDefinitionReference::Artifact {
            artifact_uid: *artifact_uid,
            revision_uid: *revision_uid,
        },
        ConnectionDefinitionRef::BuiltIn { key, version } => {
            ConnectorDefinitionReference::BuiltIn {
                key: key.clone(),
                version: *version,
            }
        }
    }
}
pub(super) fn use_subject(subject: WireUseSubject) -> ConnectorUseSubject {
    match subject {
        WireUseSubject::Operator { id } => ConnectorUseSubject::Operator { id },
        WireUseSubject::Agent { id } => ConnectorUseSubject::Agent { id },
        WireUseSubject::Contact { id } => ConnectorUseSubject::Contact { id },
    }
}
pub(super) fn connection_response(
    connection: ConnectorConnection,
    readiness: Vec<CredentialSlotReadiness>,
) -> ConnectorConnectionResponse {
    ConnectorConnectionResponse {
        connection_id: connection.connection_id,
        display_name: connection.display_name,
        definition_ref: domain_definition_to_wire(&connection.definition),
        origin: connection.origin.map(|origin| origin.to_string()),
        non_secret_config: connection.non_secret_config,
        generation: connection.generation.get(),
        status: wire_status(connection.status),
        health: wire_health(connection.health),
        health_reason: connection.health_reason,
        credential_slots: readiness.into_iter().map(wire_slot).collect(),
        created_at: connection.created_at,
        updated_at: connection.updated_at,
    }
}

pub(super) fn wire_status(status: ConnectionStatus) -> ConnectorConnectionStatus {
    match status {
        ConnectionStatus::PendingAuth => ConnectorConnectionStatus::PendingAuth,
        ConnectionStatus::Active => ConnectorConnectionStatus::Active,
        ConnectionStatus::Suspended => ConnectorConnectionStatus::Suspended,
        ConnectionStatus::Disconnecting => ConnectorConnectionStatus::Disconnecting,
        ConnectionStatus::Deleted => ConnectorConnectionStatus::Deleted,
    }
}

pub(super) fn wire_health(health: ConnectionHealth) -> ConnectorConnectionHealth {
    match health {
        ConnectionHealth::Pending => ConnectorConnectionHealth::Pending,
        ConnectionHealth::Ready => ConnectorConnectionHealth::Ready,
        ConnectionHealth::Degraded => ConnectorConnectionHealth::Degraded,
        ConnectionHealth::Unavailable => ConnectorConnectionHealth::Unavailable,
        ConnectionHealth::Quarantined => ConnectorConnectionHealth::Quarantined,
    }
}

pub(super) fn wire_slot(readiness: CredentialSlotReadiness) -> ConnectorCredentialSlotResponse {
    ConnectorCredentialSlotResponse {
        slot_name: readiness.slot,
        kind: readiness.kind,
        ready: readiness.ready,
    }
}
