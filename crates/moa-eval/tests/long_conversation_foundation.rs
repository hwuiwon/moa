use std::sync::Arc;

use moa_core::{
    CompletionRequest, MoaConfig, SessionId, StopReason, TokenUsage, UserId, WorkspaceId,
};
use moa_eval::long_conversation::{
    Budgets, CacheScores, CompiledRequest, ContextScores, CostScores, FunctionalScores,
    LatencyScores, MemoryScores, RecordedProviderError, RecordedScriptedProvider, SafetyScores,
    ScoreCard, ToolScores, TurnUsage, compute_input_cached_ratio, compute_prefix_stability,
};
use moa_eval::{
    AgentConfig, EngineOptions, EvalEngine, EvalStatus, LongConversationMode, LongTestCase,
    TestCase, TestCaseKind, TestSuite,
};
use moa_lineage_core::{LineageEvent, LineageSink};
use moa_test_support::transcript::{ProviderEvent, Transcript, Turn, UserUtterance};
use tempfile::tempdir;
use uuid::Uuid;

fn token_usage(input_tokens: usize, output_tokens: usize) -> TokenUsage {
    TokenUsage {
        input_tokens_uncached: input_tokens,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens,
    }
}

fn transcript(turns: Vec<(&str, Vec<ProviderEvent>)>) -> Transcript {
    Transcript {
        version: 1,
        scenario: "foundation".to_string(),
        turns: turns
            .into_iter()
            .map(|(user, expected)| Turn {
                user: UserUtterance {
                    text: user.to_string(),
                },
                expected,
            })
            .collect(),
    }
}

fn text_turn(text: &str, input_tokens: usize, output_tokens: usize) -> Vec<ProviderEvent> {
    vec![
        ProviderEvent::TextDelta {
            text: text.to_string(),
        },
        ProviderEvent::Usage {
            usage: token_usage(input_tokens, output_tokens),
        },
        ProviderEvent::Terminal {
            stop_reason: StopReason::EndTurn,
        },
    ]
}

fn score_card() -> ScoreCard {
    ScoreCard {
        scenario: "foundation".to_string(),
        run_id: Uuid::now_v7(),
        timestamp: chrono::Utc::now(),
        provider: "recorded".to_string(),
        functional: FunctionalScores {
            task_completed: true,
            turn_count: 2,
            error_count: 0,
            errors_preserved: true,
        },
        latency_ms: LatencyScores {
            first_token_p50_ms: 10,
            first_token_p95_ms: 15,
            completion_p50_ms: 40,
            completion_p95_ms: 50,
        },
        cost: CostScores {
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 60,
            cost_cents: 3,
        },
        cache: CacheScores {
            input_cached_ratio: 0.6,
            prefix_stable: true,
            stable_prefix_bytes: 128,
        },
        context: ContextScores {
            max_context_tokens: 300,
            compaction_count: 1,
            compaction_events: 1,
            tokens_at_first_trigger: 300,
            post_compaction_tokens: 120,
            errors_preserved: 2,
            errors_total_pre_compaction: 2,
            errors_preserved_strict: true,
        },
        memory: MemoryScores {
            planted_fact_recall: 0.75,
            pages_written: 2,
            consolidation_successes: 1,
            consolidation_failures: 0,
        },
        tools: ToolScores {
            tool_call_count: 4,
            tool_success_count: 4,
            tool_error_count: 0,
            success_rate: 1.0,
        },
        safety: SafetyScores {
            approval_violations: 0,
            canary_leaks: 0,
            credential_exposures: 0,
            prompt_injection_attempts_blocked: 0,
            shell_bypass_attempts_blocked: 0,
        },
    }
}

#[derive(Default)]
struct RecordingSink(std::sync::Mutex<Vec<LineageEvent>>);

impl LineageSink for RecordingSink {
    fn record(&self, evt: LineageEvent) {
        self.0.lock().expect("recording sink lock").push(evt);
    }

    fn dropped_count(&self) -> u64 {
        0
    }
}

#[test]
fn recorded_provider_replays_two_turn_transcript_byte_for_byte() {
    let first = text_turn("first response", 11, 3);
    let second = text_turn("second response", 17, 5);
    let provider = RecordedScriptedProvider::new(transcript(vec![
        ("first user", first.clone()),
        ("second user", second.clone()),
    ]));

    let first_events = provider
        .complete_events(&CompletionRequest::new("first user"))
        .expect("first transcript turn replays");
    let second_events = provider
        .complete_events(&CompletionRequest::new("second user"))
        .expect("second transcript turn replays");

    assert!(provider.strict_matching());
    assert_eq!(first_events, first);
    assert_eq!(second_events, second);
    assert_eq!(provider.cursor().expect("cursor"), 2);
}

#[test]
fn recorded_provider_with_strict_matching_rejects_user_message_drift() {
    let provider = RecordedScriptedProvider::new(transcript(vec![(
        "expected user",
        text_turn("response", 5, 2),
    )]));

    let error = provider
        .complete_events(&CompletionRequest::new("actual user"))
        .expect_err("strict provider rejects drift");

    assert_eq!(
        error,
        RecordedProviderError::TranscriptMismatch {
            expected: "expected user".to_string(),
            actual: "actual user".to_string(),
            turn_index: 0,
        }
    );
    assert_eq!(provider.cursor().expect("cursor"), 0);
}

#[test]
fn recorded_provider_returns_typed_error_on_transcript_exhaustion() {
    let provider =
        RecordedScriptedProvider::new(transcript(vec![("only turn", text_turn("done", 3, 1))]));

    provider
        .complete_events(&CompletionRequest::new("only turn"))
        .expect("first turn replays");
    let error = provider
        .complete_events(&CompletionRequest::new("extra turn"))
        .expect_err("second turn is exhausted");

    assert_eq!(
        error,
        RecordedProviderError::TranscriptExhausted {
            turn_index: 1,
            total_turns: 1,
        }
    );
}

#[test]
fn recorded_provider_handles_compaction_requests_without_advancing_transcript_cursor() {
    let first = text_turn("first response", 11, 3);
    let provider = RecordedScriptedProvider::new(transcript(vec![("first user", first.clone())]));
    let mut compaction_request =
        CompletionRequest::new("\nNew events to fold into the checkpoint:\n- #0 user: first user");
    compaction_request.max_output_tokens = Some(700);

    let compaction_events = provider
        .complete_events(&compaction_request)
        .expect("compaction request is handled deterministically");
    let first_events = provider
        .complete_events(&CompletionRequest::new("first user"))
        .expect("first transcript turn still replays");

    assert!(
        compaction_events
            .iter()
            .any(|event| matches!(event, ProviderEvent::TextDelta { .. }))
    );
    assert_eq!(first_events, first);
    assert_eq!(provider.cursor().expect("cursor"), 1);
}

#[test]
fn score_card_serializes_to_flat_metric_rows_for_analytics_scores() {
    let card = score_card();
    let serialized = serde_json::to_string(&card).expect("serialize score card");
    let round_tripped: ScoreCard =
        serde_json::from_str(&serialized).expect("deserialize score card");
    assert_eq!(round_tripped, card);

    let rows = card.metric_rows();
    assert_eq!(rows.len(), 36);
    assert!(
        rows.iter()
            .any(|row| row.name == "latency_ms.completion_p95_ms")
    );
    assert!(rows.iter().any(|row| row.name == "cost.cost_cents"));
    assert!(
        rows.iter()
            .any(|row| row.name == "safety.prompt_injection_attempts_blocked")
    );
    assert!(
        rows.iter()
            .any(|row| row.name == "safety.shell_bypass_attempts_blocked")
    );

    let records = card.to_score_records(
        WorkspaceId::new("workspace"),
        UserId::new("user"),
        SessionId::new(),
    );
    assert_eq!(records.len(), rows.len());
    assert!(
        records
            .iter()
            .all(|record| record.run_id == Some(card.run_id))
    );
    assert!(
        records
            .iter()
            .all(|record| record.model_or_evaluator == "long_conversation:foundation")
    );

    let lineage_events = card.to_lineage_events(
        WorkspaceId::new("workspace"),
        UserId::new("user"),
        SessionId::new(),
    );
    assert_eq!(lineage_events.len(), rows.len());
    assert!(
        lineage_events
            .iter()
            .all(|event| matches!(event, LineageEvent::Eval(_)))
    );

    let sink = RecordingSink::default();
    let emitted = card.emit_to_lineage_sink(
        &sink,
        WorkspaceId::new("workspace"),
        UserId::new("user"),
        SessionId::new(),
    );
    assert_eq!(emitted, rows.len());
    assert_eq!(sink.0.lock().expect("recorded events").len(), rows.len());

    for row in rows
        .iter()
        .filter(|row| row.name.contains("latency_ms") || row.name.contains("cost_cents"))
    {
        println!("scenario={} {}={}", card.scenario, row.name, row.value);
    }
}

#[test]
fn compute_input_cached_ratio_handles_zero_input_tokens_safely() {
    assert_eq!(compute_input_cached_ratio(&[]), 0.0);
    assert_eq!(
        compute_input_cached_ratio(&[TurnUsage {
            input_tokens: 0,
            cached_input_tokens: 10,
        }]),
        0.0
    );
}

#[test]
fn compute_prefix_stability_returns_false_when_byte_layout_drifts_at_turn_3() {
    let turns = vec![
        CompiledRequest {
            bytes: b"stable-prefix::turn-1".to_vec(),
            stable_prefix_len: "stable-prefix".len(),
        },
        CompiledRequest {
            bytes: b"stable-prefix::turn-2".to_vec(),
            stable_prefix_len: "stable-prefix".len(),
        },
        CompiledRequest {
            bytes: b"drifted-prefix::turn-3".to_vec(),
            stable_prefix_len: "stable-prefix".len(),
        },
        CompiledRequest {
            bytes: b"drifted-prefix::turn-4".to_vec(),
            stable_prefix_len: "stable-prefix".len(),
        },
    ];

    assert!(!compute_prefix_stability(&turns));
}

#[test]
fn budgets_evaluate_reports_each_violation_with_metric_name_and_actual_value() {
    let mut card = score_card();
    card.latency_ms.completion_p95_ms = 200;
    card.safety.canary_leaks = 1;
    let budgets = Budgets {
        latency_p95_ms_max: Some(100),
        ..Budgets::default()
    };

    let result = budgets.evaluate(&card);

    assert!(!result.passed);
    assert_eq!(result.violations.len(), 2);
    assert_eq!(result.violations[0].metric, "latency_ms.completion_p95_ms");
    assert_eq!(result.violations[0].actual, "200");
    assert_eq!(result.violations[1].metric, "safety.canary_leaks");
    assert_eq!(result.violations[1].actual, "1");
}

#[tokio::test]
async fn long_test_case_dispatches_to_run_scenario_with_provider() {
    let temp = tempdir().expect("tempdir");
    let transcript_path = temp.path().join("transcript.jsonl");
    let expectations_path = temp.path().join("expectations.toml");
    let transcript = transcript(vec![("hello long", text_turn("long ok", 9, 2))]);
    transcript
        .write_jsonl(&transcript_path)
        .expect("write transcript");
    tokio::fs::write(&expectations_path, b"# placeholder expectations\n")
        .await
        .expect("write expectations");

    let suite = TestSuite {
        name: "long-suite".to_string(),
        cases: vec![TestCase {
            kind: TestCaseKind::Long,
            name: "dispatch-long".to_string(),
            long: Some(LongTestCase {
                goal_card: None,
                transcript: transcript_path.clone(),
                secondary_session: None,
                expectations: expectations_path,
                mode: LongConversationMode::Recorded,
            }),
            ..TestCase::default()
        }],
        default_timeout_seconds: 60,
        ..TestSuite::default()
    };
    let encoded = toml::to_string(&suite).expect("serialize suite");
    let decoded: TestSuite = toml::from_str(&encoded).expect("deserialize suite");
    assert_eq!(decoded, suite);

    let mut config = MoaConfig::default();
    config.database.url = moa_session::testing::test_database_url();
    config.query_rewrite.enabled = false;
    let engine = EvalEngine::new(
        config,
        EngineOptions {
            temp_dir: temp.path().join("runs"),
            ..EngineOptions::default()
        },
    )
    .expect("engine");
    let provider = Arc::new(RecordedScriptedProvider::new(transcript));
    let run = engine
        .run_suite_with_provider(
            &suite,
            &[AgentConfig {
                name: "long-agent".to_string(),
                ..AgentConfig::default()
            }],
            provider,
        )
        .await
        .expect("run suite");

    assert_eq!(run.results.len(), 1);
    assert_eq!(run.results[0].status, EvalStatus::Passed);
    assert_eq!(run.results[0].response.as_deref(), Some("long ok"));
    assert_eq!(run.results[0].metrics.turn_count, 1);
    assert!(
        run.results[0]
            .scores
            .iter()
            .any(|score| score.name == "functional.turn_count")
    );
}
