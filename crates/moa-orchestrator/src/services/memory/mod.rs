//! Restate service for cloud-owned graph-memory search, show, ingest, and debug retrieval.

mod ingest;
mod responses;
mod retrieval;
mod scope;
mod tools;

pub use ingest::document_ingest_session_id;
pub use scope::{
    UserScopeError, checked_ingest_contact_id, checked_memory_scope, effective_user_id,
};
pub use tools::OrchestratorMemoryRetrievalExecutor;

use moa_authz_schema::Relation;
use moa_core::wire::memory::{
    MemoryIngestRequest, MemoryIngestResponse, MemoryRetrieveDebugRequest,
    MemoryRetrieveDebugResponse, MemorySearchRequest, MemorySearchResponse, MemoryShowRequest,
    MemoryShowResponse,
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;

use crate::handlers::authz_shim::authorize_tenant;

use self::ingest::ingest_documents_inner;
use self::retrieval::{retrieve_debug_inner, search_inner, show_inner};

/// Restate service surface for graph-memory operations.
#[restate_sdk::service]
#[name = "Memory"]
pub trait Memory {
    /// Searches graph memory after a tenant operator check.
    async fn search(
        request: Json<MemorySearchRequest>,
    ) -> Result<Json<MemorySearchResponse>, HandlerError>;

    /// Shows one graph-memory node after a tenant operator check.
    async fn show(
        request: Json<MemoryShowRequest>,
    ) -> Result<Json<MemoryShowResponse>, HandlerError>;

    /// Ingests documents into graph memory after a tenant operator check.
    async fn ingest_documents(
        request: Json<MemoryIngestRequest>,
    ) -> Result<Json<MemoryIngestResponse>, HandlerError>;

    /// Runs graph-memory retrieval with debug lineage after a tenant operator check.
    async fn retrieve_debug(
        request: Json<MemoryRetrieveDebugRequest>,
    ) -> Result<Json<MemoryRetrieveDebugResponse>, HandlerError>;
}

/// Concrete memory service implementation.
#[derive(Clone, Default)]
pub struct MemoryImpl;

impl Memory for MemoryImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn search(
        &self,
        ctx: Context<'_>,
        request: Json<MemorySearchRequest>,
    ) -> Result<Json<MemorySearchResponse>, HandlerError> {
        annotate_restate_handler_span("Memory", "search");
        let request = request.into_inner();
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let scope = checked_memory_scope(request.tenant_id, request.contact_id, &identity)
            .map_err(user_scope_handler_error)?;

        Ok(ctx
            .run(|| async move { search_inner(request, scope).await.map(Json::from) })
            .name("memory_search")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn show(
        &self,
        ctx: Context<'_>,
        request: Json<MemoryShowRequest>,
    ) -> Result<Json<MemoryShowResponse>, HandlerError> {
        annotate_restate_handler_span("Memory", "show");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;

        Ok(ctx
            .run(|| async move { show_inner(request).await.map(Json::from) })
            .name("memory_show")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn ingest_documents(
        &self,
        ctx: Context<'_>,
        request: Json<MemoryIngestRequest>,
    ) -> Result<Json<MemoryIngestResponse>, HandlerError> {
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
        annotate_restate_handler_span("Memory", "retrieve_debug");
        let request = request.into_inner();
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let scope = checked_memory_scope(request.tenant_id, request.contact_id, &identity)
            .map_err(user_scope_handler_error)?;

        let response = ctx
            .run(|| async move {
                retrieve_debug_inner(request, scope, &identity)
                    .await
                    .map(Json::from)
            })
            .name("memory_retrieve_debug")
            .await?
            .into_inner();

        Ok(Json(response))
    }
}

fn user_scope_handler_error(error: UserScopeError) -> HandlerError {
    TerminalError::new_with_code(400, error.to_string()).into()
}

fn memory_handler_error(error: impl std::fmt::Display) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}
