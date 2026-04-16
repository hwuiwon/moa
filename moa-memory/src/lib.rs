//! File-backed wiki memory store with a Postgres-backed search stub.

use std::env;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    BrainId, MemoryPath, MemoryScope, MemorySearchResult, MemoryStore, MoaConfig, MoaError,
    PageSummary, PageType, Result, SessionStore, WikiPage,
};
use tokio::fs;

pub mod bootstrap;
pub mod branching;
pub mod consolidation;
pub mod index;
pub mod ingest;
pub mod search;
pub mod wiki;

pub use branching::{ChangeOperation, ReconcileReport};
pub use consolidation::ConsolidationReport;
use index::{
    INDEX_FILENAME, LogEntry, append_log_entry, compile_index, load_index_file, load_log_file,
};
pub use moa_core::IngestReport;
use search::WikiSearchIndex;
use wiki::{parse_markdown, render_markdown};

/// File-wiki memory store rooted at a local `.moa` data directory.
#[derive(Clone)]
pub struct FileMemoryStore {
    base_dir: Arc<PathBuf>,
    search_index: WikiSearchIndex,
}

impl FileMemoryStore {
    /// Creates a file-backed memory store rooted at the provided MOA data directory.
    pub async fn new(base_dir: impl AsRef<Path>) -> Result<Self> {
        let base_dir = base_dir.as_ref().to_path_buf();
        fs::create_dir_all(base_dir.join("memory")).await?;
        fs::create_dir_all(base_dir.join("workspaces")).await?;

        Ok(Self {
            base_dir: Arc::new(base_dir),
            search_index: WikiSearchIndex::new(),
        })
    }

    /// Creates a file-backed memory store from the local memory config.
    pub async fn from_config(config: &MoaConfig) -> Result<Self> {
        let configured_memory_dir = if config.cloud.enabled {
            config
                .cloud
                .memory_dir
                .as_deref()
                .unwrap_or(&config.local.memory_dir)
        } else {
            &config.local.memory_dir
        };
        let memory_dir = expand_local_path(configured_memory_dir)?;
        let base_dir = memory_dir.parent().map(Path::to_path_buf).ok_or_else(|| {
            MoaError::ConfigError("local.memory_dir must have a parent".to_string())
        })?;
        Self::new(base_dir).await
    }

    /// Returns the local filesystem root backing the memory store.
    pub fn base_dir(&self) -> &Path {
        self.base_dir.as_ref()
    }

    /// Appends a markdown entry to the scope-local `_log.md` file and refreshes its search row.
    pub async fn append_scope_log(&self, scope: &MemoryScope, entry: LogEntry) -> Result<()> {
        let scope_root = self.scope_root(scope);
        append_log_entry(&scope_root, &entry).await?;
        let log_path: MemoryPath = "_log.md".into();
        let log_page = self.read_page(scope.clone(), &log_path).await?;
        self.search_index
            .upsert_page(scope, &log_path, &log_page)
            .await?;
        Ok(())
    }

    /// Loads the raw append-only `_log.md` contents for a scope.
    pub async fn load_scope_log(&self, scope: &MemoryScope) -> Result<String> {
        load_log_file(&self.scope_root(scope)).await
    }

    /// Regenerates `MEMORY.md` from the current page summaries in a scope.
    pub async fn refresh_scope_index(&self, scope: &MemoryScope) -> Result<String> {
        let pages = self.list_pages(scope.clone(), None).await?;
        let content = compile_index(scope, &pages);
        let index_path: MemoryPath = INDEX_FILENAME.into();
        let now = chrono::Utc::now();
        let page = WikiPage {
            path: Some(index_path.clone()),
            title: "Memory Index".to_string(),
            page_type: PageType::Index,
            content: content.clone(),
            created: now,
            updated: now,
            confidence: moa_core::ConfidenceLevel::High,
            related: pages
                .iter()
                .filter(|page| {
                    !matches!(
                        page.page_type,
                        PageType::Index | PageType::Log | PageType::Schema
                    )
                })
                .take(32)
                .map(|page| page.path.as_str().to_string())
                .collect(),
            sources: Vec::new(),
            tags: vec!["index".to_string()],
            auto_generated: true,
            last_referenced: now,
            reference_count: 0,
            metadata: std::collections::HashMap::new(),
        };
        self.write_page(scope.clone(), &index_path, page).await?;
        Ok(content)
    }

    /// Runs direct consolidation tasks against a single memory scope.
    pub async fn run_consolidation(&self, scope: &MemoryScope) -> Result<ConsolidationReport> {
        consolidation::run_consolidation(self, scope).await
    }

    /// Runs scheduled consolidation checks and executes any due workspace consolidations.
    pub async fn run_due_consolidations<S: SessionStore + ?Sized>(
        &self,
        session_store: &S,
    ) -> Result<Vec<ConsolidationReport>> {
        consolidation::run_due_consolidations(self, session_store).await
    }

    /// Ingests a raw source document into the scoped wiki and updates derived pages.
    pub async fn ingest_source(
        &self,
        scope: &MemoryScope,
        source_name: &str,
        source: &str,
    ) -> Result<IngestReport> {
        ingest::ingest_source(self, scope, source_name, source).await
    }

    /// Writes a branch-local page snapshot for later reconciliation.
    pub async fn write_page_branched(
        &self,
        scope: &MemoryScope,
        brain_id: &BrainId,
        path: &MemoryPath,
        page: WikiPage,
    ) -> Result<()> {
        branching::write_page_branched(self, scope, brain_id, path, page).await
    }

    /// Reconciles all pending branch-local writes back into the main scope.
    pub async fn reconcile_branches(&self, scope: &MemoryScope) -> Result<ReconcileReport> {
        branching::reconcile_branches(self, scope).await
    }

    pub(crate) async fn list_scope_files(&self, scope: &MemoryScope) -> Result<Vec<MemoryPath>> {
        let root = self.scope_root(scope);
        if !try_exists(&root).await? {
            return Ok(Vec::new());
        }

        let mut files = Vec::new();
        collect_markdown_files(&root, &root, &mut files).await?;
        files.sort_by(|left, right| left.as_str().cmp(right.as_str()));

        Ok(files)
    }

    pub(crate) fn scope_root(&self, scope: &MemoryScope) -> PathBuf {
        match scope {
            MemoryScope::User(_) => self.base_dir.join("memory"),
            MemoryScope::Workspace(workspace_id) => self
                .base_dir
                .join("workspaces")
                .join(workspace_id.as_str())
                .join("memory"),
        }
    }

    pub(crate) fn file_path(&self, scope: &MemoryScope, path: &MemoryPath) -> Result<PathBuf> {
        let logical_path = Path::new(path.as_str());
        if logical_path.is_absolute() {
            return Err(MoaError::ValidationError(format!(
                "memory paths must be relative: {}",
                path.as_str()
            )));
        }
        for component in logical_path.components() {
            if matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            ) {
                return Err(MoaError::ValidationError(format!(
                    "memory paths must stay within the scope root: {}",
                    path.as_str()
                )));
            }
        }

        Ok(self.scope_root(scope).join(logical_path))
    }
}

fn expand_local_path(path: &str) -> Result<PathBuf> {
    if let Some(relative) = path.strip_prefix("~/") {
        let home = env::var("HOME").map_err(|_| MoaError::HomeDirectoryNotFound)?;
        return Ok(PathBuf::from(home).join(relative));
    }

    Ok(PathBuf::from(path))
}

#[async_trait]
impl MemoryStore for FileMemoryStore {
    /// Searches indexed wiki content within a single scope.
    async fn search(
        &self,
        query: &str,
        scope: MemoryScope,
        limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        self.search_index.search(query, &scope, limit).await
    }

    /// Reads a wiki page within an explicit scope.
    async fn read_page(&self, scope: MemoryScope, path: &MemoryPath) -> Result<WikiPage> {
        let file_path = self.file_path(&scope, path)?;
        let markdown =
            fs::read_to_string(&file_path)
                .await
                .map_err(|error| match error.kind() {
                    std::io::ErrorKind::NotFound => {
                        MoaError::StorageError(format!("memory page not found: {}", path.as_str()))
                    }
                    _ => error.into(),
                })?;
        let mut page = parse_markdown(Some(path.clone()), &markdown)?;
        page.path = Some(path.clone());
        Ok(page)
    }

    /// Writes a wiki page within an explicit scope.
    async fn write_page(
        &self,
        scope: MemoryScope,
        path: &MemoryPath,
        mut page: WikiPage,
    ) -> Result<()> {
        let file_path = self.file_path(&scope, path)?;
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        page.path = Some(path.clone());
        let markdown = render_markdown(&page)?;
        fs::write(&file_path, markdown).await?;
        self.search_index.upsert_page(&scope, path, &page).await?;

        Ok(())
    }

    /// Deletes a wiki page within an explicit scope.
    async fn delete_page(&self, scope: MemoryScope, path: &MemoryPath) -> Result<()> {
        let file_path = self.file_path(&scope, path)?;
        match fs::remove_file(&file_path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(MoaError::StorageError(format!(
                    "memory page not found: {}",
                    path.as_str()
                )));
            }
            Err(error) => return Err(error.into()),
        }
        self.search_index.delete_page(&scope, path).await?;

        Ok(())
    }

    /// Lists all markdown pages stored in a scope.
    async fn list_pages(
        &self,
        scope: MemoryScope,
        filter: Option<PageType>,
    ) -> Result<Vec<PageSummary>> {
        let paths = self.list_scope_files(&scope).await?;
        let mut pages = Vec::new();

        for path in paths {
            let page = self.read_page(scope.clone(), &path).await?;
            if filter
                .as_ref()
                .is_some_and(|page_type| page.page_type != *page_type)
            {
                continue;
            }
            pages.push(PageSummary {
                path: path.clone(),
                title: page.title,
                page_type: page.page_type,
                confidence: page.confidence,
                updated: page.updated,
            });
        }

        Ok(pages)
    }

    /// Returns the truncated `MEMORY.md` contents for a scope.
    async fn get_index(&self, scope: MemoryScope) -> Result<String> {
        let index_path = self.scope_root(&scope).join(INDEX_FILENAME);
        load_index_file(&index_path).await
    }

    /// Ingests a raw source document into the scoped wiki and updates derived pages.
    async fn ingest_source(
        &self,
        scope: MemoryScope,
        source_name: &str,
        content: &str,
    ) -> Result<IngestReport> {
        FileMemoryStore::ingest_source(self, &scope, source_name, content).await
    }

    /// Rebuilds the FTS index for a scope from markdown files on disk.
    async fn rebuild_search_index(&self, scope: MemoryScope) -> Result<()> {
        let paths = self.list_scope_files(&scope).await?;
        let mut pages = Vec::with_capacity(paths.len());

        for path in paths {
            let page = self.read_page(scope.clone(), &path).await?;
            pages.push((path, page));
        }

        self.search_index.rebuild_scope(&scope, &pages).await
    }
}

pub(crate) fn memory_error(error: impl std::fmt::Display) -> MoaError {
    MoaError::StorageError(error.to_string())
}

async fn collect_markdown_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<MemoryPath>,
) -> Result<()> {
    let mut pending = vec![current.to_path_buf()];

    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            let path = entry.path();

            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if !file_type.is_file() || path.extension().is_none_or(|ext| ext != "md") {
                continue;
            }

            let relative = path
                .strip_prefix(root)
                .map_err(memory_error)?
                .to_string_lossy()
                .replace('\\', "/");
            files.push(MemoryPath::new(relative));
        }
    }

    Ok(())
}

async fn try_exists(path: &Path) -> Result<bool> {
    fs::try_exists(path).await.map_err(Into::into)
}
