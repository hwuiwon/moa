//! Analytics service response-mapping and scoping tests.

use chrono::{TimeZone, Utc};
use moa_core::wire::{
    ExperimentRunTrendPoint, ExperimentScoreRunRef, ExperimentStatusCount,
    ExperimentTrialTrendPoint, LearningCandidateListRequest, SessionSearchRequest,
    ToolStatsRequest,
};
use moa_core::{
    CacheDailyMetric, ContactId, Event, EventRecord, EventType, LearningCandidateStatus,
    SessionAnalyticsSummary, SessionId, SessionStatus, TenantAnalyticsSummary, TenantId,
    ToolCallSummary,
};
use moa_orchestrator::services::analytics::{
    ToolStatsScope, cache_stats_response_from_parts, experiment_stats_response_from_parts,
    redacted_event_snippet, redacted_payload_preview, session_search_response_from_events,
    session_stats_response_from_summary, tenant_stats_response_from_summary,
    tool_stats_response_from_rows, tool_stats_scope,
};
use serde_json::json;
use uuid::Uuid;

#[test]
fn tool_stats_scope_accepts_deployment_request() {
    // Pins: unscoped tool stats preserves the former deployment-wide aggregate mode.
    let request = ToolStatsRequest { tenant_id: None };

    let scope = tool_stats_scope(&request);

    assert_eq!(scope, ToolStatsScope::Deployment);
    assert_eq!(scope.tenant_id(), None);
}

#[test]
fn tool_stats_scope_accepts_explicit_tenant() {
    // Pins: tool stats uses the caller-provided tenant as the protected scope.
    let tenant_id = tenant("11111111-1111-1111-1111-111111111111");
    let request = ToolStatsRequest {
        tenant_id: Some(tenant_id),
    };

    let scope = tool_stats_scope(&request);

    assert_eq!(scope, ToolStatsScope::Tenant { tenant_id });
    assert_eq!(scope.tenant_id(), Some(&tenant_id));
}

#[test]
fn learning_candidate_list_request_is_tenant_bounded() {
    // Pins: learning-candidate analytics names the tenant boundary explicitly.
    let tenant_id = tenant("22222222-2222-2222-2222-222222222222");
    let request = LearningCandidateListRequest {
        tenant_id,
        status: Some(LearningCandidateStatus::Proposed),
        limit: 20,
    };

    assert_eq!(request.tenant_id, tenant_id);
    assert_eq!(request.status, Some(LearningCandidateStatus::Proposed));
}

#[test]
fn analytics_experiment_stats_response_sums_status_counts() {
    // Pins: experiment analytics exposes counts and score-run references without raw SQL results.
    let created_at = Utc
        .with_ymd_and_hms(2026, 6, 16, 12, 0, 0)
        .single()
        .expect("fixture datetime should be valid");
    let tenant_id = tenant("33333333-3333-3333-3333-333333333333");
    let response = experiment_stats_response_from_parts(
        tenant_id,
        vec![
            ExperimentStatusCount {
                status: "completed".to_string(),
                count: 2,
            },
            ExperimentStatusCount {
                status: "failed".to_string(),
                count: 1,
            },
        ],
        vec![ExperimentScoreRunRef {
            run_uid: Uuid::parse_str("11111111-1111-1111-1111-111111111111")
                .expect("run UUID fixture parses"),
            name: "support variant".to_string(),
            status: "completed".to_string(),
            score_run_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222")
                .expect("score UUID fixture parses"),
            created_at,
        }],
        vec![ExperimentRunTrendPoint {
            day: created_at,
            status: "completed".to_string(),
            count: 2,
        }],
        vec![ExperimentTrialTrendPoint {
            day: created_at,
            status: "completed".to_string(),
            variant_key: "candidate".to_string(),
            scenario_id: Some("ambiguous-merchant-dispute".to_string()),
            count: 4,
        }],
    );

    assert_eq!(response.tenant_id, tenant_id);
    assert_eq!(response.total_runs, 3);
    assert_eq!(response.statuses[0].status, "completed");
    assert_eq!(response.score_runs.len(), 1);
    assert_eq!(
        response.score_runs[0].score_run_id,
        Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("score UUID fixture parses")
    );
    assert_eq!(response.run_trends.len(), 1);
    assert_eq!(response.run_trends[0].day, created_at);
    assert_eq!(response.run_trends[0].status, "completed");
    assert_eq!(response.trial_trends.len(), 1);
    assert_eq!(response.trial_trends[0].variant_key, "candidate");
    assert_eq!(response.trial_trends[0].count, 4);
}

#[test]
fn analytics_response_helpers_preserve_core_fields() {
    // Pins: Analytics response helpers preserve core analytics values exactly.
    let session_id = SessionId(
        uuid::Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("session UUID fixture parses"),
    );
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let session = session_stats_response_from_summary(SessionAnalyticsSummary {
        session_id,
        tenant_id,
        contact_id: Some(contact_id),
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
    assert_eq!(session.tenant_id, tenant_id);
    assert_eq!(session.contact_id, Some(contact_id));
    assert_eq!(session.status, SessionStatus::Completed);
    assert_eq!(session.turn_count, 3);
    assert_eq!(session.error_count, 1);

    let tenant = tenant_stats_response_from_summary(TenantAnalyticsSummary {
        tenant_id,
        days: 14,
        session_count: 5,
        turn_count: 8,
        total_input_tokens: 1000,
        total_cache_read_tokens: 250,
        total_output_tokens: 300,
        total_cost_cents: 42,
        cache_hit_rate: 0.25,
    });
    assert_eq!(tenant.days, 14);
    assert_eq!(tenant.session_count, 5);
    assert_eq!(tenant.total_cache_read_tokens, 250);

    let tool = tool_stats_response_from_rows(
        Some(tenant_id),
        vec![ToolCallSummary {
            tool_name: "file.read".to_string(),
            call_count: 4,
            avg_duration_ms: 12.5,
            p50_ms: 10.0,
            p95_ms: 20.0,
            success_rate: 0.75,
        }],
    );
    assert_eq!(tool.tenant_id, Some(tenant_id));
    assert_eq!(tool.rows.len(), 1);
    assert_eq!(tool.rows[0].tool_name, "file.read");
    assert_eq!(tool.rows[0].success_rate, 0.75);

    let day = Utc
        .with_ymd_and_hms(2026, 6, 8, 0, 0, 0)
        .single()
        .expect("fixture datetime should be valid");
    let cache = cache_stats_response_from_parts(
        TenantAnalyticsSummary {
            tenant_id,
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
            tenant_id,
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

#[test]
fn analytics_redaction_helpers_remove_secret_values() {
    // Pins: analytics read models expose previews, not raw sensitive dynamic payload values.
    let payload = json!({
        "api_key": "sk-live-secret",
        "nested": {
            "token": "refresh-token-123",
            "safe": "visible"
        }
    });

    let preview = redacted_payload_preview(&payload);

    assert!(preview.contains("[redacted]"));
    assert!(preview.contains("visible"));
    assert!(!preview.contains("sk-live-secret"));
    assert!(!preview.contains("refresh-token-123"));

    let snippet = redacted_event_snippet(&Event::UserMessage {
        text: "rotate token=refresh-token-123 with sk-live-secret".to_string(),
        attachments: Vec::new(),
    });
    assert_eq!(snippet, "rotate [redacted] with [redacted]");

    let embedded = redacted_event_snippet(&Event::UserMessage {
        text: concat!(
            "check https://example.test/callback?token=refresh-token-123 ",
            "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.sflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c ",
            "aws AKIAIOSFODNN7EXAMPLE google AIzaSyD0f4akeKeyStringNeedsThirtyFiveChars"
        )
        .to_string(),
        attachments: Vec::new(),
    });
    assert!(embedded.contains("[redacted]"));
    assert!(!embedded.contains("refresh-token-123"));
    assert!(!embedded.contains("eyJhbGciOiJIUzI1NiJ9"));
    assert!(!embedded.contains("AKIAIOSFODNN7EXAMPLE"));
    assert!(!embedded.contains("AIzaSyD0f4akeKeyStringNeedsThirtyFiveChars"));
}

#[test]
fn session_search_response_returns_redacted_snippets_only() {
    // Pins: session search returns event IDs and redacted snippets, not full raw event payloads.
    let tenant_id = TenantId::new();
    let session_id = SessionId(
        Uuid::parse_str("33333333-3333-3333-3333-333333333333")
            .expect("session UUID fixture parses"),
    );
    let event_id =
        Uuid::parse_str("44444444-4444-4444-4444-444444444444").expect("event UUID fixture parses");
    let timestamp = Utc
        .with_ymd_and_hms(2026, 6, 16, 13, 0, 0)
        .single()
        .expect("fixture datetime should be valid");
    let response = session_search_response_from_events(
        SessionSearchRequest {
            tenant_id,
            query: "token".to_string(),
            from_time: None,
            to_time: None,
            event_types: Some(vec![EventType::UserMessage]),
            limit: 10,
        },
        vec![EventRecord {
            id: event_id,
            session_id,
            sequence_num: 7,
            event_type: EventType::UserMessage,
            event: Event::UserMessage {
                text: "refresh token=refresh-token-123".to_string(),
                attachments: Vec::new(),
            },
            timestamp,
            brain_id: None,
            hand_id: None,
            token_count: None,
        }],
    );

    assert_eq!(response.tenant_id, tenant_id);
    assert_eq!(response.query, "token");
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].session_id, session_id);
    assert_eq!(response.results[0].event_id, event_id);
    assert_eq!(response.results[0].sequence_num, 7);
    assert_eq!(response.results[0].event_type, EventType::UserMessage);
    assert_eq!(response.results[0].timestamp, timestamp);
    assert_eq!(response.results[0].snippet, "refresh [redacted]");
    assert!(!response.results[0].snippet.contains("refresh-token-123"));
}

fn tenant(value: &str) -> TenantId {
    TenantId::from(Uuid::parse_str(value).expect("tenant UUID fixture parses"))
}
