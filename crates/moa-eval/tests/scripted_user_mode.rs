use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, CompletionStream, Event, LLMProvider,
    MessageRole, MoaConfig, ModelCapabilities, ModelId, StopReason, TokenPricing, TokenUsage,
    ToolCallContent, ToolCallFormat, ToolInvocation,
};
use moa_eval::long_conversation::{
    ScriptedApprovalDecision, ScriptedUserScript, run_scenario_with_provider,
};
use moa_eval::{
    AgentConfig, EngineOptions, EvalStatus, LongConversationMode, LongTestCase, PermissionOverride,
    TestCase, TestCaseKind, TestSuite,
};
use serde_json::json;
use tempfile::tempdir;

type TestResult = Result<(), Box<dyn std::error::Error>>;

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
async fn scripted_user_script_reads_turns_approvals_fragments_and_probe_ids() -> TestResult {
    let temp = tempdir()?;
    let script_path = temp.path().join("script.jsonl");
    let raw = r#"{"version":1,"scenario":"scripted-user-case","expected_final_answer_fragments":["scripted final"],"probe_ids":["probe-root"]}
{"user":{"text":"turn one"},"approval":{"decision":"always_allow","pattern":"cat *"},"probe_ids":["probe-turn-one"]}
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
    assert_eq!(
        script.turns[0].approval,
        Some(ScriptedApprovalDecision::AlwaysAllow {
            pattern: Some("cat *".to_string())
        })
    );
    assert_eq!(script.turns[1].user.text, "turn two");
    assert_eq!(script.turns[1].approval, None);
    Ok(())
}

#[tokio::test]
async fn scripted_user_runner_drives_turn_approval_and_checks_final_answer() -> TestResult {
    if std::env::var_os("MOA_TEST_POSTGRES_URL").is_none() {
        return Ok(());
    }

    let temp = tempdir()?;
    let goal_card_path = temp.path().join("goal_card.md");
    let script_path = temp.path().join("script.jsonl");
    let expectations_path = temp.path().join("expectations.toml");
    tokio::fs::write(
        &goal_card_path,
        b"# Scripted user goal\nRun the approved command and report the result.\n",
    )
    .await?;
    tokio::fs::write(
        &script_path,
        r#"{"version":1,"scenario":"scripted-user-runner","expected_final_answer_fragments":["scripted final"],"probe_ids":["probe-runner"]}
{"user":{"text":"please run the approved command"},"approval":{"decision":"allow_once"},"probe_ids":["probe-approval"]}
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
        permissions: PermissionOverride {
            auto_approve: vec![
                "file_read".to_string(),
                "file_search".to_string(),
                "grep".to_string(),
            ],
            ..PermissionOverride::default()
        },
        ..AgentConfig::default()
    };
    let mut base_config = MoaConfig::default();
    base_config.database.url = moa_test_support::postgres::test_database_url();
    base_config.query_rewrite.enabled = false;

    let provider = Arc::new(ApprovalThenFinalProvider::default());
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
        Some("Approved tool completed with scripted final fragment.")
    );
    assert_eq!(report.score_card.functional.turn_count, 1);
    assert_eq!(report.result.metrics.turn_count, 1);
    assert_eq!(
        provider.seen_user_messages(),
        vec![
            "please run the approved command".to_string(),
            "please run the approved command".to_string(),
        ]
    );
    assert_eq!(
        report
            .events
            .iter()
            .filter(|event| matches!(event, Event::ApprovalDecided { .. }))
            .count(),
        1
    );
    assert!(
        report.events.iter().any(|event| matches!(
            event,
            Event::ToolResult { output, success, .. }
                if *success && output.to_text().contains("scripted approval ok")
        )),
        "approved bash tool result was not persisted"
    );
    Ok(())
}

#[derive(Default)]
struct ApprovalThenFinalProvider {
    seen_user_messages: Mutex<Vec<String>>,
}

impl ApprovalThenFinalProvider {
    fn seen_user_messages(&self) -> Vec<String> {
        self.seen_user_messages
            .lock()
            .expect("seen user messages lock")
            .clone()
    }
}

#[async_trait]
impl LLMProvider for ApprovalThenFinalProvider {
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

    async fn complete(&self, request: CompletionRequest) -> moa_core::Result<CompletionStream> {
        let latest_user = latest_user_message(&request).unwrap_or_default();
        let request_index = {
            let mut seen = self
                .seen_user_messages
                .lock()
                .map_err(|error| moa_core::MoaError::ProviderError(error.to_string()))?;
            seen.push(latest_user);
            seen.len()
        };
        let response = if request_index == 1 {
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some("tool-scripted-approval".to_string()),
                        name: "bash".to_string(),
                        input: json!({ "cmd": "printf 'scripted approval ok'" }),
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
                        && message.content.contains("scripted approval ok")
                }),
                "second request did not include approved tool output: {request:?}"
            );
            CompletionResponse {
                text: "Approved tool completed with scripted final fragment.".to_string(),
                content: vec![CompletionContent::Text(
                    "Approved tool completed with scripted final fragment.".to_string(),
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

fn token_usage(input_tokens: usize, output_tokens: usize) -> TokenUsage {
    TokenUsage {
        input_tokens_uncached: input_tokens,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens,
    }
}
