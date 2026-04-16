//! Wiki page search stub until the Postgres tsvector index lands.

use moa_core::{MemoryPath, MemoryScope, MemorySearchResult, MoaError, Result, WikiPage};

/// Search index facade for wiki pages.
#[derive(Clone, Default)]
pub struct WikiSearchIndex;

impl WikiSearchIndex {
    /// Creates a new wiki search index handle.
    pub fn new() -> Self {
        Self
    }

    /// Searches wiki content within a memory scope.
    pub async fn search(
        &self,
        _query: &str,
        _scope: &MemoryScope,
        _limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        Err(MoaError::NotImplemented(
            "wiki search requires the Postgres tsvector index — see step 90".to_string(),
        ))
    }

    /// Upserts one wiki page into the search index.
    pub async fn upsert_page(
        &self,
        _scope: &MemoryScope,
        _path: &MemoryPath,
        _page: &WikiPage,
    ) -> Result<()> {
        Ok(())
    }

    /// Removes one wiki page from the search index.
    pub async fn delete_page(&self, _scope: &MemoryScope, _path: &MemoryPath) -> Result<()> {
        Ok(())
    }

    /// Rebuilds the search index for one scope.
    pub async fn rebuild_scope(
        &self,
        _scope: &MemoryScope,
        _pages: &[(MemoryPath, WikiPage)],
    ) -> Result<()> {
        Ok(())
    }
}
