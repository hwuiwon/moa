//! Analytics service response-mapping and scoping tests.

use chrono::{TimeZone, Utc};
use moa_core::wire::ToolStatsRequest;
use moa_core::{
    CacheDailyMetric, SessionAnalyticsSummary, SessionId, SessionStatus, ToolCallSummary, UserId,
    WorkspaceAnalyticsSummary, WorkspaceId,
};
use moa_orchestrator::services::analytics::{
    ToolStatsScope, cache_stats_response_from_parts, session_stats_response_from_summary,
    tool_stats_response_from_rows, tool_stats_scope, workspace_stats_response_from_summary,
};

#[test]
fn tool_stats_scope_accepts_deployment_request() {
    // Pins: unscoped tool stats preserves the former deployment-wide aggregate mode.
    let request = ToolStatsRequest { workspace_id: None };

    let scope = tool_stats_scope(&request);

    assert_eq!(scope, ToolStatsScope::Deployment);
    assert_eq!(scope.workspace_id(), None);
}

#[test]
fn tool_stats_scope_accepts_explicit_workspace() {
    // Pins: tool stats uses the caller-provided workspace as the protected scope.
    let request = ToolStatsRequest {
        workspace_id: Some(WorkspaceId::new("workspace-a")),
    };

    let scope = tool_stats_scope(&request);

    assert_eq!(
        scope,
        ToolStatsScope::Workspace {
            workspace_id: WorkspaceId::new("workspace-a")
        }
    );
    assert_eq!(scope.workspace_id(), Some(&WorkspaceId::new("workspace-a")));
}

#[test]
fn analytics_response_helpers_preserve_core_fields() {
    // Pins: Analytics response helpers preserve core analytics values exactly.
    let session_id = SessionId(
        uuid::Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("session UUID fixture parses"),
    );
    let workspace_id = WorkspaceId::new("workspace-a");
    let user_id = UserId::new("user-a");
    let session = session_stats_response_from_summary(SessionAnalyticsSummary {
        session_id,
        workspace_id: workspace_id.clone(),
        user_id: user_id.clone(),
        status: SessionStatus::Completed,
        turn_count: 3,
        event_count: 9,
        total_input_tokens: 120,
        total_output_tokens: 45,
        total_cost_cents: 7,
        main_cost_cents: 6,
        auxiliary_cost_cents: 1,
        cache_hit_rate: 0.25,
        duration_seconds: 2.5,
        tool_call_count: 4,
        error_count: 1,
    });
    assert_eq!(session.session_id, session_id);
    assert_eq!(session.workspace_id, workspace_id);
    assert_eq!(session.user_id, user_id);
    assert_eq!(session.status, SessionStatus::Completed);
    assert_eq!(session.turn_count, 3);
    assert_eq!(session.error_count, 1);

    let workspace = workspace_stats_response_from_summary(WorkspaceAnalyticsSummary {
        workspace_id: WorkspaceId::new("workspace-a"),
        days: 14,
        session_count: 5,
        turn_count: 8,
        total_input_tokens: 1000,
        total_cache_read_tokens: 250,
        total_output_tokens: 300,
        total_cost_cents: 42,
        cache_hit_rate: 0.25,
    });
    assert_eq!(workspace.days, 14);
    assert_eq!(workspace.session_count, 5);
    assert_eq!(workspace.total_cache_read_tokens, 250);

    let tool = tool_stats_response_from_rows(
        Some(WorkspaceId::new("workspace-a")),
        vec![ToolCallSummary {
            tool_name: "file.read".to_string(),
            call_count: 4,
            avg_duration_ms: 12.5,
            p50_ms: 10.0,
            p95_ms: 20.0,
            success_rate: 0.75,
        }],
    );
    assert_eq!(tool.workspace_id, Some(WorkspaceId::new("workspace-a")));
    assert_eq!(tool.rows.len(), 1);
    assert_eq!(tool.rows[0].tool_name, "file.read");
    assert_eq!(tool.rows[0].success_rate, 0.75);

    let day = Utc
        .with_ymd_and_hms(2026, 6, 8, 0, 0, 0)
        .single()
        .expect("fixture datetime should be valid");
    let cache = cache_stats_response_from_parts(
        WorkspaceAnalyticsSummary {
            workspace_id: WorkspaceId::new("workspace-a"),
            days: 7,
            session_count: 2,
            turn_count: 3,
            total_input_tokens: 500,
            total_cache_read_tokens: 200,
            total_output_tokens: 100,
            total_cost_cents: 8,
            cache_hit_rate: 0.4,
        },
        vec![CacheDailyMetric {
            workspace_id: WorkspaceId::new("workspace-a"),
            day,
            session_count: 2,
            turn_count: 3,
            total_input_tokens: 500,
            total_cache_read_tokens: 200,
            total_output_tokens: 100,
            total_cost_cents: 8,
            avg_cache_hit_rate: 0.4,
        }],
    );
    assert_eq!(cache.days, 7);
    assert_eq!(cache.estimated_savings_cents, None);
    assert_eq!(cache.daily.len(), 1);
    assert_eq!(cache.daily[0].day, day);
    assert_eq!(cache.daily[0].avg_cache_hit_rate, 0.4);
}
