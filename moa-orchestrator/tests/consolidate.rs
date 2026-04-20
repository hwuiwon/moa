//! Unit coverage for the consolidation workflow and memory service wrappers.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use chrono::{Duration, TimeZone, Utc};
use moa_core::{ConfidenceLevel, MemoryScope, MemoryStore, PageType, WikiPage, WorkspaceId};
use moa_memory::FileMemoryStore;
use moa_orchestrator::services::memory_store::MemoryStoreImpl;
use moa_orchestrator::workflows::consolidate::ConsolidateReport;
use moa_session::testing;
use tempfile::tempdir;

fn sample_page(title: &str, page_type: PageType, content: &str) -> WikiPage {
    let timestamp = Utc
        .with_ymd_and_hms(2026, 4, 9, 16, 45, 0)
        .single()
        .expect("valid timestamp");
    WikiPage {
        path: None,
        title: title.to_string(),
        page_type,
        content: content.to_string(),
        created: timestamp,
        updated: timestamp,
        confidence: ConfidenceLevel::High,
        related: Vec::new(),
        sources: Vec::new(),
        tags: vec!["rust".to_string()],
        auto_generated: false,
        last_referenced: timestamp,
        reference_count: 1,
        metadata: std::collections::HashMap::new(),
    }
}

#[test]
fn consolidate_report_maps_memory_report_fields() {
    let workspace_id = WorkspaceId::new("workspace-r09");
    let target_date = chrono::NaiveDate::from_ymd_opt(2026, 4, 20).expect("valid date");
    let ran_at = Utc
        .with_ymd_and_hms(2026, 4, 20, 5, 0, 0)
        .single()
        .expect("valid timestamp");

    let memory_report = moa_orchestrator::services::memory_store::WorkspaceConsolidationReport {
        scope: MemoryScope::Workspace(workspace_id.clone()),
        pages_updated: 2,
        pages_deleted: 1,
        relative_dates_normalized: 3,
        contradictions_resolved: 4,
        confidence_decayed: 5,
        orphaned_pages: vec!["topics/orphan.md".into()],
        memory_lines_before: 10,
        memory_lines_after: 12,
    };

    let report = ConsolidateReport::from_memory_report(
        workspace_id.clone(),
        target_date,
        ran_at,
        250,
        memory_report,
    );

    assert_eq!(report.workspace_id, workspace_id);
    assert_eq!(report.target_date, target_date);
    assert_eq!(report.pages_updated, 2);
    assert_eq!(report.pages_deleted, 1);
    assert_eq!(report.relative_dates_normalized, 3);
    assert_eq!(report.contradictions_resolved, 4);
    assert_eq!(report.confidence_decayed, 5);
    assert_eq!(report.orphaned_pages, vec!["topics/orphan.md".to_string()]);
    assert_eq!(report.duration_ms, 250);
    assert!(report.errors.is_empty());
}

#[tokio::test]
async fn memory_store_service_returns_empty_report_for_empty_workspace() -> Result<()> {
    let dir = tempdir()?;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    let file_store = Arc::new(
        FileMemoryStore::new_with_pool_and_schema(
            dir.path(),
            Arc::new(session_store.pool().clone()),
            Some(&schema_name),
        )
        .await?,
    );
    let service = MemoryStoreImpl::new(file_store);

    let report = service
        .run_workspace_consolidation_inner(WorkspaceId::new("empty-workspace"))
        .await
        .map_err(|error| anyhow!("{error:?}"))?;

    assert_eq!(report.pages_updated, 0);
    assert_eq!(report.pages_deleted, 0);

    testing::cleanup_test_schema(&database_url, &schema_name).await?;
    Ok(())
}

#[tokio::test]
async fn memory_store_service_normalizes_relative_dates() -> Result<()> {
    let dir = tempdir()?;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    let file_store = Arc::new(
        FileMemoryStore::new_with_pool_and_schema(
            dir.path(),
            Arc::new(session_store.pool().clone()),
            Some(&schema_name),
        )
        .await?,
    );
    let service = MemoryStoreImpl::new(file_store.clone());
    let workspace_id = WorkspaceId::new("normalize-workspace");
    let scope = MemoryScope::Workspace(workspace_id.clone());

    let mut page = sample_page(
        "Architecture",
        PageType::Topic,
        "# Architecture\n\nThe deploy happened today.\n",
    );
    page.updated -= Duration::days(10);
    file_store
        .write_page(&scope, &"topics/architecture.md".into(), page)
        .await?;

    let report = service
        .run_workspace_consolidation_inner(workspace_id)
        .await
        .map_err(|error| anyhow!("{error:?}"))?;
    let page = file_store
        .read_page(&scope, &"topics/architecture.md".into())
        .await?;

    assert!(report.relative_dates_normalized >= 1);
    assert!(!page.content.contains("today"));

    testing::cleanup_test_schema(&database_url, &schema_name).await?;
    Ok(())
}
