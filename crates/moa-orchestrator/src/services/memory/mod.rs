//! Restate service for cloud-owned graph-memory search, show, ingest, and debug inspection.

mod ingest;
mod responses;
mod retrieval;
mod scope;
mod tools;

pub use ingest::document_ingest_session_id;
pub use scope::{UserScopeError, checked_ingest_contact_id, effective_user_id};
pub use tools::OrchestratorMemoryRetrievalExecutor;

use moa_authz_schema::Relation;
use moa_core::config::MoaConfig;
use moa_core::traits::{Identity, SessionStore};
use moa_core::types::identifiers::SessionId;
use moa_core::types::session::SessionMeta;
use moa_core::wire::memory::{
    MemoryIngestRequest, MemoryIngestResponse, MemoryRetrieveDebugRequest,
    MemoryRetrieveDebugResponse, MemorySearchRequest, MemorySearchResponse, MemoryShowRequest,
    MemoryShowResponse,
};
use moa_crypto::KeyManagementProvider;
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use std::sync::Arc;

use crate::handlers::authz_shim::{authorize_session_participant, authorize_tenant};

use self::ingest::ingest_documents_inner;
use self::retrieval::{
    MemoryRequestProvenance, MemoryServiceDeps, retrieve_debug_inner, search_inner, show_inner,
};

/// Restate service surface for graph-memory operations.
#[restate_sdk::service]
#[name = "Memory"]
pub trait Memory {
    /// Searches graph memory under the persisted session participant and policy scope.
    async fn search(
        request: Json<MemorySearchRequest>,
    ) -> Result<Json<MemorySearchResponse>, HandlerError>;

    /// Shows one graph-memory node under the persisted session participant and policy scope.
    async fn show(
        request: Json<MemoryShowRequest>,
    ) -> Result<Json<MemoryShowResponse>, HandlerError>;

    /// Ingests documents into graph memory after a tenant operator check.
    async fn ingest_documents(
        request: Json<MemoryIngestRequest>,
    ) -> Result<Json<MemoryIngestResponse>, HandlerError>;

    /// Runs diagnostic retrieval under the persisted session participant and policy scope.
    async fn retrieve_debug(
        request: Json<MemoryRetrieveDebugRequest>,
    ) -> Result<Json<MemoryRetrieveDebugResponse>, HandlerError>;
}

/// Concrete memory service implementation.
#[derive(Clone)]
pub struct MemoryImpl {
    pool: sqlx::PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    config: Arc<MoaConfig>,
    session_store: Arc<moa_session::PostgresSessionStore>,
}

impl MemoryImpl {
    /// Creates the memory adapter with its graph and retrieval dependencies.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        kms: Arc<dyn KeyManagementProvider>,
        config: Arc<MoaConfig>,
        session_store: Arc<moa_session::PostgresSessionStore>,
    ) -> Self {
        Self {
            pool,
            kms,
            config,
            session_store,
        }
    }
}

impl Memory for MemoryImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn search(
        &self,
        ctx: Context<'_>,
        request: Json<MemorySearchRequest>,
    ) -> Result<Json<MemorySearchResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Memory", "search");
        let request = request.into_inner();
        let identity = authorize_session_participant(&ctx, request.session_id).await?;
        let session = load_session(&ctx, self.session_store.clone(), request.session_id).await?;
        require_session_tenant(&identity, &session)?;
        let operation_id = format!("memory.search:{}", ctx.invocation_id());

        let pool = self.pool.clone();
        let kms = self.kms.clone();
        let config = self.config.clone();
        Ok(ctx
            .run(|| async move {
                search_inner(
                    request,
                    MemoryRequestProvenance {
                        session,
                        identity,
                        operation_id,
                    },
                    MemoryServiceDeps {
                        pool: &pool,
                        kms: &kms,
                    },
                    config.as_ref(),
                )
                .await
                .map(Json::from)
            })
            .name("memory_search")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn show(
        &self,
        ctx: Context<'_>,
        request: Json<MemoryShowRequest>,
    ) -> Result<Json<MemoryShowResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Memory", "show");
        let request = request.into_inner();
        let identity = authorize_session_participant(&ctx, request.session_id).await?;
        let session = load_session(&ctx, self.session_store.clone(), request.session_id).await?;
        require_session_tenant(&identity, &session)?;
        let operation_id = format!("memory.show:{}", ctx.invocation_id());

        let pool = self.pool.clone();
        let kms = self.kms.clone();
        Ok(ctx
            .run(|| async move {
                show_inner(
                    request,
                    MemoryRequestProvenance {
                        session,
                        identity,
                        operation_id,
                    },
                    MemoryServiceDeps {
                        pool: &pool,
                        kms: &kms,
                    },
                )
                .await
                .map(Json::from)
            })
            .name("memory_show")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn ingest_documents(
        &self,
        ctx: Context<'_>,
        request: Json<MemoryIngestRequest>,
    ) -> Result<Json<MemoryIngestResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Memory", "ingest_documents");
        let request = request.into_inner();
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let contact_id = checked_ingest_contact_id(request.contact_id, &identity)
            .map_err(user_scope_handler_error)?;

        let response = ingest_documents_inner(&ctx, request, contact_id).await?;

        Ok(Json(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn retrieve_debug(
        &self,
        ctx: Context<'_>,
        request: Json<MemoryRetrieveDebugRequest>,
    ) -> Result<Json<MemoryRetrieveDebugResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Memory", "retrieve_debug");
        let request = request.into_inner();
        let identity = authorize_session_participant(&ctx, request.session_id).await?;
        let session = load_session(&ctx, self.session_store.clone(), request.session_id).await?;
        require_session_tenant(&identity, &session)?;
        let operation_id = format!("memory.retrieve_debug:{}", ctx.invocation_id());

        let pool = self.pool.clone();
        let kms = self.kms.clone();
        let config = self.config.clone();
        let response = ctx
            .run(|| async move {
                retrieve_debug_inner(
                    request,
                    MemoryRequestProvenance {
                        session,
                        identity,
                        operation_id,
                    },
                    MemoryServiceDeps {
                        pool: &pool,
                        kms: &kms,
                    },
                    config.as_ref(),
                )
                .await
                .map(Json::from)
            })
            .name("memory_retrieve_debug")
            .await?
            .into_inner();

        Ok(Json(response))
    }
}

async fn load_session(
    ctx: &Context<'_>,
    session_store: Arc<moa_session::PostgresSessionStore>,
    session_id: SessionId,
) -> Result<SessionMeta, HandlerError> {
    Ok(ctx
        .run(|| async move {
            session_store
                .get_session(session_id)
                .await
                .map(Json::from)
                .map_err(memory_handler_error)
        })
        .name("memory_load_pinned_session")
        .await?
        .into_inner())
}

fn require_session_tenant(identity: &Identity, session: &SessionMeta) -> Result<(), HandlerError> {
    if session.tenant_id != identity.tenant_id {
        return Err(TerminalError::new_with_code(404, "session not found").into());
    }
    Ok(())
}

fn user_scope_handler_error(error: UserScopeError) -> HandlerError {
    TerminalError::new_with_code(400, error.to_string()).into()
}

fn memory_handler_error(error: impl std::fmt::Display) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}
