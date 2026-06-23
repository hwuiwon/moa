//! LineageAdmin helper coverage.

use moa_orchestrator::services::lineage_admin::prepare_lineage_sql;

#[test]
fn prepare_lineage_sql_scopes_logical_source_to_workspace_and_since() {
    // Pins: LineageAdmin query rewrites the logical source to a tenant-scoped hot-store subquery.
    let sql = prepare_lineage_sql("SELECT count(*) FROM lineage WHERE record_kind = 4")
        .expect("lineage query should prepare");

    assert!(sql.contains("analytics.turn_lineage"));
    assert!(sql.contains("workspace_id = $1"));
    assert!(sql.contains("($2::text)::interval"));
    assert!(sql.contains("record_kind = 4"));
}

#[test]
fn prepare_lineage_sql_rejects_mutating_statement() {
    // Pins: LineageAdmin rejects mutating SQL before any database query runs.
    let error =
        prepare_lineage_sql("DELETE FROM lineage").expect_err("mutating lineage query should fail");

    assert!(format!("{error:?}").contains("only SELECT"));
}
