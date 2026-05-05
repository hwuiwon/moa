//! Unit coverage for the graph-memory consolidation workflow shell.

use chrono::{TimeZone, Utc};
use moa_core::WorkspaceId;
use moa_orchestrator::workflows::consolidate::ConsolidateReport;

#[test]
fn graph_noop_report_has_no_wiki_page_updates() {
    let workspace_id = WorkspaceId::new("workspace-r09");
    let target_date = chrono::NaiveDate::from_ymd_opt(2026, 4, 20).expect("valid date");
    let ran_at = Utc
        .with_ymd_and_hms(2026, 4, 20, 5, 0, 0)
        .single()
        .expect("valid timestamp");

    let report = ConsolidateReport::graph_noop(workspace_id.clone(), target_date, ran_at, 250);

    assert_eq!(report.workspace_id, workspace_id);
    assert_eq!(report.target_date, target_date);
    assert_eq!(report.pages_updated, 0);
    assert_eq!(report.pages_deleted, 0);
    assert_eq!(report.relative_dates_normalized, 0);
    assert_eq!(report.contradictions_resolved, 0);
    assert_eq!(report.confidence_decayed, 0);
    assert!(report.orphaned_pages.is_empty());
    assert_eq!(report.duration_ms, 250);
    assert!(report.errors.is_empty());
}
