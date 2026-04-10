//! Branch-isolated wiki writes and deterministic reconciliation helpers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use moa_core::{BrainId, MemoryPath, MemoryScope, MemoryStore, Result, WikiPage};
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::FileMemoryStore;
use crate::index::{LogChange, LogEntry};
use crate::memory_error;
use crate::wiki::{parse_markdown, render_markdown};

const BRANCH_ROOT: &str = ".branches";
const CHANGE_MANIFEST: &str = "_changes.json";

/// Change kind recorded inside a branch manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOperation {
    /// Branch wrote or updated a page.
    Write,
    /// Branch deleted a page.
    Delete,
}

/// Single branch-local change record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRecord {
    /// Logical page path affected by the branch.
    pub path: MemoryPath,
    /// Operation applied to the page.
    pub operation: ChangeOperation,
    /// Branch-local timestamp.
    pub timestamp: DateTime<Utc>,
}

/// Summary of a branch reconciliation run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Scope that was reconciled.
    pub scope: MemoryScope,
    /// Number of newly created mainline pages.
    pub pages_created: usize,
    /// Number of existing mainline pages updated without conflict.
    pub pages_merged: usize,
    /// Number of conflicting pages merged deterministically.
    pub conflicts_resolved: usize,
    /// Number of branch directories removed after reconciliation.
    pub branches_reconciled: usize,
}

impl ReconcileReport {
    /// Returns an empty reconciliation report for a scope.
    pub fn empty(scope: MemoryScope) -> Self {
        Self {
            scope,
            pages_created: 0,
            pages_merged: 0,
            conflicts_resolved: 0,
            branches_reconciled: 0,
        }
    }
}

/// Writes a page into a branch-local directory instead of the main scope root.
pub async fn write_page_branched(
    store: &FileMemoryStore,
    scope: &MemoryScope,
    brain_id: &BrainId,
    path: &MemoryPath,
    mut page: WikiPage,
) -> Result<()> {
    let branch_path = branch_file_path(store, scope, brain_id, path)?;
    if let Some(parent) = branch_path.parent() {
        fs::create_dir_all(parent).await?;
    }

    page.path = Some(path.clone());
    fs::write(branch_path, render_markdown(&page)?).await?;
    append_change_manifest(
        &branch_dir(store, scope, brain_id),
        ChangeRecord {
            path: path.clone(),
            operation: ChangeOperation::Write,
            timestamp: page.updated,
        },
    )
    .await
}

/// Returns all branch identifiers present for a scope.
pub async fn list_branches(store: &FileMemoryStore, scope: &MemoryScope) -> Result<Vec<String>> {
    let root = branch_root(store, scope);
    if !fs::try_exists(&root).await? {
        return Ok(Vec::new());
    }

    let mut branches = Vec::new();
    let mut entries = fs::read_dir(root).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            branches.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    branches.sort();
    Ok(branches)
}

/// Reconciles all branch-local writes into the main memory scope.
pub async fn reconcile_branches(
    store: &FileMemoryStore,
    scope: &MemoryScope,
) -> Result<ReconcileReport> {
    let branches = list_branches(store, scope).await?;
    let mut report = ReconcileReport::empty(scope.clone());
    if branches.is_empty() {
        return Ok(report);
    }

    for branch in branches {
        let branch_root = branch_root(store, scope).join(&branch);
        let changes = read_change_manifest(&branch_root).await?;

        for change in changes {
            match change.operation {
                ChangeOperation::Write => {
                    let branch_page = read_branch_page(store, scope, &branch, &change.path).await?;
                    match store.read_page(scope.clone(), &change.path).await {
                        Ok(main_page)
                            if main_page.updated > branch_page.updated
                                || (main_page.updated == branch_page.updated
                                    && main_page.content.trim() != branch_page.content.trim()) =>
                        {
                            let merged = merge_pages(&main_page, &branch_page);
                            store
                                .write_page(scope.clone(), &change.path, merged)
                                .await?;
                            report.conflicts_resolved += 1;
                        }
                        Ok(_) => {
                            store
                                .write_page(scope.clone(), &change.path, branch_page)
                                .await?;
                            report.pages_merged += 1;
                        }
                        Err(_) => {
                            store
                                .write_page(scope.clone(), &change.path, branch_page)
                                .await?;
                            report.pages_created += 1;
                        }
                    }
                }
                ChangeOperation::Delete => {
                    if store.delete_page(scope.clone(), &change.path).await.is_ok() {
                        report.pages_merged += 1;
                    }
                }
            }
        }

        fs::remove_dir_all(&branch_root).await?;
        report.branches_reconciled += 1;
    }

    store.refresh_scope_index(scope).await?;
    store.rebuild_search_index(scope.clone()).await?;
    store
        .append_scope_log(
            scope,
            LogEntry {
                timestamp: Utc::now(),
                operation: "reconcile".to_string(),
                description: "Reconciled branch-local memory writes".to_string(),
                changes: vec![LogChange {
                    action: "Merged".to_string(),
                    path: "MEMORY.md".into(),
                    detail: Some(format!(
                        "{} branches, {} created, {} updated, {} conflicts",
                        report.branches_reconciled,
                        report.pages_created,
                        report.pages_merged,
                        report.conflicts_resolved
                    )),
                }],
                brain_session: None,
            },
        )
        .await?;

    Ok(report)
}

pub(crate) async fn read_branch_page(
    store: &FileMemoryStore,
    scope: &MemoryScope,
    branch: &str,
    path: &MemoryPath,
) -> Result<WikiPage> {
    store.file_path(scope, path)?;
    let raw =
        fs::read_to_string(branch_root(store, scope).join(branch).join(path.as_str())).await?;
    let mut page = parse_markdown(Some(path.clone()), &raw)?;
    page.path = Some(path.clone());
    Ok(page)
}

fn merge_pages(main: &WikiPage, branch: &WikiPage) -> WikiPage {
    let newer = if branch.updated >= main.updated {
        branch
    } else {
        main
    };
    let mut merged = newer.clone();
    merged.created = main.created.min(branch.created);
    merged.updated = main.updated.max(branch.updated);
    merged.auto_generated = main.auto_generated && branch.auto_generated;
    merged.related = union_strings(&main.related, &branch.related);
    merged.sources = union_strings(&main.sources, &branch.sources);
    merged.tags = union_strings(&main.tags, &branch.tags);
    merged.reference_count = main.reference_count.max(branch.reference_count);
    merged.last_referenced = main.last_referenced.max(branch.last_referenced);
    merged.metadata.extend(main.metadata.clone());
    merged.metadata.extend(branch.metadata.clone());
    merged.content = merge_markdown(&main.content, &branch.content);
    merged
}

fn merge_markdown(main: &str, branch: &str) -> String {
    if main.trim() == branch.trim() {
        return main.to_string();
    }
    if main.contains(branch.trim()) {
        return main.to_string();
    }
    if branch.contains(main.trim()) {
        return branch.to_string();
    }

    let mut merged = Vec::new();
    let mut seen = HashSet::new();
    let mut last_blank = false;

    for source in [main, branch] {
        for line in source.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                if !last_blank && !merged.is_empty() {
                    merged.push(String::new());
                }
                last_blank = true;
                continue;
            }

            let key = trimmed.to_ascii_lowercase();
            if seen.insert(key) {
                merged.push(line.to_string());
                last_blank = false;
            }
        }
    }

    merged.join("\n")
}

fn union_strings(left: &[String], right: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut values = Vec::new();

    for value in left.iter().chain(right.iter()) {
        if seen.insert(value.to_ascii_lowercase()) {
            values.push(value.clone());
        }
    }

    values
}

async fn append_change_manifest(branch_root: &Path, change: ChangeRecord) -> Result<()> {
    fs::create_dir_all(branch_root).await?;
    let manifest_path = branch_root.join(CHANGE_MANIFEST);
    let mut changes = read_change_manifest(branch_root).await?;
    changes.push(change);
    let serialized = serde_json::to_vec_pretty(&changes).map_err(memory_error)?;
    fs::write(manifest_path, serialized).await?;
    Ok(())
}

async fn read_change_manifest(branch_root: &Path) -> Result<Vec<ChangeRecord>> {
    let manifest_path = branch_root.join(CHANGE_MANIFEST);
    match fs::read(&manifest_path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(memory_error),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn branch_root(store: &FileMemoryStore, scope: &MemoryScope) -> PathBuf {
    store.scope_root(scope).join(BRANCH_ROOT)
}

fn branch_dir(store: &FileMemoryStore, scope: &MemoryScope, brain_id: &BrainId) -> PathBuf {
    branch_root(store, scope).join(brain_id.to_string())
}

fn branch_file_path(
    store: &FileMemoryStore,
    scope: &MemoryScope,
    brain_id: &BrainId,
    path: &MemoryPath,
) -> Result<PathBuf> {
    store.file_path(scope, path)?;
    let relative = Path::new(path.as_str());
    Ok(branch_dir(store, scope, brain_id).join(relative))
}
