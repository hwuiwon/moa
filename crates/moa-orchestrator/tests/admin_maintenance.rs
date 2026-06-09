//! Admin maintenance service helper coverage.

use moa_core::WorkspaceId;
use moa_memory_vector::PromotionReport;
use moa_orchestrator::services::admin_maintenance::{
    promotion_response_from_report, promotion_update_response,
};

#[test]
fn promotion_report_maps_to_wire_response() {
    // Pins: hosted vector promotion preserves copied count, validation overlap, backend, and dual-read metadata.
    let response = promotion_response_from_report(
        PromotionReport {
            workspace_id: "workspace-a".to_string(),
            copied: 42,
            validation_overlap: 0.981,
            vector_backend: "turbopuffer".to_string(),
            vector_backend_state: "dual_read".to_string(),
        },
        Some(24),
    );

    assert_eq!(response.workspace_id, WorkspaceId::new("workspace-a"));
    assert_eq!(response.copied_vectors, 42);
    assert_eq!(response.validation_overlap, 0.981);
    assert_eq!(response.vector_backend, "turbopuffer");
    assert_eq!(response.vector_backend_state, "dual_read");
    assert_eq!(response.dual_read_hours, Some(24));
}

#[test]
fn promotion_update_response_marks_steady_state() {
    // Pins: rollback/finalize responses report a steady backend state without pretending vectors were copied.
    let response = promotion_update_response(WorkspaceId::new("workspace-a"), "pgvector", "steady");

    assert_eq!(response.workspace_id, WorkspaceId::new("workspace-a"));
    assert_eq!(response.copied_vectors, 0);
    assert_eq!(response.validation_overlap, 1.0);
    assert_eq!(response.vector_backend, "pgvector");
    assert_eq!(response.vector_backend_state, "steady");
    assert_eq!(response.dual_read_hours, None);
}
