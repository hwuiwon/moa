//! Secret-isolated connector action runtime boundary.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use jsonschema::{Draft, Retrieve, Uri};
use moa_artifacts::connector::{RuntimeConnectorKindV1, RuntimeOperationBindingV1};
use moa_core::traits::Identity;
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId, ToolCallId};
use moa_core::types::security::ToolOutputAssessment;
use serde::{Deserialize, Serialize};
use serde_canonical_json::CanonicalFormatter;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::catalog::InstalledConnectorAction;
use crate::domain::{
    ConnectionDefinitionRef, ConnectionGeneration, ConnectionStatus, ConnectorConnection,
    ConnectorInvocationId, ConnectorInvocationTerminal, InstalledActionBinding,
    InstalledActionBindingId, OperationContractHash,
};
use crate::repository::{
    ConnectionRepository, InvocationReservation, InvocationReservationRequest,
};
use crate::{Error, Result};

const REQUEST_HASH_DOMAIN: &str = "moa.connector.invocation.v1";

/// Durable, secret-free identity of one installed connector action.
///
/// Tool routing persists this typed provenance directly. A generated
/// `conn__...` tool name is only a model-facing lookup key and is never parsed
/// to reconstruct execution authority.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledConnectorActionPin {
    /// Exact tenant installation selected from the catalog.
    pub connection_id: ConnectorConnectionId,
    /// Expected connection generation selected from the catalog.
    pub connection_generation: ConnectionGeneration,
    /// Expected immutable connector definition revision.
    pub definition: ConnectionDefinitionRef,
    /// Exact compiled binding selected from the catalog.
    pub binding_id: InstalledActionBindingId,
    /// Canonical logical action identifier within the definition.
    pub action_id: String,
    /// Expected normalized governed contract hash.
    pub contract_hash: OperationContractHash,
    /// Expected governed contract revision.
    pub governed_contract_revision: String,
}

impl From<&InstalledConnectorAction> for InstalledConnectorActionPin {
    fn from(action: &InstalledConnectorAction) -> Self {
        Self {
            connection_id: action.connection_id(),
            connection_generation: action.binding().connection_generation,
            definition: action.definition().clone(),
            binding_id: action.binding().binding_id,
            action_id: action.binding().action_id.clone(),
            contract_hash: action.binding().contract_hash,
            governed_contract_revision: action.binding().governed_contract_revision.clone(),
        }
    }
}

/// Fully pinned request for one connector action runtime invocation.
///
/// This request contains no credential, header, or caller-controlled origin.
/// Delegated `connector_connection#use` authorization is a mandatory caller
/// prerequisite. The runtime must then re-read the connection and binding,
/// compare every expected pin, admit destination policy, and only then resolve
/// credential material.
#[derive(Clone)]
pub struct ConnectorActionInvocation {
    /// Authenticated principal whose tenant and delegated use govern the call.
    pub caller: Identity,
    /// Durable tool-call identity used by invocation replay.
    pub tool_call_id: ToolCallId,
    /// Typed connection, definition, generation, binding, and policy pins.
    pub action: InstalledConnectorActionPin,
    /// Schema-validated model input; it never contains transport configuration.
    pub input: Value,
    /// Required cooperative cancellation context for pre-send and in-flight work.
    pub cancellation_token: CancellationToken,
}

impl ConnectorActionInvocation {
    /// Returns the tenant derived exclusively from the authenticated caller.
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.caller.tenant_id
    }
}

impl fmt::Debug for ConnectorActionInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectorActionInvocation")
            .field("caller", &self.caller)
            .field("tool_call_id", &self.tool_call_id)
            .field("action", &self.action)
            .field("input", &"<connector action input>")
            .field("cancelled", &self.cancellation_token.is_cancelled())
            .finish()
    }
}

/// Repository-backed authority and replay coordinator shared by connector transports.
///
/// The coordinator is the only public path that can mint the opaque authorized,
/// reserved, and transmitting carriers below. Runtime dispatchers authorize one
/// invocation once, inspect its typed compiled transport, and then move that
/// same authority into the selected transport without reconstructing authority
/// from a model-facing tool name.
#[derive(Clone)]
pub struct ConnectorInvocationCoordinator {
    catalog: Arc<dyn crate::catalog::InstalledConnectorCatalog>,
    repository: Arc<dyn ConnectionRepository>,
}

impl ConnectorInvocationCoordinator {
    /// Composes the governed installed-action catalog with its durable ledger.
    #[must_use]
    pub fn new(
        catalog: Arc<dyn crate::catalog::InstalledConnectorCatalog>,
        repository: Arc<dyn ConnectionRepository>,
    ) -> Self {
        Self {
            catalog,
            repository,
        }
    }

    /// Authorizes one exact installed action and reloads all durable pins.
    ///
    /// No connection or binding is read before the governed catalog has
    /// required delegated `connector_connection#use` authorization.
    pub async fn authorize(
        &self,
        invocation: ConnectorActionInvocation,
    ) -> Result<AuthorizedConnectorInvocation> {
        let snapshot = self
            .catalog
            .snapshot(crate::catalog::InstalledConnectorCatalogQuery::new(
                invocation.caller.clone(),
                [invocation.action.connection_id],
            ))
            .await?;
        let catalog_action = snapshot
            .actions()
            .iter()
            .find(|candidate| {
                candidate.connection_id() == invocation.action.connection_id
                    && candidate.binding().binding_id == invocation.action.binding_id
                    && candidate.binding().action_id == invocation.action.action_id
            })
            .ok_or(Error::ActionPinMismatch { field: "catalog" })?;
        validate_catalog_pin(&invocation.action, catalog_action)?;

        let (connection, binding) = self.reload_pinned_state(&invocation).await?;
        validate_connector_schema(
            &binding.compiled_contract.operation.policy().input_schema,
            &invocation.input,
            "input",
        )?;
        Ok(AuthorizedConnectorInvocation {
            invocation,
            connection,
            binding,
        })
    }

    /// Reloads the exact connection and binding after secret resolution.
    ///
    /// The reserved carrier remains the only authority for the pending call;
    /// this method does not reserve again or create a second send opportunity.
    pub async fn recheck(&self, reserved: &ReservedConnectorInvocation) -> Result<()> {
        self.reload_pinned_state(&reserved.authorized.invocation)
            .await
            .map(|_| ())
    }

    /// Reloads exact durable state for a fresh authorized protocol leg.
    ///
    /// This is used after that leg resolves credential material. It does not
    /// create ledger authority; callers discard the auxiliary authorized
    /// carrier after the recheck and use only the original transmitting state.
    pub async fn recheck_authorized(
        &self,
        authorized: &AuthorizedConnectorInvocation,
    ) -> Result<()> {
        self.reload_pinned_state(&authorized.invocation)
            .await
            .map(|_| ())
    }

    /// Returns the canonical replay hash for an authorized connector request.
    pub fn request_hash(
        authorized: &AuthorizedConnectorInvocation,
    ) -> Result<OperationContractHash> {
        #[derive(Serialize)]
        struct HashInput<'a> {
            schema_version: u32,
            tenant_id: TenantId,
            tool_call_id: String,
            action: &'a InstalledConnectorActionPin,
            input: &'a Value,
        }
        let invocation = &authorized.invocation;
        let payload = HashInput {
            schema_version: 1,
            tenant_id: invocation.tenant_id(),
            tool_call_id: invocation.tool_call_id.to_string(),
            action: &invocation.action,
            input: &invocation.input,
        };
        let mut serializer =
            serde_json::Serializer::with_formatter(Vec::new(), CanonicalFormatter::new());
        payload.serialize(&mut serializer)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(REQUEST_HASH_DOMAIN.as_bytes());
        hasher.update(&[0]);
        hasher.update(&serializer.into_inner());
        Ok(OperationContractHash::from_bytes(
            *hasher.finalize().as_bytes(),
        ))
    }

    /// Reserves the replay key after the selected transport is fully prepared.
    ///
    /// A replay or in-flight record is returned as a closed error and never as
    /// a carrier that could reach the transmission transition.
    pub async fn reserve(
        &self,
        authorized: AuthorizedConnectorInvocation,
        request_hash: OperationContractHash,
        upstream_idempotency_key: Option<String>,
    ) -> Result<ReservedConnectorInvocation> {
        let request = InvocationReservationRequest {
            invocation_id: ConnectorInvocationId(uuid::Uuid::now_v7()),
            tenant_id: authorized.invocation.tenant_id(),
            connection_id: authorized.invocation.action.connection_id,
            binding_id: authorized.invocation.action.binding_id,
            connection_generation: authorized.invocation.action.connection_generation,
            tool_call_id: authorized.invocation.tool_call_id.to_string(),
            request_hash,
            upstream_idempotency_key,
        };
        match self.repository.reserve_invocation(request.clone()).await? {
            InvocationReservation::Reserved(record) => {
                validate_reserved_record(&request, &record)?;
                Ok(ReservedConnectorInvocation {
                    authorized,
                    record,
                    repository: Arc::clone(&self.repository),
                })
            }
            InvocationReservation::Replay(record) | InvocationReservation::InFlight(record) => {
                Err(Error::InvocationUnavailable {
                    state: record.state,
                })
            }
        }
    }

    async fn reload_pinned_state(
        &self,
        invocation: &ConnectorActionInvocation,
    ) -> Result<(ConnectorConnection, InstalledActionBinding)> {
        let connection = self
            .repository
            .load(invocation.tenant_id(), invocation.action.connection_id)
            .await?
            .ok_or(Error::ActionPinMismatch {
                field: "connection",
            })?;
        let binding = self
            .repository
            .load_binding(
                invocation.tenant_id(),
                invocation.action.connection_id,
                invocation.action.binding_id,
            )
            .await?
            .ok_or(Error::ActionPinMismatch { field: "binding" })?;
        validate_pinned_state(&invocation.action, &connection, &binding)?;
        Ok((connection, binding))
    }
}

/// One delegated-use-authorized, exact-generation connector invocation.
///
/// Fields and construction are private so callers can inspect typed dispatch
/// data but cannot mint execution authority.
pub struct AuthorizedConnectorInvocation {
    invocation: ConnectorActionInvocation,
    connection: ConnectorConnection,
    binding: InstalledActionBinding,
}

impl AuthorizedConnectorInvocation {
    /// Returns the original fully pinned invocation request.
    #[must_use]
    pub const fn invocation(&self) -> &ConnectorActionInvocation {
        &self.invocation
    }

    /// Returns the authorized, generation-pinned connection.
    #[must_use]
    pub const fn connection(&self) -> &ConnectorConnection {
        &self.connection
    }

    /// Returns the authorized compiled binding.
    #[must_use]
    pub const fn binding(&self) -> &InstalledActionBinding {
        &self.binding
    }

    /// Returns the typed compiled runtime selected by the governed catalog.
    #[must_use]
    pub const fn runtime(&self) -> &RuntimeConnectorKindV1 {
        &self.binding.compiled_contract.runtime
    }

    /// Returns the typed compiled operation selected by the governed catalog.
    #[must_use]
    pub const fn operation(&self) -> &RuntimeOperationBindingV1 {
        &self.binding.compiled_contract.operation
    }
}

impl fmt::Debug for AuthorizedConnectorInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthorizedConnectorInvocation")
            .field("invocation", &self.invocation)
            .field("connection_id", &self.connection.connection_id)
            .field("binding_id", &self.binding.binding_id)
            .finish_non_exhaustive()
    }
}

/// One newly reserved connector invocation that has not crossed the send fence.
///
/// This carrier can move only to `failed_before_send` or consume itself into a
/// [`TransmittingConnectorInvocation`]. It cannot be cloned or constructed by
/// runtime callers.
pub struct ReservedConnectorInvocation {
    authorized: AuthorizedConnectorInvocation,
    record: crate::domain::ConnectorInvocationRecord,
    repository: Arc<dyn ConnectionRepository>,
}

impl ReservedConnectorInvocation {
    /// Returns the underlying authorized invocation for final state rechecks.
    #[must_use]
    pub const fn authorized(&self) -> &AuthorizedConnectorInvocation {
        &self.authorized
    }

    /// Records a known failure before transmission and consumes send authority.
    pub async fn fail_before_send<T>(self, error: Error, code: &'static str) -> Result<T> {
        self.repository
            .finish_invocation(
                self.authorized.invocation.tenant_id(),
                self.record.invocation_id,
                ConnectorInvocationTerminal::FailedBeforeSend {
                    error_metadata: serde_json::json!({"code": code}),
                },
            )
            .await?;
        Err(error)
    }

    /// Atomically crosses the one-way transmission fence.
    pub async fn mark_transmitting(self) -> Result<TransmittingConnectorInvocation> {
        let marked = self
            .repository
            .mark_transmitting(
                self.authorized.invocation.tenant_id(),
                self.record.invocation_id,
            )
            .await;
        if let Err(error) = marked {
            return self
                .fail_before_send(error, "transmission_fence_failed")
                .await;
        }
        Ok(TransmittingConnectorInvocation {
            authorized: self.authorized,
            record: self.record,
            repository: self.repository,
        })
    }
}

impl fmt::Debug for ReservedConnectorInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReservedConnectorInvocation")
            .field("authorized", &self.authorized)
            .finish_non_exhaustive()
    }
}

/// One invocation that may have reached the upstream transport.
///
/// This carrier can no longer transition to `failed_before_send`. It can finish
/// as a known failure or unknown outcome, or produce the raw result and private
/// completion ticket consumed by the output-security boundary.
pub struct TransmittingConnectorInvocation {
    authorized: AuthorizedConnectorInvocation,
    record: crate::domain::ConnectorInvocationRecord,
    repository: Arc<dyn ConnectionRepository>,
}

impl TransmittingConnectorInvocation {
    /// Validates untrusted upstream output against the authorized governed schema.
    pub fn validate_output(&self, output: &Value) -> Result<()> {
        validate_connector_schema(
            &self
                .authorized
                .binding
                .compiled_contract
                .operation
                .policy()
                .output_schema,
            output,
            "output",
        )
    }

    /// Records a known post-response rejection and consumes completion authority.
    pub async fn fail<T>(self, error: Error, code: &'static str) -> Result<T> {
        self.finish_error(error, code, false).await
    }

    /// Records an unsafe-to-retry transport outcome and consumes completion authority.
    pub async fn unknown<T>(self, error: Error, code: &'static str) -> Result<T> {
        self.finish_error(error, code, true).await
    }

    /// Produces unclassified output while leaving the ledger transmitting until
    /// the hands output-security boundary journals and finalizes the result.
    #[must_use]
    pub fn succeed_raw(self, output: Value) -> RawConnectorActionResult {
        let invocation = &self.authorized.invocation;
        RawConnectorActionResult::new(
            output,
            ConnectorInvocationCompletionTicket::new(
                invocation.tenant_id(),
                self.record.invocation_id,
                invocation.action.connection_id,
                invocation.action.binding_id,
                invocation.action.connection_generation,
                invocation.tool_call_id,
                self.record.request_hash,
            ),
        )
    }

    async fn finish_error<T>(self, error: Error, code: &'static str, unknown: bool) -> Result<T> {
        let terminal = if unknown {
            ConnectorInvocationTerminal::UnknownOutcome {
                error_metadata: serde_json::json!({"code": code}),
            }
        } else {
            ConnectorInvocationTerminal::Failed {
                error_metadata: serde_json::json!({"code": code}),
            }
        };
        self.repository
            .finish_invocation(
                self.authorized.invocation.tenant_id(),
                self.record.invocation_id,
                terminal,
            )
            .await?;
        Err(error)
    }
}

impl fmt::Debug for TransmittingConnectorInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TransmittingConnectorInvocation")
            .field("authorized", &self.authorized)
            .finish_non_exhaustive()
    }
}

fn validate_catalog_pin(
    expected: &InstalledConnectorActionPin,
    actual: &InstalledConnectorAction,
) -> Result<()> {
    if actual.definition() != &expected.definition {
        return Err(Error::ActionPinMismatch {
            field: "definition",
        });
    }
    validate_binding_pin(expected, actual.binding())
}

fn validate_pinned_state(
    expected: &InstalledConnectorActionPin,
    connection: &ConnectorConnection,
    binding: &InstalledActionBinding,
) -> Result<()> {
    if connection.status != ConnectionStatus::Active {
        return Err(Error::ActionPinMismatch {
            field: "connection_status",
        });
    }
    if connection.connection_id != expected.connection_id {
        return Err(Error::ActionPinMismatch {
            field: "connection_id",
        });
    }
    if connection.generation != expected.connection_generation {
        return Err(Error::ActionPinMismatch {
            field: "connection_generation",
        });
    }
    if connection.definition != expected.definition {
        return Err(Error::ActionPinMismatch {
            field: "definition",
        });
    }
    if binding.tenant_id != connection.tenant_id
        || binding.connection_id != connection.connection_id
    {
        return Err(Error::ActionPinMismatch {
            field: "binding_owner",
        });
    }
    if !binding.enabled {
        return Err(Error::ActionPinMismatch {
            field: "binding_enabled",
        });
    }
    binding.validate()?;
    validate_binding_pin(expected, binding)
}

fn validate_binding_pin(
    expected: &InstalledConnectorActionPin,
    binding: &InstalledActionBinding,
) -> Result<()> {
    if binding.binding_id != expected.binding_id {
        return Err(Error::ActionPinMismatch {
            field: "binding_id",
        });
    }
    if binding.connection_generation != expected.connection_generation {
        return Err(Error::ActionPinMismatch {
            field: "binding_generation",
        });
    }
    if binding.action_id != expected.action_id {
        return Err(Error::ActionPinMismatch { field: "action_id" });
    }
    if binding.contract_hash != expected.contract_hash {
        return Err(Error::ActionPinMismatch {
            field: "contract_hash",
        });
    }
    if binding.governed_contract_revision != expected.governed_contract_revision {
        return Err(Error::ActionPinMismatch {
            field: "governed_contract_revision",
        });
    }
    Ok(())
}

fn validate_reserved_record(
    request: &InvocationReservationRequest,
    record: &crate::domain::ConnectorInvocationRecord,
) -> Result<()> {
    if record.invocation_id != request.invocation_id
        || record.tenant_id != request.tenant_id
        || record.connection_id != request.connection_id
        || record.binding_id != request.binding_id
        || record.connection_generation != request.connection_generation
        || record.tool_call_id != request.tool_call_id
        || record.request_hash != request.request_hash
        || record.upstream_idempotency_key != request.upstream_idempotency_key
        || record.state != crate::domain::ConnectorInvocationState::Reserved
    {
        return Err(Error::CatalogInvariant {
            message: "invocation repository returned a mismatched reservation".to_string(),
        });
    }
    Ok(())
}

fn validate_connector_schema(
    schema: &Value,
    instance: &Value,
    direction: &'static str,
) -> Result<()> {
    let validator = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_retriever(RejectExternalSchemaRetrieval)
        .build(schema)
        .map_err(|_| Error::SchemaValidation { direction })?;
    if validator.is_valid(instance) {
        Ok(())
    } else {
        Err(Error::SchemaValidation { direction })
    }
}

struct RejectExternalSchemaRetrieval;

impl Retrieve for RejectExternalSchemaRetrieval {
    fn retrieve(
        &self,
        _uri: &Uri<String>,
    ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err(Box::new(ExternalSchemaRetrievalDisabled))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("external JSON Schema retrieval is disabled")]
struct ExternalSchemaRetrievalDisabled;

/// Untrusted connector response before `moa-hands` output security.
///
/// The carrier contains only the extracted upstream JSON body. Connector
/// runtimes must never attach request headers, credential material, admitted
/// addresses, or raw transport diagnostics. The value may itself contain
/// hostile or restricted upstream content and therefore must be classified by
/// the existing hands output-security boundary before persistence or logging.
pub struct RawConnectorActionResult {
    output: Value,
    completion: ConnectorInvocationCompletionTicket,
}

impl RawConnectorActionResult {
    /// Creates a raw result whose invocation remains transmitting until hands
    /// persists secured output metadata.
    pub(crate) fn new(output: Value, completion: ConnectorInvocationCompletionTicket) -> Self {
        Self { output, completion }
    }

    /// Borrows the unclassified response JSON.
    #[must_use]
    pub const fn output(&self) -> &Value {
        &self.output
    }

    /// Consumes the carrier into unclassified output and its one-shot durable
    /// secret-free completion ticket.
    ///
    /// The caller must classify the output through the hands security circuit,
    /// persist only secured metadata, and then call
    /// [`ConnectorInvocationCompletionService::finalize_succeeded`] only after
    /// the surrounding durable runtime has journaled that secured result.
    /// Abandoning the ticket leaves the invocation in `transmitting`, which
    /// recovery treats as an unknown outcome and never retransmits automatically.
    #[must_use]
    pub fn into_parts(self) -> (Value, ConnectorInvocationCompletionTicket) {
        (self.output, self.completion)
    }
}

impl fmt::Debug for RawConnectorActionResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawConnectorActionResult")
            .field("output", &"<unclassified connector output>")
            .field("completion", &self.completion)
            .finish()
    }
}

/// Secret-free proof metadata produced only after hands output security runs.
///
/// This deliberately cannot carry the connector response. Persisting upstream
/// JSON before classification would bypass the single raw-output security
/// circuit. T5 constructs this value only after the secured output has reached
/// its durable owner.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecuredConnectorOutputMetadata {
    /// Typed detector result applied to the raw connector response.
    pub assessment: ToolOutputAssessment,
    /// Serialized byte count of the secured, post-classification output.
    pub secured_output_bytes: u64,
}

/// Stable reason an already-transmitting connector invocation could not finish
/// secured-output persistence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorUnknownOutcomeReason {
    /// The caller stopped before secured output persistence completed.
    SecuredOutputNotPersisted,
    /// Output-security processing was cancelled after the upstream responded.
    OutputSecurityCancelled,
    /// Output-security processing failed after the upstream responded.
    OutputSecurityFailed,
}

impl ConnectorUnknownOutcomeReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SecuredOutputNotPersisted => "secured_output_not_persisted",
            Self::OutputSecurityCancelled => "output_security_cancelled",
            Self::OutputSecurityFailed => "output_security_failed",
        }
    }
}

/// Secret-free durable identity used to finalize one transmitting invocation.
///
/// The raw response and credential material are absent. Private fields prevent
/// callers from constructing completion authority, while serialization lets a
/// Restate journal carry the ticket only after raw output was classified inside
/// the journaled closure. `Debug` deliberately reveals no invocation identity.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorInvocationCompletionTicket {
    schema_version: u32,
    tenant_id: TenantId,
    invocation_id: ConnectorInvocationId,
    connection_id: ConnectorConnectionId,
    binding_id: InstalledActionBindingId,
    connection_generation: ConnectionGeneration,
    tool_call_id: ToolCallId,
    request_hash: OperationContractHash,
}

impl ConnectorInvocationCompletionTicket {
    pub(crate) fn new(
        tenant_id: TenantId,
        invocation_id: ConnectorInvocationId,
        connection_id: ConnectorConnectionId,
        binding_id: InstalledActionBindingId,
        connection_generation: ConnectionGeneration,
        tool_call_id: ToolCallId,
        request_hash: OperationContractHash,
    ) -> Self {
        Self {
            schema_version: 1,
            tenant_id,
            invocation_id,
            connection_id,
            binding_id,
            connection_generation,
            tool_call_id,
            request_hash,
        }
    }
}

impl fmt::Debug for ConnectorInvocationCompletionTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConnectorInvocationCompletionTicket(<redacted>)")
    }
}

/// Repository-backed post-journal completion boundary for connector output.
#[derive(Clone)]
pub struct ConnectorInvocationCompletionService {
    repository: std::sync::Arc<dyn ConnectionRepository>,
}

impl ConnectorInvocationCompletionService {
    /// Creates the completion boundary over the authoritative invocation ledger.
    #[must_use]
    pub fn new(repository: std::sync::Arc<dyn ConnectionRepository>) -> Self {
        Self { repository }
    }

    /// Replay-safely marks an invocation successful after the surrounding
    /// durable runtime journaled classified output and secured metadata.
    pub async fn finalize_succeeded(
        &self,
        ticket: &ConnectorInvocationCompletionTicket,
        metadata: SecuredConnectorOutputMetadata,
    ) -> Result<()> {
        self.validate_ticket(ticket).await?;
        let output_metadata = serde_json::to_value(metadata)?;
        self.repository
            .finish_invocation(
                ticket.tenant_id,
                ticket.invocation_id,
                ConnectorInvocationTerminal::Succeeded { output_metadata },
            )
            .await
            .map(|_| ())
    }

    /// Resolves the invocation conservatively when output security cannot
    /// durably finish after an upstream response was received.
    pub async fn finalize_unknown(
        &self,
        ticket: &ConnectorInvocationCompletionTicket,
        reason: ConnectorUnknownOutcomeReason,
    ) -> Result<()> {
        self.validate_ticket(ticket).await?;
        self.repository
            .finish_invocation(
                ticket.tenant_id,
                ticket.invocation_id,
                ConnectorInvocationTerminal::UnknownOutcome {
                    error_metadata: serde_json::json!({"code": reason.as_str()}),
                },
            )
            .await
            .map(|_| ())
    }

    async fn validate_ticket(&self, ticket: &ConnectorInvocationCompletionTicket) -> Result<()> {
        if ticket.schema_version != 1 {
            return Err(crate::Error::ActionPinMismatch {
                field: "completion_ticket_schema_version",
            });
        }
        let record = self
            .repository
            .load_invocation(ticket.tenant_id, ticket.invocation_id)
            .await?
            .ok_or(crate::Error::ActionPinMismatch {
                field: "completion_ticket_invocation",
            })?;
        if record.connection_id != ticket.connection_id {
            return Err(crate::Error::ActionPinMismatch {
                field: "completion_ticket_connection",
            });
        }
        if record.binding_id != ticket.binding_id {
            return Err(crate::Error::ActionPinMismatch {
                field: "completion_ticket_binding",
            });
        }
        if record.connection_generation != ticket.connection_generation {
            return Err(crate::Error::ActionPinMismatch {
                field: "completion_ticket_generation",
            });
        }
        if record.tool_call_id != ticket.tool_call_id.to_string() {
            return Err(crate::Error::ActionPinMismatch {
                field: "completion_ticket_tool_call",
            });
        }
        if record.request_hash != ticket.request_hash {
            return Err(crate::Error::ActionPinMismatch {
                field: "completion_ticket_request_hash",
            });
        }
        Ok(())
    }
}

/// Runtime port for executing a revision- and generation-pinned connector action.
#[async_trait]
pub trait ConnectorActionRuntime: Send + Sync {
    /// Executes one connector action and returns only the unclassified response
    /// carrier that `moa-hands` must immediately send through output security.
    async fn invoke(
        &self,
        invocation: ConnectorActionInvocation,
    ) -> Result<RawConnectorActionResult>;
}
