//! Durable Restate façade over the file-backed MOA memory store.

use std::sync::Arc;

use moa_core::{
    MemoryPath, MemoryScope, MemorySearchMode, MemorySearchResult, MemoryStore as CoreMemoryStore,
    MoaError, PageSummary, PageType, WikiPage,
};
use moa_memory::FileMemoryStore;
use restate_sdk::prelude::*;

use crate::observability::annotate_restate_handler_span;

/// Request payload for `MemoryStore/read_page`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReadPageRequest {
    /// Scope containing the page.
    pub scope: MemoryScope,
    /// Logical wiki path to read.
    pub path: MemoryPath,
}

/// Request payload for `MemoryStore/write_page`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WritePageRequest {
    /// Scope containing the page.
    pub scope: MemoryScope,
    /// Logical wiki path to write.
    pub path: MemoryPath,
    /// Page contents to persist.
    pub page: WikiPage,
}

/// Request payload for `MemoryStore/search_pages`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SearchPagesRequest {
    /// Scope to search within.
    pub scope: MemoryScope,
    /// Search query.
    pub query: String,
    /// Maximum number of results to return.
    pub limit: usize,
    /// Retrieval mode override.
    #[serde(default)]
    pub mode: Option<MemorySearchMode>,
}

/// Request payload for `MemoryStore/list_pages`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListPagesRequest {
    /// Scope to list.
    pub scope: MemoryScope,
    /// Optional page-type filter.
    #[serde(default)]
    pub page_type: Option<PageType>,
}

/// Request payload for `MemoryStore/run_workspace_consolidation`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunWorkspaceConsolidationRequest {
    /// Workspace whose wiki should be consolidated.
    pub workspace_id: moa_core::WorkspaceId,
}

/// Serializable consolidation outcome returned by the memory service.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceConsolidationReport {
    /// Scope that was consolidated.
    pub scope: MemoryScope,
    /// Number of pages rewritten in place.
    pub pages_updated: u64,
    /// Number of pages removed as stale.
    pub pages_deleted: u64,
    /// Number of relative date phrases normalized.
    pub relative_dates_normalized: u64,
    /// Number of contradictory claims rewritten.
    pub contradictions_resolved: u64,
    /// Number of pages whose confidence was decayed.
    pub confidence_decayed: u64,
    /// Paths that have no inbound references after consolidation.
    pub orphaned_pages: Vec<MemoryPath>,
    /// `MEMORY.md` line count before regeneration.
    pub memory_lines_before: u64,
    /// `MEMORY.md` line count after regeneration.
    pub memory_lines_after: u64,
}

impl From<moa_memory::ConsolidationReport> for WorkspaceConsolidationReport {
    fn from(value: moa_memory::ConsolidationReport) -> Self {
        Self {
            scope: value.scope,
            pages_updated: value.pages_updated as u64,
            pages_deleted: value.pages_deleted as u64,
            relative_dates_normalized: value.relative_dates_normalized as u64,
            contradictions_resolved: value.contradictions_resolved as u64,
            confidence_decayed: value.confidence_decayed as u64,
            orphaned_pages: value.orphaned_pages,
            memory_lines_before: value.memory_lines_before as u64,
            memory_lines_after: value.memory_lines_after as u64,
        }
    }
}

/// Restate service surface for durable memory-wiki operations.
#[restate_sdk::service]
pub trait MemoryStore {
    /// Reads one wiki page, returning `None` when the page does not exist.
    async fn read_page(
        request: Json<ReadPageRequest>,
    ) -> Result<Json<Option<WikiPage>>, HandlerError>;

    /// Persists one wiki page.
    async fn write_page(request: Json<WritePageRequest>) -> Result<(), HandlerError>;

    /// Searches wiki pages inside the requested scope.
    async fn search_pages(
        request: Json<SearchPagesRequest>,
    ) -> Result<Json<Vec<MemorySearchResult>>, HandlerError>;

    /// Lists page summaries for one scope.
    async fn list_pages(
        request: Json<ListPagesRequest>,
    ) -> Result<Json<Vec<PageSummary>>, HandlerError>;

    /// Runs the existing heuristic consolidation engine for one workspace scope.
    async fn run_workspace_consolidation(
        request: Json<RunWorkspaceConsolidationRequest>,
    ) -> Result<Json<WorkspaceConsolidationReport>, HandlerError>;
}

/// Concrete Restate service implementation backed by `FileMemoryStore`.
#[derive(Clone)]
pub struct MemoryStoreImpl {
    store: Arc<FileMemoryStore>,
}

impl MemoryStoreImpl {
    /// Creates a new Restate memory-store facade.
    #[must_use]
    pub fn new(store: Arc<FileMemoryStore>) -> Self {
        Self { store }
    }

    async fn read_page_inner(
        &self,
        request: ReadPageRequest,
    ) -> Result<Option<WikiPage>, HandlerError> {
        match self.store.read_page(&request.scope, &request.path).await {
            Ok(page) => Ok(Some(page)),
            Err(MoaError::StorageError(message))
                if message.contains("memory page not found:")
                    || message.contains("memory page not found") =>
            {
                Ok(None)
            }
            Err(error) => Err(to_handler_error(error)),
        }
    }

    async fn write_page_inner(&self, request: WritePageRequest) -> Result<(), HandlerError> {
        self.store
            .write_page(&request.scope, &request.path, request.page)
            .await
            .map_err(to_handler_error)
    }

    async fn search_pages_inner(
        &self,
        request: SearchPagesRequest,
    ) -> Result<Vec<MemorySearchResult>, HandlerError> {
        match request.mode {
            Some(mode) => self
                .store
                .search_with_mode(&request.query, &request.scope, request.limit, mode)
                .await
                .map_err(to_handler_error),
            None => self
                .store
                .search(&request.query, &request.scope, request.limit)
                .await
                .map_err(to_handler_error),
        }
    }

    async fn list_pages_inner(
        &self,
        request: ListPagesRequest,
    ) -> Result<Vec<PageSummary>, HandlerError> {
        self.store
            .list_pages(&request.scope, request.page_type)
            .await
            .map_err(to_handler_error)
    }

    /// Runs one consolidation pass against a workspace scope and returns a serializable report.
    pub async fn run_workspace_consolidation_inner(
        &self,
        workspace_id: moa_core::WorkspaceId,
    ) -> Result<WorkspaceConsolidationReport, HandlerError> {
        let scope = MemoryScope::Workspace { workspace_id };
        self.store
            .run_consolidation(&scope)
            .await
            .map(WorkspaceConsolidationReport::from)
            .map_err(to_handler_error)
    }
}

impl MemoryStore for MemoryStoreImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn read_page(
        &self,
        ctx: Context<'_>,
        request: Json<ReadPageRequest>,
    ) -> Result<Json<Option<WikiPage>>, HandlerError> {
        annotate_restate_handler_span("MemoryStore", "read_page");
        let service = self.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move { service.read_page_inner(request).await.map(Json::from) })
            .name("read_page")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn write_page(
        &self,
        ctx: Context<'_>,
        request: Json<WritePageRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("MemoryStore", "write_page");
        let service = self.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move { service.write_page_inner(request).await })
            .name("write_page")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn search_pages(
        &self,
        ctx: Context<'_>,
        request: Json<SearchPagesRequest>,
    ) -> Result<Json<Vec<MemorySearchResult>>, HandlerError> {
        annotate_restate_handler_span("MemoryStore", "search_pages");
        let service = self.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move { service.search_pages_inner(request).await.map(Json::from) })
            .name("search_pages")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_pages(
        &self,
        ctx: Context<'_>,
        request: Json<ListPagesRequest>,
    ) -> Result<Json<Vec<PageSummary>>, HandlerError> {
        annotate_restate_handler_span("MemoryStore", "list_pages");
        let service = self.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move { service.list_pages_inner(request).await.map(Json::from) })
            .name("list_pages")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn run_workspace_consolidation(
        &self,
        ctx: Context<'_>,
        request: Json<RunWorkspaceConsolidationRequest>,
    ) -> Result<Json<WorkspaceConsolidationReport>, HandlerError> {
        annotate_restate_handler_span("MemoryStore", "run_workspace_consolidation");
        let service = self.clone();
        let request = request.into_inner();
        Ok(ctx
            .run(|| async move {
                service
                    .run_workspace_consolidation_inner(request.workspace_id)
                    .await
                    .map(Json::from)
            })
            .name("run_workspace_consolidation")
            .await?)
    }
}

fn to_handler_error(error: MoaError) -> HandlerError {
    if error.is_fatal() {
        return TerminalError::new(error.to_string()).into();
    }

    HandlerError::from(error)
}
