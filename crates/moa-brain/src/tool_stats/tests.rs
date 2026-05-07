//! Unit tests for workspace tool statistics.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use moa_core::{
    Event, EventRecord, ModelId, Platform, SessionId, SessionMeta, ToolCallId, ToolContent,
    ToolOutput, UserId, WorkspaceId,
};
use uuid::Uuid;

use super::aggregation::{TOOL_STATS_EMA_ALPHA, workspace_tool_stats_from_events};
use super::ranking::{apply_tool_rankings, tool_annotation};
use super::*;

#[test]
fn ranking_puts_successful_tools_first() {
    let stats = WorkspaceToolStats {
        tools: HashMap::from([
            (
                "bash".to_string(),
                ToolStats {
                    tool_name: "bash".to_string(),
                    total_calls: 20,
                    ema_success_rate: 0.95,
                    ..ToolStats::default()
                },
            ),
            (
                "file_read".to_string(),
                ToolStats {
                    tool_name: "file_read".to_string(),
                    total_calls: 20,
                    ema_success_rate: 0.99,
                    ..ToolStats::default()
                },
            ),
            (
                "web_search".to_string(),
                ToolStats {
                    tool_name: "web_search".to_string(),
                    total_calls: 20,
                    ema_success_rate: 0.60,
                    ..ToolStats::default()
                },
            ),
        ]),
        ..WorkspaceToolStats::default()
    };
    let ranked = apply_tool_rankings(
        vec![
            serde_json::json!({"name": "web_search", "description": "search"}),
            serde_json::json!({"name": "bash", "description": "shell"}),
            serde_json::json!({"name": "file_read", "description": "read"}),
        ],
        &stats,
    );

    let names = ranked
        .iter()
        .map(|schema| schema["name"].as_str().expect("tool name"))
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["file_read", "bash", "web_search"]);
}

#[test]
fn annotation_warns_on_low_success() {
    let stats = ToolStats {
        tool_name: "web_search".to_string(),
        total_calls: 12,
        failures: 5,
        ema_success_rate: 0.5,
        common_errors: vec![("timeout".to_string(), 3)],
        ..ToolStats::default()
    };

    let annotation = tool_annotation(&stats).expect("annotation");
    assert!(annotation.contains("Workspace warning"));
    assert!(annotation.contains("timeout"));
}

#[test]
fn ema_decays_old_failures() {
    let mut value = 0.0;
    for _ in 0..7 {
        value = update_ema(value, 1.0, TOOL_STATS_EMA_ALPHA);
    }

    assert!(value > 0.5);
}

#[test]
fn no_annotation_below_threshold() {
    let stats = ToolStats {
        tool_name: "bash".to_string(),
        total_calls: 3,
        ema_success_rate: 1.0,
        ..ToolStats::default()
    };

    assert_eq!(tool_annotation(&stats), None);
}

#[tokio::test]
async fn stats_update_from_events() {
    let session = SessionMeta {
        id: SessionId::new(),
        workspace_id: WorkspaceId::new("ws-stats"),
        user_id: UserId::new("user"),
        platform: Platform::Cli,
        model: ModelId::new("claude-sonnet-4-6"),
        ..SessionMeta::default()
    };
    let now = Utc::now();
    let tool_id = ToolCallId::new();
    let events = vec![
        event_record(
            &session,
            1,
            now,
            Event::ToolCall {
                tool_id,
                provider_tool_use_id: None,
                provider_thought_signature: None,
                tool_name: "bash".to_string(),
                input: serde_json::json!({"cmd": "npm test"}),
                hand_id: None,
            },
        ),
        event_record(
            &session,
            2,
            now + Duration::seconds(1),
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: None,
                output: ToolOutput {
                    content: vec![ToolContent::Text {
                        text: "ok".to_string(),
                    }],
                    is_error: false,
                    structured: None,
                    duration: std::time::Duration::from_secs(2),
                    truncated: false,
                    original_output_tokens: None,
                    artifact: None,
                },
                original_output_tokens: None,
                success: true,
                duration_ms: 2000,
            },
        ),
        event_record(
            &session,
            3,
            now + Duration::seconds(2),
            Event::ToolError {
                tool_id: ToolCallId::new(),
                provider_tool_use_id: None,
                tool_name: "web_search".to_string(),
                error: "provider error: timeout".to_string(),
                retryable: false,
            },
        ),
    ];

    let stats = workspace_tool_stats_from_events(&events).expect("stats");

    assert_eq!(stats.sessions_tracked, 1);
    assert_eq!(stats.tools["bash"].successes, 1);
    assert_eq!(stats.tools["web_search"].failures, 1);
}

#[tokio::test]
async fn cache_stability_preserves_identical_ranked_output() {
    let stats = WorkspaceToolStats {
        tools: HashMap::from([(
            "bash".to_string(),
            ToolStats {
                tool_name: "bash".to_string(),
                total_calls: 12,
                ema_success_rate: 0.95,
                ..ToolStats::default()
            },
        )]),
        ..WorkspaceToolStats::default()
    };

    let first = serde_json::to_string(&apply_tool_rankings(
        vec![
            serde_json::json!({"name": "bash", "description": "shell"}),
            serde_json::json!({"name": "web_search", "description": "search"}),
        ],
        &stats,
    ))
    .expect("first serialization");
    let second = serde_json::to_string(&apply_tool_rankings(
        vec![
            serde_json::json!({"name": "bash", "description": "shell"}),
            serde_json::json!({"name": "web_search", "description": "search"}),
        ],
        &stats,
    ))
    .expect("second serialization");

    assert_eq!(first, second);
}

fn event_record(
    session: &SessionMeta,
    sequence_num: u64,
    timestamp: DateTime<Utc>,
    event: Event,
) -> EventRecord {
    EventRecord {
        id: Uuid::now_v7(),
        session_id: session.id,
        sequence_num,
        event_type: event.event_type(),
        event,
        timestamp,
        brain_id: None,
        hand_id: None,
        token_count: None,
    }
}
