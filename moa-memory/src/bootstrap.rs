//! First-run workspace memory bootstrap from project instruction files.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use moa_core::{ConfidenceLevel, MemoryPath, MemoryScope, MemoryStore, PageType, Result, WikiPage};
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::FileMemoryStore;
use crate::index::INDEX_FILENAME;

const INSTRUCTION_FILES: &[&str] = &["CONTRIBUTING.md"];
const BOOTSTRAP_PAGE_PATHS: &[&str] = &[INDEX_FILENAME, "topics/project.md"];
const SENTINEL_FILENAME: &str = "_bootstrap.json";
const MAX_INSTRUCTION_SIZE: usize = 50 * 1024;
const TRUNCATION_NOTE: &str = "\n\n<!-- Truncated: original file exceeded 50KB -->";

/// Summary of one successful workspace bootstrap run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapReport {
    /// Instruction filename copied into memory, when one was found.
    pub source_file: Option<String>,
    /// Logical wiki pages created or updated by bootstrap.
    pub pages_created: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BootstrapSentinel {
    bootstrapped_at: DateTime<Utc>,
    source_file: Option<String>,
    pages_created: Vec<String>,
}

/// Returns whether the given scope should receive an automatic bootstrap pass.
pub async fn should_bootstrap(store: &FileMemoryStore, scope: &MemoryScope) -> Result<bool> {
    if fs::try_exists(&sentinel_path(store, scope)).await? {
        return Ok(false);
    }

    let pages = store.list_pages(scope, None).await?;
    if pages.is_empty() {
        return Ok(true);
    }

    for summary in pages {
        if !BOOTSTRAP_PAGE_PATHS.contains(&summary.path.as_str()) {
            return Ok(false);
        }
        if summary.path.as_str() == INDEX_FILENAME {
            let index = store.get_index(scope).await?;
            if !index.trim().is_empty() && !is_bootstrap_index(&index) {
                return Ok(false);
            }
        } else {
            let page = store.read_page(scope, &summary.path).await?;
            if !page.auto_generated {
                return Ok(false);
            }
        }
    }

    Ok(true)
}

/// Runs workspace bootstrap by copying the highest-priority instruction file into memory.
pub async fn run_bootstrap(
    store: &FileMemoryStore,
    scope: &MemoryScope,
    workspace_path: &Path,
    workspace_name: &str,
) -> Result<BootstrapReport> {
    let now = Utc::now();
    let (source_file, content) = find_instruction_file(workspace_path).await?;
    let mut pages_created = Vec::new();

    let index_path = MemoryPath::new(INDEX_FILENAME);
    let index_page = match (&source_file, &content) {
        (Some(filename), Some(_)) => index_page_with_instructions(
            index_path.clone(),
            workspace_name,
            workspace_path,
            filename,
            now,
        ),
        _ => minimal_index_page(index_path.clone(), workspace_name, workspace_path, now),
    };
    store.write_page(scope, &index_path, index_page).await?;
    pages_created.push(INDEX_FILENAME.to_string());

    if let (Some(filename), Some(contents)) = (&source_file, content.as_deref()) {
        let project_path = MemoryPath::new("topics/project.md");
        let project_page = project_instructions_page(project_path.clone(), filename, contents, now);
        store.write_page(scope, &project_path, project_page).await?;
        pages_created.push(project_path.as_str().to_string());
    } else {
        let project_path = MemoryPath::new("topics/project.md");
        if store.read_page(scope, &project_path).await.is_ok() {
            store.delete_page(scope, &project_path).await?;
        }
    }

    write_sentinel(
        &sentinel_path(store, scope),
        &BootstrapSentinel {
            bootstrapped_at: now,
            source_file: source_file.clone(),
            pages_created: pages_created.clone(),
        },
    )
    .await?;

    Ok(BootstrapReport {
        source_file,
        pages_created,
    })
}

async fn find_instruction_file(workspace_path: &Path) -> Result<(Option<String>, Option<String>)> {
    for filename in INSTRUCTION_FILES {
        let path = workspace_path.join(filename);
        let bytes = match fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let (contents, truncated) = truncate_instruction_bytes(&bytes);
        let mut contents = String::from_utf8_lossy(contents).into_owned();
        if truncated {
            contents.push_str(TRUNCATION_NOTE);
        }
        return Ok((Some((*filename).to_string()), Some(contents)));
    }

    Ok((None, None))
}

fn truncate_instruction_bytes(bytes: &[u8]) -> (&[u8], bool) {
    if bytes.len() <= MAX_INSTRUCTION_SIZE {
        return (bytes, false);
    }

    (&bytes[..MAX_INSTRUCTION_SIZE], true)
}

fn index_page_with_instructions(
    path: MemoryPath,
    workspace_name: &str,
    workspace_path: &Path,
    filename: &str,
    now: DateTime<Utc>,
) -> WikiPage {
    WikiPage {
        path: Some(path),
        title: format!("Workspace: {workspace_name}"),
        page_type: PageType::Index,
        content: format!(
            "# Workspace: {workspace_name}\n\n\
             Workspace path: `{}`\n\n\
             Project instructions loaded from `{filename}`.\n\
             See [[topics/project]] for full project context.\n",
            workspace_path.display()
        ),
        created: now,
        updated: now,
        confidence: ConfidenceLevel::High,
        related: vec!["topics/project.md".to_string()],
        sources: Vec::new(),
        tags: vec!["index".to_string()],
        auto_generated: true,
        last_referenced: now,
        reference_count: 0,
        metadata: Default::default(),
    }
}

fn minimal_index_page(
    path: MemoryPath,
    workspace_name: &str,
    workspace_path: &Path,
    now: DateTime<Utc>,
) -> WikiPage {
    WikiPage {
        path: Some(path),
        title: format!("Workspace: {workspace_name}"),
        page_type: PageType::Index,
        content: format!(
            "# Workspace: {workspace_name}\n\n\
             Workspace path: `{}`\n\n\
             No project instructions file found.\n\
             As you learn about this project, update this index.\n",
            workspace_path.display()
        ),
        created: now,
        updated: now,
        confidence: ConfidenceLevel::High,
        related: Vec::new(),
        sources: Vec::new(),
        tags: vec!["index".to_string()],
        auto_generated: true,
        last_referenced: now,
        reference_count: 0,
        metadata: Default::default(),
    }
}

fn project_instructions_page(
    path: MemoryPath,
    filename: &str,
    content: &str,
    now: DateTime<Utc>,
) -> WikiPage {
    WikiPage {
        path: Some(path),
        title: "Project Instructions".to_string(),
        page_type: PageType::Topic,
        content: format!("# Project Instructions\n\nSource: `{filename}`\n\n{content}"),
        created: now,
        updated: now,
        confidence: ConfidenceLevel::High,
        related: vec![INDEX_FILENAME.to_string()],
        sources: vec![filename.to_string()],
        tags: vec!["project".to_string(), "instructions".to_string()],
        auto_generated: true,
        last_referenced: now,
        reference_count: 0,
        metadata: Default::default(),
    }
}

fn is_bootstrap_index(index: &str) -> bool {
    index.contains("Workspace path: `")
        && (index.contains("Project instructions loaded from `")
            || index.contains("No project instructions file found."))
}

async fn write_sentinel(path: &Path, sentinel: &BootstrapSentinel) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, serde_json::to_vec_pretty(sentinel)?).await?;
    Ok(())
}

fn sentinel_path(store: &FileMemoryStore, scope: &MemoryScope) -> PathBuf {
    store.scope_root(scope).join(SENTINEL_FILENAME)
}

#[cfg(test)]
mod tests {
    use moa_core::MemoryStore;
    use tempfile::tempdir;
    use tokio::fs;

    use super::{
        INDEX_FILENAME, MemoryPath, MemoryScope, PageType, SENTINEL_FILENAME, run_bootstrap,
        should_bootstrap,
    };
    use crate::FileMemoryStore;

    #[tokio::test]
    async fn run_bootstrap_uses_contributing_file_and_can_rerun_after_sentinel_delete() {
        let workspace = tempdir().unwrap();
        fs::write(
            workspace.path().join("CONTRIBUTING.md"),
            "# CONTRIBUTING\n\npreferred instructions\n",
        )
        .await
        .unwrap();

        let base = tempdir().unwrap();
        let store = FileMemoryStore::new(base.path()).await.unwrap();
        let scope = MemoryScope::Workspace {
            workspace_id: "workspace".into(),
        };

        let report = run_bootstrap(&store, &scope, workspace.path(), "workspace")
            .await
            .unwrap();
        assert_eq!(report.source_file.as_deref(), Some("CONTRIBUTING.md"));
        assert_eq!(
            report.pages_created,
            vec!["MEMORY.md".to_string(), "topics/project.md".to_string()]
        );

        let project = store
            .read_page(&scope, &MemoryPath::new("topics/project.md"))
            .await
            .unwrap();
        assert_eq!(project.page_type, PageType::Topic);
        assert!(project.content.contains("preferred instructions"));
        assert!(!project.content.contains("fallback instructions"));
        assert!(!should_bootstrap(&store, &scope).await.unwrap());

        let sentinel = base
            .path()
            .join("workspaces")
            .join("workspace")
            .join("memory")
            .join(SENTINEL_FILENAME);
        fs::remove_file(&sentinel).await.unwrap();
        assert!(should_bootstrap(&store, &scope).await.unwrap());
    }

    #[tokio::test]
    async fn run_bootstrap_without_instruction_file_writes_minimal_index() {
        let workspace = tempdir().unwrap();
        let base = tempdir().unwrap();
        let store = FileMemoryStore::new(base.path()).await.unwrap();
        let scope = MemoryScope::Workspace {
            workspace_id: "workspace".into(),
        };

        let report = run_bootstrap(&store, &scope, workspace.path(), "workspace")
            .await
            .unwrap();

        assert_eq!(report.source_file, None);
        assert_eq!(report.pages_created, vec![INDEX_FILENAME.to_string()]);

        let index = store.get_index(&scope).await.unwrap();
        assert!(index.contains("No project instructions file found"));
        assert!(
            store
                .read_page(&scope, &MemoryPath::new("topics/project.md"))
                .await
                .is_err()
        );
    }
}
