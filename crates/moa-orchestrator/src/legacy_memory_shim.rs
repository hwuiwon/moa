//! Temporary wiki-memory bridge used during the graph cutover.

use async_trait::async_trait;
use moa_core::{
    IngestReport, MemoryPath, MemoryScope, MemorySearchResult, MemoryStore, MoaError, PageSummary,
    PageType, Result, WikiPage,
};

/// No-op implementation for the soon-to-be-deleted wiki memory trait.
///
/// C03 removes concrete wiki-store wiring from the orchestrators while C04/C05 finish
/// deleting the remaining wiki-shaped tool and skill APIs. Read-only enumeration methods return
/// empty data so the context pipeline can continue when no graph-native replacement is wired yet;
/// mutating and path-addressed methods fail if a missed migration calls them.
#[derive(Debug, Default)]
pub struct DeadMemoryStoreShim;

impl DeadMemoryStoreShim {
    fn unavailable() -> MoaError {
        MoaError::Unsupported("wiki memory bridge was invoked during graph cutover".to_string())
    }
}

#[async_trait]
impl MemoryStore for DeadMemoryStoreShim {
    async fn search(
        &self,
        _query: &str,
        _scope: &MemoryScope,
        _limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        Err(MoaError::NotImplemented(
            "wiki memory search is unavailable in graph runtime".to_string(),
        ))
    }

    async fn read_page(&self, _scope: &MemoryScope, _path: &MemoryPath) -> Result<WikiPage> {
        Err(Self::unavailable())
    }

    async fn write_page(
        &self,
        _scope: &MemoryScope,
        _path: &MemoryPath,
        _page: WikiPage,
    ) -> Result<()> {
        Err(Self::unavailable())
    }

    async fn delete_page(&self, _scope: &MemoryScope, _path: &MemoryPath) -> Result<()> {
        Err(Self::unavailable())
    }

    async fn list_pages(
        &self,
        _scope: &MemoryScope,
        _filter: Option<PageType>,
    ) -> Result<Vec<PageSummary>> {
        Ok(Vec::new())
    }

    async fn get_index(&self, _scope: &MemoryScope) -> Result<String> {
        Ok(String::new())
    }

    async fn ingest_source(
        &self,
        _scope: &MemoryScope,
        _source_name: &str,
        _content: &str,
    ) -> Result<IngestReport> {
        Err(Self::unavailable())
    }

    async fn rebuild_search_index(&self, _scope: &MemoryScope) -> Result<()> {
        Err(Self::unavailable())
    }
}
