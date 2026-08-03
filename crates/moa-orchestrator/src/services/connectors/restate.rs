//! Restate service adapter for connector management.

use moa_wire::connectors::{
    ConnectorConnectionCreateRequest, ConnectorConnectionListRequest as WireConnectionListRequest,
    ConnectorConnectionListResponse, ConnectorConnectionMutationCommand,
    ConnectorConnectionMutationRequest, ConnectorConnectionResponse, ConnectorConnectionSelector,
    ConnectorConnectionUseCommand, ConnectorConnectionVerificationResponse,
};

use super::authz::ConnectorManagementAuthorizationError;
use super::definitions::ConnectorDefinitionResolutionError;
use super::management::{
    ConnectorDestinationVerificationError, ConnectorManagementError, ConnectorManagementService,
};

use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;

use crate::handlers::authz_shim::require_identity;

/// Restate surface for secret-free connector connection management.
#[restate_sdk::service]
#[name = "ConnectorConnections"]
pub trait ConnectorConnections {
    /// Creates one pending connection from an exact published definition.
    async fn create(
        request: Json<ConnectorConnectionCreateRequest>,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError>;

    /// Lists connections in the authenticated caller's tenant.
    async fn list(
        request: Json<WireConnectionListRequest>,
    ) -> Result<Json<ConnectorConnectionListResponse>, HandlerError>;

    /// Gets one exact connection.
    async fn get(
        selector: Json<ConnectorConnectionSelector>,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError>;

    /// Verifies local destination admission and credential readiness.
    async fn verify(
        command: Json<ConnectorConnectionMutationCommand>,
    ) -> Result<Json<ConnectorConnectionVerificationResponse>, HandlerError>;

    /// Compiles bindings and activates one connection generation.
    async fn activate(
        command: Json<ConnectorConnectionMutationCommand>,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError>;

    /// Suspends one active connection.
    async fn suspend(
        command: Json<ConnectorConnectionMutationCommand>,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError>;

    /// Resumes one suspended connection.
    async fn resume(
        command: Json<ConnectorConnectionMutationCommand>,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError>;

    /// Fences and disconnects one connection while preserving audit.
    async fn disconnect(
        command: Json<ConnectorConnectionMutationCommand>,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError>;

    /// Deletes one pending-auth or already-disconnecting connection projection.
    async fn delete(
        command: Json<ConnectorConnectionMutationCommand>,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError>;

    /// Grants one direct same-tenant connection-use relationship.
    async fn grant_use(command: Json<ConnectorConnectionUseCommand>) -> Result<(), HandlerError>;

    /// Revokes one direct same-tenant connection-use relationship.
    async fn revoke_use(command: Json<ConnectorConnectionUseCommand>) -> Result<(), HandlerError>;
}

/// Concrete Restate adapter around the independently testable application service.
#[derive(Clone)]
pub struct ConnectorConnectionsImpl {
    service: ConnectorManagementService,
}

#[derive(Clone, Copy)]
enum MutationOperation {
    Activate,
    Suspend,
    Resume,
    Disconnect,
    Delete,
}

impl ConnectorConnectionsImpl {
    /// Creates the Restate adapter.
    #[must_use]
    pub const fn new(service: ConnectorManagementService) -> Self {
        Self { service }
    }
}

impl ConnectorConnections for ConnectorConnectionsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before definition or connection access.
    async fn create(
        &self,
        ctx: Context<'_>,
        request: Json<ConnectorConnectionCreateRequest>,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
        annotate_restate_handler_span("ConnectorConnections", "create");
        let identity = require_identity(&ctx)?;
        let service = self.service.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move {
                service
                    .create(&identity, request)
                    .await
                    .map(Json)
                    .map_err(management_error_to_handler_error)
            })
            .name("connector_connections_create")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before tenant connection reads.
    async fn list(
        &self,
        ctx: Context<'_>,
        request: Json<WireConnectionListRequest>,
    ) -> Result<Json<ConnectorConnectionListResponse>, HandlerError> {
        annotate_restate_handler_span("ConnectorConnections", "list");
        let identity = require_identity(&ctx)?;
        let service = self.service.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move {
                service
                    .list(&identity, request)
                    .await
                    .map(Json)
                    .map_err(management_error_to_handler_error)
            })
            .name("connector_connections_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, selector))]
    // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before connection reads.
    async fn get(
        &self,
        ctx: Context<'_>,
        selector: Json<ConnectorConnectionSelector>,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
        annotate_restate_handler_span("ConnectorConnections", "get");
        let identity = require_identity(&ctx)?;
        let service = self.service.clone();
        let selector = selector.into_inner();
        Ok(ctx
            .run(|| async move {
                service
                    .get(&identity, selector.connection_id)
                    .await
                    .map(Json)
                    .map_err(management_error_to_handler_error)
            })
            .name("connector_connections_get")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, command))]
    // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before connection or credential-metadata reads.
    async fn verify(
        &self,
        ctx: Context<'_>,
        command: Json<ConnectorConnectionMutationCommand>,
    ) -> Result<Json<ConnectorConnectionVerificationResponse>, HandlerError> {
        annotate_restate_handler_span("ConnectorConnections", "verify");
        let identity = require_identity(&ctx)?;
        let service = self.service.clone();
        let command = command.into_inner();
        Ok(ctx
            .run(|| async move {
                service
                    .verify(&identity, command.connection_id, mutation(&command))
                    .await
                    .map(Json)
                    .map_err(management_error_to_handler_error)
            })
            .name("connector_connections_verify")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, command))]
    // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before activation reads or writes.
    async fn activate(
        &self,
        ctx: Context<'_>,
        command: Json<ConnectorConnectionMutationCommand>,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
        annotate_restate_handler_span("ConnectorConnections", "activate");
        mutation_response(
            self.service.clone(),
            ctx,
            command,
            MutationOperation::Activate,
        )
        .await
    }

    #[tracing::instrument(skip(self, ctx, command))]
    // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before lifecycle reads or writes.
    async fn suspend(
        &self,
        ctx: Context<'_>,
        command: Json<ConnectorConnectionMutationCommand>,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
        annotate_restate_handler_span("ConnectorConnections", "suspend");
        mutation_response(
            self.service.clone(),
            ctx,
            command,
            MutationOperation::Suspend,
        )
        .await
    }

    #[tracing::instrument(skip(self, ctx, command))]
    // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before lifecycle reads or writes.
    async fn resume(
        &self,
        ctx: Context<'_>,
        command: Json<ConnectorConnectionMutationCommand>,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
        annotate_restate_handler_span("ConnectorConnections", "resume");
        mutation_response(
            self.service.clone(),
            ctx,
            command,
            MutationOperation::Resume,
        )
        .await
    }

    #[tracing::instrument(skip(self, ctx, command))]
    // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before lifecycle and credential-revocation access.
    async fn disconnect(
        &self,
        ctx: Context<'_>,
        command: Json<ConnectorConnectionMutationCommand>,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
        annotate_restate_handler_span("ConnectorConnections", "disconnect");
        mutation_response(
            self.service.clone(),
            ctx,
            command,
            MutationOperation::Disconnect,
        )
        .await
    }

    #[tracing::instrument(skip(self, ctx, command))]
    // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before deletion reads or writes.
    async fn delete(
        &self,
        ctx: Context<'_>,
        command: Json<ConnectorConnectionMutationCommand>,
    ) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
        annotate_restate_handler_span("ConnectorConnections", "delete");
        mutation_response(
            self.service.clone(),
            ctx,
            command,
            MutationOperation::Delete,
        )
        .await
    }

    #[tracing::instrument(skip(self, ctx, command))]
    // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before direct-use writes.
    async fn grant_use(
        &self,
        ctx: Context<'_>,
        command: Json<ConnectorConnectionUseCommand>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("ConnectorConnections", "grant_use");
        use_response(self.service.clone(), ctx, command, true).await
    }

    #[tracing::instrument(skip(self, ctx, command))]
    // SAFETY: this adapter performs no protected access; the authenticated management service authorizes before direct-use writes.
    async fn revoke_use(
        &self,
        ctx: Context<'_>,
        command: Json<ConnectorConnectionUseCommand>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("ConnectorConnections", "revoke_use");
        use_response(self.service.clone(), ctx, command, false).await
    }
}

async fn mutation_response(
    service: ConnectorManagementService,
    ctx: Context<'_>,
    command: Json<ConnectorConnectionMutationCommand>,
    operation: MutationOperation,
) -> Result<Json<ConnectorConnectionResponse>, HandlerError> {
    let identity = require_identity(&ctx)?;
    let command = command.into_inner();
    Ok(ctx
        .run(|| async move {
            let request = mutation(&command);
            let result = match operation {
                MutationOperation::Activate => {
                    service
                        .activate(&identity, command.connection_id, request)
                        .await
                }
                MutationOperation::Suspend => {
                    service
                        .suspend(&identity, command.connection_id, request)
                        .await
                }
                MutationOperation::Resume => {
                    service
                        .resume(&identity, command.connection_id, request)
                        .await
                }
                MutationOperation::Disconnect => {
                    service
                        .disconnect(&identity, command.connection_id, request)
                        .await
                }
                MutationOperation::Delete => {
                    service
                        .delete(&identity, command.connection_id, request)
                        .await
                }
            };
            result.map(Json).map_err(management_error_to_handler_error)
        })
        .name(match operation {
            MutationOperation::Activate => "connector_connections_activate",
            MutationOperation::Suspend => "connector_connections_suspend",
            MutationOperation::Resume => "connector_connections_resume",
            MutationOperation::Disconnect => "connector_connections_disconnect",
            MutationOperation::Delete => "connector_connections_delete",
        })
        .await?)
}

async fn use_response(
    service: ConnectorManagementService,
    ctx: Context<'_>,
    command: Json<ConnectorConnectionUseCommand>,
    grant: bool,
) -> Result<(), HandlerError> {
    let identity = require_identity(&ctx)?;
    let command = command.into_inner();
    Ok(ctx
        .run(|| async move {
            let result = if grant {
                service
                    .grant_use(&identity, command.connection_id, command.request)
                    .await
            } else {
                service
                    .revoke_use(&identity, command.connection_id, command.request)
                    .await
            };
            result.map_err(management_error_to_handler_error)
        })
        .name(if grant {
            "connector_connections_grant_use"
        } else {
            "connector_connections_revoke_use"
        })
        .await?)
}

const fn mutation(
    command: &ConnectorConnectionMutationCommand,
) -> ConnectorConnectionMutationRequest {
    ConnectorConnectionMutationRequest {
        expected_generation: command.expected_generation,
    }
}

fn management_error_to_handler_error(error: ConnectorManagementError) -> HandlerError {
    let (code, message) = match error {
        ConnectorManagementError::Authorization(ConnectorManagementAuthorizationError::Denied) => {
            (403, "forbidden")
        }
        ConnectorManagementError::Authorization(
            ConnectorManagementAuthorizationError::Unavailable,
        ) => (503, "authorization unavailable"),
        ConnectorManagementError::Definition(ConnectorDefinitionResolutionError::NotFound)
        | ConnectorManagementError::Connector(moa_connectors::Error::ConnectionNotFound {
            ..
        }) => (404, "connector resource not found"),
        ConnectorManagementError::Definition(
            ConnectorDefinitionResolutionError::Unavailable
            | ConnectorDefinitionResolutionError::BuiltInUnavailable,
        )
        | ConnectorManagementError::Destination(
            ConnectorDestinationVerificationError::Unavailable,
        )
        | ConnectorManagementError::CredentialRevocation(_)
        | ConnectorManagementError::Connector(
            moa_connectors::Error::DatabaseScope(_)
            | moa_connectors::Error::Authorization(_)
            | moa_connectors::Error::AuthorizationUnavailable
            | moa_connectors::Error::Storage(_),
        ) => (503, "connector management unavailable"),
        ConnectorManagementError::Connector(
            moa_connectors::Error::GenerationConflict { .. }
            | moa_connectors::Error::InvalidTransition { .. }
            | moa_connectors::Error::InvocationConflict { .. }
            | moa_connectors::Error::InvocationStateConflict { .. },
        ) => (409, "connector state conflict"),
        ConnectorManagementError::Definition(
            ConnectorDefinitionResolutionError::NotPublished
            | ConnectorDefinitionResolutionError::NotInstallable,
        )
        | ConnectorManagementError::Destination(ConnectorDestinationVerificationError::Rejected)
        | ConnectorManagementError::CredentialSlotMismatch
        | ConnectorManagementError::ManagedKnowledgeOperation(_)
        | ConnectorManagementError::Connector(
            moa_connectors::Error::InvalidConnectionOrigin { .. }
            | moa_connectors::Error::InvalidGeneration { .. }
            | moa_connectors::Error::GenerationExhausted
            | moa_connectors::Error::InvalidContract { .. }
            | moa_connectors::Error::CredentialSlotMissing { .. }
            | moa_connectors::Error::UseGrantConnectionUnavailable { .. }
            | moa_connectors::Error::UseGrantSubjectNotFound { .. }
            | moa_connectors::Error::UseGrantSubjectInactive { .. },
        ) => (400, "invalid connector management request"),
        ConnectorManagementError::UnsupportedOwnerIdentity => {
            (403, "connector owner identity is not permitted")
        }
        ConnectorManagementError::DefinitionReferenceMismatch
        | ConnectorManagementError::Connector(_) => (500, "connector management invariant failed"),
    };
    TerminalError::new_with_code(code, message).into()
}
