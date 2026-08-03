//! Private, non-Restate request boundary for connector credential writes.
//!
//! This module owns only the inbound shape and secret-memory boundary. HTTP
//! authentication, delegated connector authorization, vault staging, the
//! generation fence, and losing-write compensation are composed by the private
//! ingress handler around these types.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use moa_core::traits::{CredentialVault, Identity};
use moa_core::types::credentials::{
    CredentialContext, CredentialError, CredentialIdentity, CredentialOperation,
    CredentialPrincipal, CredentialSlotName,
};
use moa_wire::connectors::{
    CONNECTOR_CONNECTION_ID_HEADER, CONNECTOR_CREDENTIAL_INGRESS_PATH,
    CONNECTOR_CREDENTIAL_SLOT_HEADER, ConnectorCredentialWriteMetadata,
};
use secrecy::SecretString;
use serde::de::Visitor;
use serde::{Deserialize, Deserializer};

use crate::services::connectors::{ConnectorManagementError, ConnectorManagementService};

/// Maximum accepted HTTP body size for one private credential-ingress request.
///
/// The listener must enforce this limit before buffering or deserializing JSON.
pub const MAX_CONNECTOR_CREDENTIAL_INGRESS_BODY_BYTES: usize = 65_536;

/// Maximum UTF-8 byte length of one connector credential value.
pub const MAX_CONNECTOR_CREDENTIAL_MATERIAL_BYTES: usize = 32_768;

const CREDENTIAL_WRITE_HASH_DOMAIN: &str = "moa.connector.credential-write.v1";

/// Stable failure returned by the private credential boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CredentialIngressError {
    /// Trusted edge headers or request selectors were malformed.
    #[error("invalid connector credential request")]
    InvalidRequest,
    /// No authenticated identity reached the private listener.
    #[error("connector credential authentication required")]
    Unauthenticated,
    /// Authorization denied the requested connection mutation.
    #[error("connector credential write forbidden")]
    Forbidden,
    /// An optimistic generation, replay, or activation fence lost.
    #[error("connector credential write conflict")]
    Conflict,
    /// The requested connection definition or credential slot is invalid.
    #[error("connector credential request cannot be applied")]
    Unprocessable,
    /// An authorization, persistence, KMS, or compensation dependency failed.
    #[error("connector credential service unavailable")]
    Unavailable,
}

/// Secret-free management seam used by the plaintext-owning ingress controller.
#[async_trait]
pub trait CredentialIngressCoordinator: Send + Sync {
    /// Authorizes `Manage` before validating the exact installed slot selector.
    async fn admit(
        &self,
        identity: &Identity,
        metadata: &ConnectorCredentialWriteMetadata,
    ) -> Result<CredentialIdentity, ConnectorManagementError>;

    /// Reauthorizes and advances or replays the secret-free generation fence.
    async fn advance_generation(
        &self,
        identity: &Identity,
        metadata: &ConnectorCredentialWriteMetadata,
    ) -> Result<(), ConnectorManagementError>;
}

/// Thin adapter from the management application service to private ingress.
#[derive(Clone)]
pub struct ManagementCredentialIngressCoordinator {
    management: ConnectorManagementService,
}

impl ManagementCredentialIngressCoordinator {
    /// Creates a coordinator around the shared connector-management service.
    #[must_use]
    pub fn new(management: ConnectorManagementService) -> Self {
        Self { management }
    }
}

#[async_trait]
impl CredentialIngressCoordinator for ManagementCredentialIngressCoordinator {
    async fn admit(
        &self,
        identity: &Identity,
        metadata: &ConnectorCredentialWriteMetadata,
    ) -> Result<CredentialIdentity, ConnectorManagementError> {
        self.management
            .prepare_credential_write(identity, metadata)
            .await
            .map(|prepared| prepared.credential_identity().clone())
    }

    async fn advance_generation(
        &self,
        identity: &Identity,
        metadata: &ConnectorCredentialWriteMetadata,
    ) -> Result<(), ConnectorManagementError> {
        let prepared = self
            .management
            .prepare_credential_write(identity, metadata)
            .await?;
        self.management
            .advance_credential_generation(identity, &prepared)
            .await
            .map(|_| ())
    }
}

/// Host-local controller for one staged credential write.
#[derive(Clone)]
pub struct ConnectorCredentialIngress {
    coordinator: Arc<dyn CredentialIngressCoordinator>,
    vault: Arc<dyn CredentialVault>,
}

impl ConnectorCredentialIngress {
    /// Creates the controller from secret-free management and vault owners.
    #[must_use]
    pub fn new(
        coordinator: Arc<dyn CredentialIngressCoordinator>,
        vault: Arc<dyn CredentialVault>,
    ) -> Self {
        Self { coordinator, vault }
    }

    /// Applies one authorized stage, generation fence, and activation sequence.
    ///
    /// The staging token remains on this stack and never enters Restate. A
    /// definitive fence/CAS loser revokes only its inactive staged version. An
    /// indeterminate storage result remains fenced for replay/reconciliation and
    /// is never followed by an unsafe revocation of a possibly active winner.
    pub async fn write(
        &self,
        identity: &Identity,
        request: ConnectorCredentialIngressRequest,
    ) -> Result<(), CredentialIngressError> {
        let request_hash = request
            .request_hash()
            .map_err(|_| CredentialIngressError::InvalidRequest)?;
        let credential_identity = self
            .coordinator
            .admit(identity, request.metadata())
            .await
            .map_err(map_management_error)?;
        let (metadata, material) = request.into_parts();
        let principal = credential_principal(identity);
        let staged = self
            .vault
            .stage(
                credential_identity,
                material.into_secret_string(),
                &credential_context(
                    identity,
                    principal,
                    metadata.operation_id,
                    &request_hash,
                    CredentialOperation::Stage,
                ),
            )
            .await
            .map_err(map_credential_error)?;

        if let Err(error) = self
            .coordinator
            .advance_generation(identity, &metadata)
            .await
        {
            self.revoke_loser(
                identity,
                principal,
                metadata.operation_id,
                &request_hash,
                &staged,
            )
            .await?;
            return Err(map_management_error(error));
        }

        let activation = self
            .vault
            .activate_staged(
                &staged,
                &credential_context(
                    identity,
                    principal,
                    metadata.operation_id,
                    &request_hash,
                    CredentialOperation::Activate,
                ),
            )
            .await;
        match activation {
            Ok(_) => Ok(()),
            Err(error) if credential_error_is_definitive_loser(&error) => {
                self.revoke_loser(
                    identity,
                    principal,
                    metadata.operation_id,
                    &request_hash,
                    &staged,
                )
                .await?;
                Err(map_credential_error(error))
            }
            Err(CredentialError::Storage(_)) => Err(CredentialIngressError::Unavailable),
            Err(error) => Err(map_credential_error(error)),
        }
    }

    async fn revoke_loser(
        &self,
        identity: &Identity,
        principal: CredentialPrincipal,
        operation_id: uuid::Uuid,
        request_hash: &str,
        staged: &moa_core::types::credentials::CredentialStagingToken,
    ) -> Result<(), CredentialIngressError> {
        self.vault
            .revoke(
                staged.staged_reference(),
                &credential_context(
                    identity,
                    principal,
                    operation_id,
                    request_hash,
                    CredentialOperation::Revoke,
                ),
            )
            .await
            .map_err(|_| CredentialIngressError::Unavailable)
    }
}

fn credential_principal(identity: &Identity) -> CredentialPrincipal {
    CredentialPrincipal::Caller {
        identity_id: identity.id,
        delegated_by: identity.acting_on_behalf_of,
    }
}

fn credential_context(
    identity: &Identity,
    principal: CredentialPrincipal,
    operation_id: uuid::Uuid,
    request_hash: &str,
    operation: CredentialOperation,
) -> CredentialContext {
    CredentialContext {
        tenant_id: identity.tenant_id,
        principal,
        operation,
        operation_id: format!("{operation_id}:{}", operation.as_str()),
        request_hash: phase_request_hash(request_hash, operation),
    }
}

fn phase_request_hash(request_hash: &str, operation: CredentialOperation) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in [
        CREDENTIAL_WRITE_HASH_DOMAIN.as_bytes(),
        operation.as_str().as_bytes(),
        request_hash.as_bytes(),
    ] {
        hasher.update(&(part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().to_hex().to_string()
}

fn credential_error_is_definitive_loser(error: &CredentialError) -> bool {
    matches!(
        error,
        CredentialError::NotFound
            | CredentialError::Revoked
            | CredentialError::StaleVersion
            | CredentialError::WrongTenant
            | CredentialError::WrongConnection
            | CredentialError::WrongKind
            | CredentialError::Unauthorized
            | CredentialError::IdempotencyConflict
            | CredentialError::VersionConflict
    )
}

fn map_credential_error(error: CredentialError) -> CredentialIngressError {
    match error {
        CredentialError::Unauthorized => CredentialIngressError::Forbidden,
        CredentialError::IdempotencyConflict
        | CredentialError::VersionConflict
        | CredentialError::Revoked
        | CredentialError::StaleVersion => CredentialIngressError::Conflict,
        CredentialError::NotFound
        | CredentialError::WrongTenant
        | CredentialError::WrongConnection
        | CredentialError::WrongKind => CredentialIngressError::Unprocessable,
        CredentialError::DeploymentSecretMissing | CredentialError::Storage(_) => {
            CredentialIngressError::Unavailable
        }
    }
}

fn map_management_error(error: ConnectorManagementError) -> CredentialIngressError {
    use crate::services::connectors::{
        ConnectorDefinitionResolutionError, ConnectorManagementAuthorizationError,
    };

    match error {
        ConnectorManagementError::Authorization(ConnectorManagementAuthorizationError::Denied) => {
            CredentialIngressError::Forbidden
        }
        ConnectorManagementError::Authorization(
            ConnectorManagementAuthorizationError::Unavailable,
        )
        | ConnectorManagementError::Destination(_)
        | ConnectorManagementError::CredentialRevocation(_) => CredentialIngressError::Unavailable,
        ConnectorManagementError::Definition(ConnectorDefinitionResolutionError::NotFound)
        | ConnectorManagementError::Connector(moa_connectors::Error::ConnectionNotFound {
            ..
        }) => CredentialIngressError::Unprocessable,
        ConnectorManagementError::Definition(ConnectorDefinitionResolutionError::Unavailable) => {
            CredentialIngressError::Unavailable
        }
        ConnectorManagementError::Connector(moa_connectors::Error::GenerationConflict {
            ..
        }) => CredentialIngressError::Conflict,
        ConnectorManagementError::Definition(_)
        | ConnectorManagementError::UnsupportedOwnerIdentity
        | ConnectorManagementError::DefinitionReferenceMismatch
        | ConnectorManagementError::CredentialSlotMismatch
        | ConnectorManagementError::ManagedKnowledgeOperation(_)
        | ConnectorManagementError::Connector(_) => CredentialIngressError::Unprocessable,
    }
}

/// Builds the exact private connector-credential HTTP surface.
pub fn router(ingress: ConnectorCredentialIngress) -> Router {
    Router::new()
        .route(CONNECTOR_CREDENTIAL_INGRESS_PATH, post(write_handler))
        .layer(DefaultBodyLimit::max(
            MAX_CONNECTOR_CREDENTIAL_INGRESS_BODY_BYTES,
        ))
        .with_state(ingress)
}

async fn write_handler(
    State(ingress): State<ConnectorCredentialIngress>,
    headers: HeaderMap,
    Json(request): Json<ConnectorCredentialIngressRequest>,
) -> Response {
    let result = async {
        require_json_content_type(&headers)?;
        let identity = extract_trusted_identity(&headers)?;
        require_selector_match(&headers, request.metadata())?;
        ingress.write(&identity, request).await
    }
    .await;
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => ingress_error_response(error),
    }
}

fn require_json_content_type(headers: &HeaderMap) -> Result<(), CredentialIngressError> {
    let values = headers.get_all(CONTENT_TYPE);
    let mut values = values.iter();
    let value = values
        .next()
        .ok_or(CredentialIngressError::InvalidRequest)?;
    if values.next().is_some() {
        return Err(CredentialIngressError::InvalidRequest);
    }
    let value = value
        .to_str()
        .map_err(|_| CredentialIngressError::InvalidRequest)?;
    let media_type = value.split(';').next().unwrap_or_default().trim();
    if media_type.eq_ignore_ascii_case("application/json") {
        Ok(())
    } else {
        Err(CredentialIngressError::InvalidRequest)
    }
}

fn extract_trusted_identity(headers: &HeaderMap) -> Result<Identity, CredentialIngressError> {
    const IDENTITY_HEADERS: [&str; 5] = [
        "x-moa-identity-type",
        "x-moa-identity-id",
        "x-moa-tenant-id",
        "x-moa-api-key-id",
        "x-moa-acting-on-behalf-of",
    ];
    let mut adapted = restate_sdk::context::HeaderMap::with_capacity(IDENTITY_HEADERS.len());
    for name in IDENTITY_HEADERS {
        let values = headers.get_all(name);
        let mut values = values.iter();
        let Some(value) = values.next() else {
            continue;
        };
        if values.next().is_some() {
            return Err(CredentialIngressError::InvalidRequest);
        }
        let value = value
            .to_str()
            .map_err(|_| CredentialIngressError::InvalidRequest)?;
        adapted.insert(name, value.to_string());
    }
    crate::ctx::extract_identity(&adapted)
        .map_err(|error| match error {
            crate::ctx::IdentityHeaderError::Missing(_) => CredentialIngressError::Unauthenticated,
            crate::ctx::IdentityHeaderError::Malformed(_)
            | crate::ctx::IdentityHeaderError::UnknownType(_) => {
                CredentialIngressError::InvalidRequest
            }
        })?
        .ok_or(CredentialIngressError::Unauthenticated)
}

fn require_selector_match(
    headers: &HeaderMap,
    metadata: &ConnectorCredentialWriteMetadata,
) -> Result<(), CredentialIngressError> {
    let connection = one_header(headers, CONNECTOR_CONNECTION_ID_HEADER)?;
    let connection =
        uuid::Uuid::parse_str(connection).map_err(|_| CredentialIngressError::InvalidRequest)?;
    let slot = CredentialSlotName::try_from(one_header(headers, CONNECTOR_CREDENTIAL_SLOT_HEADER)?)
        .map_err(|_| CredentialIngressError::InvalidRequest)?;
    if connection == metadata.connection_id.0 && slot == metadata.slot_name {
        Ok(())
    } else {
        Err(CredentialIngressError::InvalidRequest)
    }
}

fn one_header<'a>(
    headers: &'a HeaderMap,
    name: &'static str,
) -> Result<&'a str, CredentialIngressError> {
    let values = headers.get_all(name);
    let mut values = values.iter();
    let value = values
        .next()
        .ok_or(CredentialIngressError::InvalidRequest)?;
    if values.next().is_some() {
        return Err(CredentialIngressError::InvalidRequest);
    }
    value
        .to_str()
        .map_err(|_| CredentialIngressError::InvalidRequest)
}

fn ingress_error_response(error: CredentialIngressError) -> Response {
    let (status, body) = match error {
        CredentialIngressError::InvalidRequest => {
            (StatusCode::BAD_REQUEST, "invalid credential request")
        }
        CredentialIngressError::Unauthenticated => {
            (StatusCode::UNAUTHORIZED, "authentication required")
        }
        CredentialIngressError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
        CredentialIngressError::Conflict => (StatusCode::CONFLICT, "credential conflict"),
        CredentialIngressError::Unprocessable => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "credential request cannot be applied",
        ),
        CredentialIngressError::Unavailable => (
            StatusCode::SERVICE_UNAVAILABLE,
            "credential service unavailable",
        ),
    };
    (status, body).into_response()
}

/// Bounded connector credential plaintext accepted only by private ingress.
///
/// The type deliberately implements neither [`Debug`](fmt::Debug), `Clone`, nor
/// `Serialize`. Its only public conversion consumes the wrapper into a
/// [`SecretString`], which zeroizes its allocation on drop.
pub struct BoundedCredentialMaterial(SecretString);

impl BoundedCredentialMaterial {
    /// Transfers ownership to the credential vault boundary.
    #[must_use]
    pub fn into_secret_string(self) -> SecretString {
        self.0
    }
}

impl<'de> Deserialize<'de> for BoundedCredentialMaterial {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_string(CredentialMaterialVisitor)
    }
}

struct CredentialMaterialVisitor;

impl Visitor<'_> for CredentialMaterialVisitor {
    type Value = BoundedCredentialMaterial;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "a non-empty credential string of at most {MAX_CONNECTOR_CREDENTIAL_MATERIAL_BYTES} UTF-8 bytes"
        )
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_string(value.to_owned())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        let bytes = value.len();
        if !(1..=MAX_CONNECTOR_CREDENTIAL_MATERIAL_BYTES).contains(&bytes) {
            return Err(E::invalid_length(bytes, &self));
        }
        Ok(BoundedCredentialMaterial(SecretString::from(value)))
    }
}

/// Private request to stage and generation-fence one connector credential slot.
///
/// Tenant and caller identities are absent by construction. The private ingress
/// derives both from its trusted authenticated headers before any connector,
/// artifact, or vault read. This request deliberately implements neither
/// `Debug`, `Clone`, nor `Serialize` so material cannot cross into Restate or
/// observability payloads by ordinary trait use.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorCredentialIngressRequest {
    /// Secret-free selector, generation fence, and replay identity.
    metadata: ConnectorCredentialWriteMetadata,
    /// Plaintext transferred directly into the orchestrator-owned vault.
    material: BoundedCredentialMaterial,
}

impl ConnectorCredentialIngressRequest {
    /// Returns the secret-free selector needed for authorization and admission.
    #[must_use]
    pub const fn metadata(&self) -> &ConnectorCredentialWriteMetadata {
        &self.metadata
    }

    /// Computes the replay request hash over secret-free metadata only.
    ///
    /// Credential material is intentionally not an input: operation replay can
    /// detect selector/generation changes without retaining or hashing plaintext.
    pub fn request_hash(&self) -> Result<String, serde_json::Error> {
        let metadata = serde_json::to_vec(&self.metadata)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(CREDENTIAL_WRITE_HASH_DOMAIN.as_bytes());
        hasher.update(&[0]);
        hasher.update(&metadata);
        Ok(hasher.finalize().to_hex().to_string())
    }

    /// Splits the request after authentication, authorization, and admission.
    #[must_use]
    pub fn into_parts(self) -> (ConnectorCredentialWriteMetadata, BoundedCredentialMaterial) {
        (self.metadata, self.material)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use axum::http::HeaderValue;
    use chrono::Utc;
    use moa_connectors::domain::ConnectionGeneration;
    use moa_core::traits::IdentityType;
    use moa_core::types::credentials::{
        CredentialSource, CredentialStagingToken, CredentialVersion, RedactedSecret,
    };
    use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
    use secrecy::ExposeSecret;
    use serde_json::json;

    const FIXTURE_SECRET: &str = "fixture_secret_private_ingress_only";
    const OTHER_FIXTURE_SECRET: &str = "different_fixture_secret_private_ingress_only";

    fn request_value(material: &str) -> serde_json::Value {
        json!({
            "metadata": {
                "connection_id": ConnectorConnectionId::new(),
                "expected_generation": 11,
                "slot_name": "primary",
                "kind": "provider_api_key",
                "operation_id": uuid::Uuid::new_v4(),
            },
            "material": material,
        })
    }

    fn identity() -> Identity {
        Identity {
            identity_type: IdentityType::Operator,
            id: uuid::Uuid::new_v4(),
            tenant_id: TenantId::new(),
            api_key_id: None,
            acting_on_behalf_of: None,
        }
    }

    #[derive(Clone, Copy)]
    enum ActivationOutcome {
        Success,
        Conflict,
        StorageUnknown,
    }

    struct RecordingCoordinator {
        events: Arc<Mutex<Vec<&'static str>>>,
        fence_conflict: bool,
    }

    #[async_trait]
    impl CredentialIngressCoordinator for RecordingCoordinator {
        async fn admit(
            &self,
            identity: &Identity,
            metadata: &ConnectorCredentialWriteMetadata,
        ) -> Result<CredentialIdentity, ConnectorManagementError> {
            self.events.lock().expect("events").push("admit");
            Ok(CredentialIdentity {
                tenant_id: identity.tenant_id,
                connection_uid: metadata.connection_id.0,
                kind: metadata.kind,
                slot_name: metadata.slot_name.clone(),
            })
        }

        async fn advance_generation(
            &self,
            _identity: &Identity,
            metadata: &ConnectorCredentialWriteMetadata,
        ) -> Result<(), ConnectorManagementError> {
            self.events.lock().expect("events").push("fence");
            if self.fence_conflict {
                return Err(moa_connectors::Error::GenerationConflict {
                    expected: ConnectionGeneration::new(metadata.expected_generation)
                        .expect("fixture generation"),
                    actual: ConnectionGeneration::new(metadata.expected_generation + 1)
                        .expect("fixture next generation"),
                }
                .into());
            }
            Ok(())
        }
    }

    struct RecordingVault {
        events: Arc<Mutex<Vec<&'static str>>>,
        activation: ActivationOutcome,
    }

    impl RecordingVault {
        fn version(staged: &CredentialStagingToken) -> CredentialVersion {
            CredentialVersion {
                reference: staged.staged_reference(),
                identity: staged.identity().clone(),
                version: staged.version(),
                active: true,
                revoked: false,
                created_at: Utc::now(),
            }
        }
    }

    #[async_trait]
    impl CredentialVault for RecordingVault {
        async fn create(
            &self,
            _identity: CredentialIdentity,
            _material: SecretString,
            _ctx: &CredentialContext,
        ) -> Result<CredentialVersion, CredentialError> {
            Err(CredentialError::NotFound)
        }

        async fn stage(
            &self,
            identity: CredentialIdentity,
            _material: SecretString,
            ctx: &CredentialContext,
        ) -> Result<CredentialStagingToken, CredentialError> {
            assert_eq!(ctx.operation, CredentialOperation::Stage);
            self.events.lock().expect("events").push("stage");
            Ok(CredentialStagingToken::new(
                moa_core::types::credentials::CredentialRef::from_uuid(uuid::Uuid::new_v4()),
                identity,
                1,
                None,
            ))
        }

        async fn activate_staged(
            &self,
            staged: &CredentialStagingToken,
            ctx: &CredentialContext,
        ) -> Result<CredentialVersion, CredentialError> {
            assert_eq!(ctx.operation, CredentialOperation::Activate);
            self.events.lock().expect("events").push("activate");
            match self.activation {
                ActivationOutcome::Success => Ok(Self::version(staged)),
                ActivationOutcome::Conflict => Err(CredentialError::VersionConflict),
                ActivationOutcome::StorageUnknown => {
                    Err(CredentialError::Storage("fixture unknown".to_string()))
                }
            }
        }

        async fn rollback_activation(
            &self,
            _candidate: moa_core::types::credentials::CredentialRef,
            _prior_active: Option<moa_core::types::credentials::CredentialRef>,
            _ctx: &CredentialContext,
        ) -> Result<CredentialVersion, CredentialError> {
            Err(CredentialError::NotFound)
        }

        async fn resolve(
            &self,
            _source: &CredentialSource,
            _ctx: &CredentialContext,
        ) -> Result<RedactedSecret, CredentialError> {
            Err(CredentialError::NotFound)
        }

        async fn has_active(
            &self,
            _identity: &CredentialIdentity,
            _ctx: &CredentialContext,
        ) -> Result<bool, CredentialError> {
            Ok(false)
        }

        async fn has_active_batch(
            &self,
            identities: &[CredentialIdentity],
            _ctx: &CredentialContext,
        ) -> Result<Vec<bool>, CredentialError> {
            Ok(vec![false; identities.len()])
        }

        async fn resolve_active(
            &self,
            _identity: &CredentialIdentity,
            _ctx: &CredentialContext,
        ) -> Result<RedactedSecret, CredentialError> {
            Err(CredentialError::NotFound)
        }

        async fn describe_batch(
            &self,
            _references: &[(uuid::Uuid, moa_core::types::credentials::CredentialRef)],
            _ctx: &CredentialContext,
        ) -> Result<Vec<(uuid::Uuid, CredentialVersion)>, CredentialError> {
            Ok(Vec::new())
        }

        async fn rotate(
            &self,
            _current: moa_core::types::credentials::CredentialRef,
            _material: SecretString,
            _ctx: &CredentialContext,
        ) -> Result<CredentialVersion, CredentialError> {
            Err(CredentialError::NotFound)
        }

        async fn revoke(
            &self,
            _reference: moa_core::types::credentials::CredentialRef,
            ctx: &CredentialContext,
        ) -> Result<(), CredentialError> {
            assert_eq!(ctx.operation, CredentialOperation::Revoke);
            self.events.lock().expect("events").push("revoke");
            Ok(())
        }

        async fn revoke_connection(
            &self,
            _connection_uid: uuid::Uuid,
            _ctx: &CredentialContext,
        ) -> Result<u64, CredentialError> {
            Ok(0)
        }

        async fn delete_connection(
            &self,
            _connection_uid: uuid::Uuid,
            _ctx: &CredentialContext,
        ) -> Result<u64, CredentialError> {
            Ok(0)
        }

        async fn purge_tenant(
            &self,
            _limit: u32,
            _ctx: &CredentialContext,
        ) -> Result<u64, CredentialError> {
            Ok(0)
        }
    }

    async fn run_recorded_write(
        fence_conflict: bool,
        activation: ActivationOutcome,
    ) -> (Result<(), CredentialIngressError>, Vec<&'static str>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let coordinator: Arc<dyn CredentialIngressCoordinator> = Arc::new(RecordingCoordinator {
            events: Arc::clone(&events),
            fence_conflict,
        });
        let vault: Arc<dyn CredentialVault> = Arc::new(RecordingVault {
            events: Arc::clone(&events),
            activation,
        });
        let ingress = ConnectorCredentialIngress::new(coordinator, vault);
        let request = serde_json::from_value(request_value(FIXTURE_SECRET))
            .expect("fixture request should deserialize");
        let result = ingress.write(&identity(), request).await;
        let recorded = events.lock().expect("events").clone();
        (result, recorded)
    }

    #[test]
    fn private_ingress_accepts_bounded_material_and_consumes_it_into_secret_memory() {
        // Pins: the only plaintext-bearing request type validates the bound and
        // transfers material into zeroizing secret memory without a public clone.
        let request: ConnectorCredentialIngressRequest =
            serde_json::from_value(request_value(FIXTURE_SECRET))
                .expect("bounded private credential request should deserialize");
        assert_eq!(request.metadata().expected_generation, 11);
        let (_, material) = request.into_parts();
        let material = material.into_secret_string();
        assert_eq!(material.expose_secret(), FIXTURE_SECRET);
    }

    #[test]
    fn private_ingress_rejects_empty_oversized_and_non_string_material() {
        // Pins: material is rejected during deserialization before a vault call
        // can allocate or persist an unbounded request.
        for invalid in [
            request_value(""),
            request_value(&"s".repeat(MAX_CONNECTOR_CREDENTIAL_MATERIAL_BYTES + 1)),
            json!({
                "metadata": {
                    "connection_id": ConnectorConnectionId::new(),
                    "expected_generation": 1,
                    "slot_name": "primary",
                    "kind": "provider_api_key",
                    "operation_id": uuid::Uuid::new_v4(),
                },
                "material": {"token": FIXTURE_SECRET},
            }),
        ] {
            assert!(
                serde_json::from_value::<ConnectorCredentialIngressRequest>(invalid).is_err(),
                "invalid private credential material must fail closed"
            );
        }
    }

    #[test]
    fn private_ingress_rejects_caller_selected_identity_and_credential_reference() {
        // Pins: the authenticated private listener, not the body, selects tenant
        // and caller identity; no credential reference may cross this boundary.
        for forbidden in ["tenant_id", "identity_id", "credential_ref"] {
            let mut invalid = request_value(FIXTURE_SECRET);
            invalid
                .as_object_mut()
                .expect("fixture must be an object")
                .insert(forbidden.to_string(), json!(uuid::Uuid::new_v4()));
            assert!(
                serde_json::from_value::<ConnectorCredentialIngressRequest>(invalid).is_err(),
                "private ingress must reject caller-selected `{forbidden}`"
            );
        }
    }

    #[test]
    fn credential_write_request_hash_excludes_plaintext() {
        // Pins: replay identity changes with secret-free selector metadata, not
        // with credential plaintext that must never be retained or journaled.
        let metadata = ConnectorCredentialWriteMetadata {
            connection_id: ConnectorConnectionId::new(),
            expected_generation: 3,
            slot_name: moa_core::types::credentials::CredentialSlotName::PRIMARY,
            kind: moa_core::types::credentials::CredentialKind::ProviderApiKey,
            operation_id: uuid::Uuid::new_v4(),
        };
        let first: ConnectorCredentialIngressRequest = serde_json::from_value(json!({
            "metadata": metadata,
            "material": FIXTURE_SECRET,
        }))
        .expect("first credential request should deserialize");
        let second: ConnectorCredentialIngressRequest = serde_json::from_value(json!({
            "metadata": first.metadata(),
            "material": OTHER_FIXTURE_SECRET,
        }))
        .expect("second credential request should deserialize");

        let first_hash = first
            .request_hash()
            .expect("secret-free metadata should hash");
        let second_hash = second
            .request_hash()
            .expect("secret-free metadata should hash");
        assert_eq!(first_hash, second_hash);
        assert!(!first_hash.contains(FIXTURE_SECRET));
        assert!(!first_hash.contains(OTHER_FIXTURE_SECRET));
    }

    #[tokio::test]
    async fn credential_write_orders_admission_stage_fence_and_activation() {
        // Pins: plaintext is staged only after authorization/admission and its
        // inactive version is activated only after the durable generation fence.
        let (result, events) = run_recorded_write(false, ActivationOutcome::Success).await;

        assert_eq!(result, Ok(()));
        assert_eq!(events, ["admit", "stage", "fence", "activate"]);
    }

    #[tokio::test]
    async fn definitive_fence_and_activation_losers_revoke_only_the_staged_version() {
        // Pins: both a known generation-CAS loser and a known vault activation
        // loser compensate the exact inactive staging token before returning.
        let (fence_result, fence_events) =
            run_recorded_write(true, ActivationOutcome::Success).await;
        assert_eq!(fence_result, Err(CredentialIngressError::Conflict));
        assert_eq!(fence_events, ["admit", "stage", "fence", "revoke"]);

        let (activation_result, activation_events) =
            run_recorded_write(false, ActivationOutcome::Conflict).await;
        assert_eq!(activation_result, Err(CredentialIngressError::Conflict));
        assert_eq!(
            activation_events,
            ["admit", "stage", "fence", "activate", "revoke"]
        );
    }

    #[tokio::test]
    async fn indeterminate_activation_does_not_revoke_a_possibly_active_winner() {
        // Pins: a storage-unknown activation outcome may have committed, so the
        // ingress fails closed and leaves reconciliation to an exact retry.
        let (result, events) = run_recorded_write(false, ActivationOutcome::StorageUnknown).await;

        assert_eq!(result, Err(CredentialIngressError::Unavailable));
        assert_eq!(events, ["admit", "stage", "fence", "activate"]);
    }

    #[test]
    fn trusted_headers_supply_identity_and_must_match_body_selectors() {
        // Pins: the body cannot select a tenant or caller, while the edge-bound
        // connection/slot headers must exactly duplicate its secret-free path selectors.
        let identity = identity();
        let request: ConnectorCredentialIngressRequest =
            serde_json::from_value(request_value(FIXTURE_SECRET)).expect("fixture request");
        let mut headers = HeaderMap::new();
        for (name, value) in [
            (
                "x-moa-identity-type",
                identity.identity_type.as_str().to_string(),
            ),
            ("x-moa-identity-id", identity.id.to_string()),
            ("x-moa-tenant-id", identity.tenant_id.to_string()),
            (
                CONNECTOR_CONNECTION_ID_HEADER,
                request.metadata().connection_id.to_string(),
            ),
            (
                CONNECTOR_CREDENTIAL_SLOT_HEADER,
                request.metadata().slot_name.to_string(),
            ),
        ] {
            headers.insert(name, HeaderValue::from_str(&value).expect("fixture header"));
        }

        assert_eq!(extract_trusted_identity(&headers), Ok(identity));
        assert_eq!(require_selector_match(&headers, request.metadata()), Ok(()));

        headers.insert(
            CONNECTOR_CONNECTION_ID_HEADER,
            HeaderValue::from_static("00000000-0000-0000-0000-000000000000"),
        );
        assert_eq!(
            require_selector_match(&headers, request.metadata()),
            Err(CredentialIngressError::InvalidRequest)
        );
    }
}
