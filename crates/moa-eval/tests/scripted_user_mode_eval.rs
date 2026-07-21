use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use moa_core::{
    config::MoaConfig, events::Event, traits::LLMProvider, types::completion::CompletionContent,
    types::completion::CompletionRequest, types::completion::CompletionResponse,
    types::completion::CompletionStream, types::completion::StopReason,
    types::completion::TokenUsage, types::completion::ToolCallContent,
    types::completion::ToolInvocation, types::context::MessageRole, types::identifiers::ModelId,
    types::model::ModelCapabilities, types::model::TokenPricing, types::model::ToolCallFormat,
};
use moa_eval::long_conversation::{ScriptedUserScript, run_scenario_with_provider};
use moa_eval_core::{
    ActionPolicyOverride, ActionPolicyRuleOverride, AgentConfig, EngineOptions, EvalStatus,
    LongConversationMode, LongTestCase, TestCase, TestCaseKind, TestSuite,
};
use moa_lineage_core::{LineageEvent, ScoreValue as LineageScoreValue};
use serde_json::json;
use tempfile::tempdir;

type TestResult = Result<(), Box<dyn std::error::Error>>;

const RESPONSIVENESS_CLARIFICATION: &str = "What should I change? Point me at the file, message, object, or output and the specific fix you want.";

/// Fails clearly when a `#[ignore]`d Postgres-backed scripted-user test is forced
/// to run without `MOA_DATABASE_URL`, instead of passing vacuously.
fn require_database_url(test_name: &str) {
    assert!(
        std::env::var_os("MOA_DATABASE_URL").is_some(),
        "{test_name} requires MOA_DATABASE_URL; run it via the eval-recorded lane with Postgres \
         (e.g. `cargo test -p moa-eval --test scripted_user_mode_eval -- --ignored`)"
    );
}

#[test]
fn long_case_accepts_scripted_user_without_transcript() {
    let raw = r#"
[suite]
name = "scripted-user-suite"
default_timeout_seconds = 60

[[cases]]
kind = "long"
name = "scripted-user-case"
goal_card = "goal_card.md"
scripted_user = "script.jsonl"
expectations = "expectations.toml"
mode = "scripted_user"
"#;
    let suite: TestSuite = toml::from_str(raw).expect("scripted-user suite parses");
    let case = suite
        .cases
        .first()
        .expect("scripted-user suite has one case");

    let long = case.long_case().expect("scripted-user long case validates");

    assert_eq!(long.mode, LongConversationMode::ScriptedUser);
    assert_eq!(long.goal_card.as_deref(), Some(Path::new("goal_card.md")));
    assert_eq!(
        long.scripted_user.as_deref(),
        Some(Path::new("script.jsonl"))
    );
    assert!(long.transcript.as_os_str().is_empty());
}

#[test]
fn recorded_long_case_still_requires_transcript() {
    let case = TestCase {
        kind: TestCaseKind::Long,
        name: "recorded-without-transcript".to_string(),
        long: Some(LongTestCase {
            expectations: "expectations.toml".into(),
            mode: LongConversationMode::Recorded,
            ..LongTestCase::default()
        }),
        ..TestCase::default()
    };

    let error = case
        .long_case()
        .expect_err("recorded mode still requires transcript");

    assert!(
        error.to_string().contains("must set transcript"),
        "unexpected validation error: {error}"
    );
}

#[tokio::test]
async fn scripted_user_script_reads_turns_fragments_and_probe_ids() -> TestResult {
    let temp = tempdir()?;
    let script_path = temp.path().join("script.jsonl");
    let raw = r#"{"version":1,"scenario":"scripted-user-case","expected_final_answer_fragments":["scripted final"],"probe_ids":["probe-root"]}
{"user":{"text":"turn one"},"probe_ids":["probe-turn-one"]}
{"user":{"text":"turn two"}}
"#;
    tokio::fs::write(&script_path, raw).await?;

    let script = ScriptedUserScript::read_jsonl(&script_path).await?;

    assert_eq!(script.version, 1);
    assert_eq!(script.scenario, "scripted-user-case");
    assert_eq!(
        script.expected_final_answer_fragments,
        vec!["scripted final".to_string()]
    );
    assert_eq!(script.probe_ids, vec!["probe-root".to_string()]);
    assert_eq!(script.turns.len(), 2);
    assert_eq!(script.turns[0].user.text, "turn one");
    assert_eq!(
        script.turns[0].probe_ids,
        vec!["probe-turn-one".to_string()]
    );
    assert_eq!(script.turns[1].user.text, "turn two");
    Ok(())
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL"]
async fn scripted_user_runner_drives_tool_turn_and_checks_final_answer() -> TestResult {
    require_database_url("scripted_user_runner_drives_tool_turn_and_checks_final_answer");

    let temp = tempdir()?;
    let goal_card_path = temp.path().join("goal_card.md");
    let script_path = temp.path().join("script.jsonl");
    let expectations_path = temp.path().join("expectations.toml");
    tokio::fs::write(
        &goal_card_path,
        b"# Scripted user goal\nRun the command and report the result.\n",
    )
    .await?;
    tokio::fs::write(
        &script_path,
        r#"{"version":1,"scenario":"scripted-user-runner","expected_final_answer_fragments":["scripted final"],"probe_ids":["probe-runner"]}
{"user":{"text":"please run the command"},"probe_ids":["probe-tool"]}
"#,
    )
    .await?;
    tokio::fs::write(&expectations_path, b"# placeholder expectations\n").await?;

    let case = TestCase {
        kind: TestCaseKind::Long,
        name: "scripted-user-runner".to_string(),
        long: Some(LongTestCase {
            goal_card: Some(goal_card_path),
            scripted_user: Some(script_path),
            expectations: expectations_path,
            mode: LongConversationMode::ScriptedUser,
            ..LongTestCase::default()
        }),
        ..TestCase::default()
    };
    let agent_config = AgentConfig {
        name: "scripted-user-agent".to_string(),
        permissions: ActionPolicyOverride {
            allow_rules: vec![ActionPolicyRuleOverride {
                tool: "bash".to_string(),
                pattern: "printf scripted tool ok".to_string(),
                reason: Some("scripted-user fixture command".to_string()),
            }],
            ..ActionPolicyOverride::default()
        },
        ..AgentConfig::default()
    };
    let mut base_config = MoaConfig::default();
    base_config.database.url = moa_test_support::postgres::test_database_url();
    base_config.query_rewrite.enabled = false;

    let provider = Arc::new(ToolThenFinalProvider::default());
    let llm_provider: Arc<dyn LLMProvider> = provider.clone();
    let report = run_scenario_with_provider(
        &base_config,
        &agent_config,
        &EngineOptions {
            temp_dir: temp.path().join("runs"),
            ..EngineOptions::default()
        },
        &case,
        llm_provider,
    )
    .await?;

    assert_eq!(report.result.status, EvalStatus::Passed);
    assert_eq!(
        report.result.response.as_deref(),
        Some("Tool completed with scripted final fragment.")
    );
    assert_eq!(report.score_card.functional.turn_count, 1);
    assert_eq!(report.result.metrics.turn_count, 1);
    assert_tool_turn_lineage_is_captured(&report)?;
    assert_eq!(
        provider.seen_user_messages(),
        vec![
            "please run the command".to_string(),
            "please run the command".to_string(),
        ]
    );
    assert!(
        !report
            .events
            .iter()
            .any(|event| matches!(event, Event::ActionReviewRequested { .. })),
        "auto-mode tool execution should not create an action review"
    );
    assert!(
        report.events.iter().any(|event| matches!(
            event,
            Event::ToolResult { output, success, .. }
                if *success && output.to_text().contains("scripted tool ok")
        )),
        "bash tool result was not persisted; events: {:#?}",
        report.events
    );
    Ok(())
}

fn assert_tool_turn_lineage_is_captured(
    report: &moa_eval::long_conversation::LongRunReport,
) -> TestResult {
    // Pins: eval runs retain streamed context, generation, and citation verifier lineage.
    let lineage_events = report
        .lineage_events
        .iter()
        .map(|event| serde_json::from_value::<LineageEvent>(event.clone()))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        lineage_events
            .iter()
            .filter(|event| matches!(event, LineageEvent::Context(_)))
            .count(),
        2,
        "tool flow should compile context for the tool-call and final-answer provider turns"
    );
    assert_eq!(
        lineage_events
            .iter()
            .filter(|event| matches!(event, LineageEvent::Generation(_)))
            .count(),
        2,
        "tool flow should capture generation lineage for both provider responses"
    );
    let citation_events = lineage_events
        .iter()
        .filter_map(|event| match event {
            LineageEvent::Citation(record) => Some(record),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        citation_events.len(),
        2,
        "tool flow should emit citation lineage for both provider responses"
    );
    let cited_answer = citation_events
        .iter()
        .find(|event| !event.citations.is_empty())
        .expect("final answer should cite the tool-result context");
    assert_eq!(
        cited_answer.citations.len(),
        1,
        "final answer should produce one best citation candidate"
    );
    assert_eq!(
        cited_answer.citations[0].verifier.method,
        "bm25+lexical_overlap"
    );

    let citation_scores = report
        .score_records
        .iter()
        .filter(|record| record.name == "citation_verified")
        .collect::<Vec<_>>();
    assert_eq!(
        citation_scores.len(),
        1,
        "citation verifier score should be retained in report.score_records"
    );
    assert_eq!(
        citation_scores[0].model_or_evaluator,
        "bm25+lexical_overlap"
    );

    let lexical_scores = report
        .score_records
        .iter()
        .filter(|record| record.name == "lexical_overlap")
        .collect::<Vec<_>>();
    assert_eq!(
        lexical_scores.len(),
        1,
        "lexical-overlap score should be retained in report.score_records"
    );
    match lexical_scores[0].value {
        LineageScoreValue::Numeric(score) => {
            assert!(
                score > 0.0,
                "lexical-overlap score should reflect shared terms"
            );
        }
        ref other => panic!("lexical-overlap score should be numeric, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL"]
async fn offline_scripted_user_replays_distilled_dispute_failure_without_live_simulation()
-> TestResult {
    require_database_url(
        "offline_scripted_user_replays_distilled_dispute_failure_without_live_simulation",
    );

    let temp = tempdir()?;
    let goal_card_path = temp.path().join("goal_card.md");
    let script_path = temp.path().join("script.jsonl");
    let expectations_path = temp.path().join("expectations.toml");
    tokio::fs::write(
        &goal_card_path,
        b"# Distilled transaction-dispute failure\nReplay the ambiguous merchant case offline and require clarification before dispute drafting.\n",
    )
    .await?;
    tokio::fs::write(
        &script_path,
        r#"{"version":1,"scenario":"distilled-ambiguous-dispute","expected_final_answer_fragments":["merchant's legal name","before I draft a dispute"],"probe_ids":["distilled-dispute-regression"]}
{"user":{"text":"I need to dispute a charge labeled SQ * CITY MARKET, but I do not know the exact merchant."},"probe_ids":["ambiguous-merchant"]}
"#,
    )
    .await?;
    tokio::fs::write(&expectations_path, b"# placeholder expectations\n").await?;

    let case = TestCase {
        kind: TestCaseKind::Long,
        name: "distilled-ambiguous-dispute".to_string(),
        long: Some(LongTestCase {
            goal_card: Some(goal_card_path),
            scripted_user: Some(script_path),
            expectations: expectations_path,
            mode: LongConversationMode::ScriptedUser,
            ..LongTestCase::default()
        }),
        ..TestCase::default()
    };
    let agent_config = AgentConfig {
        name: "offline-distilled-dispute-agent".to_string(),
        ..AgentConfig::default()
    };
    let mut base_config = MoaConfig::default();
    base_config.database.url = moa_test_support::postgres::test_database_url();
    base_config.query_rewrite.enabled = false;

    let provider = Arc::new(ClarifyingDisputeProvider::default());
    let llm_provider: Arc<dyn LLMProvider> = provider.clone();
    let report = run_scenario_with_provider(
        &base_config,
        &agent_config,
        &EngineOptions {
            temp_dir: temp.path().join("runs"),
            ..EngineOptions::default()
        },
        &case,
        llm_provider,
    )
    .await?;

    assert_eq!(report.result.status, EvalStatus::Passed);
    assert_eq!(
        report.result.response.as_deref(),
        Some(
            "Please confirm the merchant's legal name, date, amount, and whether your card was present before I draft a dispute."
        )
    );
    assert_eq!(report.score_card.functional.turn_count, 1);
    assert_eq!(
        provider.seen_user_messages(),
        vec![
            "I need to dispute a charge labeled SQ * CITY MARKET, but I do not know the exact merchant."
                .to_string()
        ]
    );
    assert!(
        !report.events.iter().any(|event| matches!(
            event,
            Event::ActionReviewRequested { .. } | Event::ToolCall { .. }
        )),
        "offline replay should not need live simulation, connector calls, or action reviews"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL"]
async fn responsiveness_scripted_user_replays_vague_fix_this_clarification() -> TestResult {
    require_database_url("responsiveness_scripted_user_replays_vague_fix_this_clarification");

    let scenario_dir = responsiveness_fixture_dir();
    let temp = tempdir()?;
    let case = TestCase {
        kind: TestCaseKind::Long,
        name: "turn_responsiveness_vague_fix_this_clarification".to_string(),
        long: Some(LongTestCase {
            goal_card: Some(scenario_dir.join("goal_card.md")),
            scripted_user: Some(scenario_dir.join("scripted_user.jsonl")),
            expectations: scenario_dir.join("expectations.toml"),
            mode: LongConversationMode::ScriptedUser,
            ..LongTestCase::default()
        }),
        ..TestCase::default()
    };
    let agent_config = AgentConfig {
        name: "scripted-user-responsiveness-agent".to_string(),
        ..AgentConfig::default()
    };
    let mut base_config = MoaConfig::default();
    base_config.database.url = moa_test_support::postgres::test_database_url();
    base_config.query_rewrite.enabled = false;

    let provider = Arc::new(ResponsivenessClarificationProvider::default());
    let llm_provider: Arc<dyn LLMProvider> = provider.clone();
    let report = run_scenario_with_provider(
        &base_config,
        &agent_config,
        &EngineOptions {
            temp_dir: temp.path().join("runs"),
            ..EngineOptions::default()
        },
        &case,
        llm_provider,
    )
    .await?;

    assert_eq!(report.result.status, EvalStatus::Passed);
    assert_eq!(
        report.result.response.as_deref(),
        Some(RESPONSIVENESS_CLARIFICATION)
    );
    assert_eq!(report.score_card.functional.turn_count, 1);
    assert_eq!(report.result.metrics.turn_count, 1);
    assert_eq!(provider.seen_user_messages(), vec!["fix this".to_string()]);
    assert!(
        !report.events.iter().any(|event| matches!(
            event,
            Event::ActionReviewRequested { .. } | Event::ToolCall { .. }
        )),
        "responsiveness fixture should clarify without tool dispatch or action review"
    );
    Ok(())
}

#[derive(Default)]
struct ToolThenFinalProvider {
    seen_user_messages: Mutex<Vec<String>>,
}

impl ToolThenFinalProvider {
    fn seen_user_messages(&self) -> Vec<String> {
        self.seen_user_messages
            .lock()
            .expect("seen user messages lock")
            .clone()
    }
}

#[async_trait]
impl LLMProvider for ToolThenFinalProvider {
    fn name(&self) -> &str {
        "scripted-fake"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("scripted-fake-model"),
            context_window: 32_000,
            max_output: 1_024,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cached_input_per_mtok: Some(0.0),
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
        let latest_user = latest_user_message(&request).unwrap_or_default();
        let request_index = {
            let mut seen = self
                .seen_user_messages
                .lock()
                .map_err(|error| moa_core::error::MoaError::ProviderError(error.to_string()))?;
            seen.push(latest_user);
            seen.len()
        };
        let response = if request_index == 1 {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("tool-scripted-action".to_string()),
                        name: "bash".to_string(),
                        input: json!({ "cmd": "printf 'scripted tool ok'" }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: self.capabilities().model_id,
                usage: token_usage(12, 4),
                duration_ms: 1,
                thought_signature: None,
            }
        } else {
            assert!(
                request.messages.iter().any(|message| {
                    message.role == MessageRole::Tool
                        && message.content.contains("scripted tool ok")
                }),
                "second request did not include tool output: {request:?}"
            );
            CompletionResponse {
                text: "Tool completed with scripted final fragment.".to_string(),
                content: vec![CompletionContent::Text(
                    "Tool completed with scripted final fragment.".to_string(),
                )],
                stop_reason: StopReason::EndTurn,
                model: self.capabilities().model_id,
                usage: token_usage(18, 8),
                duration_ms: 1,
                thought_signature: None,
            }
        };
        Ok(CompletionStream::from_response(response))
    }
}

#[derive(Default)]
struct ClarifyingDisputeProvider {
    seen_user_messages: Mutex<Vec<String>>,
}

impl ClarifyingDisputeProvider {
    fn seen_user_messages(&self) -> Vec<String> {
        self.seen_user_messages
            .lock()
            .expect("seen user messages lock")
            .clone()
    }
}

#[async_trait]
impl LLMProvider for ClarifyingDisputeProvider {
    fn name(&self) -> &str {
        "offline-distilled-dispute"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("offline-distilled-dispute-model"),
            context_window: 32_000,
            max_output: 1_024,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cached_input_per_mtok: Some(0.0),
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
        let latest_user = latest_user_message(&request).unwrap_or_default();
        self.seen_user_messages
            .lock()
            .map_err(|error| moa_core::error::MoaError::ProviderError(error.to_string()))?
            .push(latest_user);

        let text = "Please confirm the merchant's legal name, date, amount, and whether your card was present before I draft a dispute.";
        Ok(CompletionStream::from_response(CompletionResponse {
            text: text.to_string(),
            content: vec![CompletionContent::Text(text.to_string())],
            stop_reason: StopReason::EndTurn,
            model: self.capabilities().model_id,
            usage: token_usage(16, 12),
            duration_ms: 1,
            thought_signature: None,
        }))
    }
}

#[derive(Default)]
struct ResponsivenessClarificationProvider {
    seen_user_messages: Mutex<Vec<String>>,
}

impl ResponsivenessClarificationProvider {
    fn seen_user_messages(&self) -> Vec<String> {
        self.seen_user_messages
            .lock()
            .expect("seen user messages lock")
            .clone()
    }
}

#[async_trait]
impl LLMProvider for ResponsivenessClarificationProvider {
    fn name(&self) -> &str {
        "scripted-user-responsiveness"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("scripted-user-responsiveness-model"),
            context_window: 32_000,
            max_output: 1_024,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cached_input_per_mtok: Some(0.0),
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> moa_core::error::Result<CompletionStream> {
        let latest_user = latest_user_message(&request).unwrap_or_default();
        if latest_user != "fix this" {
            return Err(moa_core::error::MoaError::ProviderError(format!(
                "unexpected responsiveness fixture user message: {latest_user}"
            )));
        }
        self.seen_user_messages
            .lock()
            .map_err(|error| moa_core::error::MoaError::ProviderError(error.to_string()))?
            .push(latest_user);

        Ok(CompletionStream::from_response(CompletionResponse {
            text: RESPONSIVENESS_CLARIFICATION.to_string(),
            content: vec![CompletionContent::Text(
                RESPONSIVENESS_CLARIFICATION.to_string(),
            )],
            stop_reason: StopReason::EndTurn,
            model: self.capabilities().model_id,
            usage: token_usage(12, 12),
            duration_ms: 1,
            thought_signature: None,
        }))
    }
}

fn latest_user_message(request: &CompletionRequest) -> Option<String> {
    request
        .messages
        .iter()
        .rev()
        .find(|message| {
            message.role == MessageRole::User && !message.content.starts_with("<system-reminder>")
        })
        .map(|message| message.content.clone())
}

fn responsiveness_fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scenarios/long_conversation/turn_responsiveness_vague_fix_this_clarification")
}

fn token_usage(input_tokens: usize, output_tokens: usize) -> TokenUsage {
    TokenUsage {
        input_tokens_uncached: input_tokens,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens,
    }
}
