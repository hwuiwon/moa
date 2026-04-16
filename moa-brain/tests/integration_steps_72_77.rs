//! End-to-end regression coverage for steps 72 through 77.

use std::collections::HashMap;
use std::sync::Arc;

use moa_brain::{TurnResult, build_default_pipeline_with_tools, run_brain_turn_with_tools};
use moa_core::{
    CompletionRequest, CountedSessionStore, Event, EventRange, EventRecord, ModelCapabilities,
    Result, SessionMeta, SessionStore, TokenPricing, TokenUsage, ToolCallFormat, ToolOutput,
    TurnReplayCounters, TurnReplaySnapshot, UserId, WorkspaceId, scope_turn_replay_counters,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use moa_providers::{ScriptedProvider, ScriptedResponse, anthropic::debug_build_request_body};
use moa_security::ToolPolicies;
use moa_session::TursoSessionStore;
use serde_json::{Value, json};
use tempfile::TempDir;
use uuid::Uuid;

const PARTIAL_READ_HEADER: &str = "[showing lines 118-125 of 260 total in auth.rs]";
const FULL_READ_HEADER: &str = "[showing lines 1-200 of 260 total in auth.rs]";
const FULL_READ_TRUNCATION: &str = "[output truncated to 200 lines; use a narrower range]";
const FILE_READ_DEDUP_PLACEHOLDER: &str = "[file previously read — see latest version below]";
const OLD_SNIPPET: &str = "    let refresh_token = issue_refresh_token(user_id);\n    format!(\"refresh:{refresh_token}\")";
const NEW_SNIPPET: &str = "    let issued_refresh_token = issue_refresh_token(user_id);\n    format!(\"refresh:{issued_refresh_token}\")";

#[tokio::test]
async fn steps_72_77_e2e() -> Result<()> {
    let root = TempDir::new()?;
    let workspace = root.path().join("workspace");
    let state_dir = root.path().join("state");
    tokio::fs::create_dir_all(&workspace).await?;
    tokio::fs::create_dir_all(&state_dir).await?;
    tokio::fs::create_dir_all(workspace.join(".venv")).await?;
    tokio::fs::create_dir_all(workspace.join("ignored_dir")).await?;

    tokio::fs::write(workspace.join("auth.rs"), build_auth_source()).await?;
    tokio::fs::write(
        workspace.join("lib.rs"),
        "pub fn issue_refresh_token(user_id: &str) -> String {\n    format!(\"rt-{user_id}\")\n}\n",
    )
    .await?;
    tokio::fs::write(
        workspace.join(".venv/junk.py"),
        "refresh_token = issue_refresh_token('poison')\n",
    )
    .await?;
    tokio::fs::write(
        workspace.join("ignored_dir/ghost.rs"),
        "pub const GHOST: &str = \"issue_refresh_token\";\n",
    )
    .await?;
    tokio::fs::write(workspace.join(".gitignore"), "ignored_dir/\n").await?;

    let mut config = moa_core::MoaConfig::default();
    config.general.default_model = "claude-sonnet-4-6".to_string();
    config.general.workspace_instructions = Some("Cache integration guidance.\n".repeat(200));
    config.compaction.recent_turns_verbatim = 2;
    config.permissions.auto_approve = vec!["bash".to_string(), "str_replace".to_string()];

    let memory_store = Arc::new(FileMemoryStore::new(&state_dir).await?);
    let session_store =
        Arc::new(TursoSessionStore::new_local(&state_dir.join("sessions.db")).await?);
    let counted_session_store: Arc<dyn SessionStore> =
        Arc::new(CountedSessionStore::new(session_store.clone()));
    let workspace_id = WorkspaceId::new("steps-72-77");
    let session = SessionMeta {
        workspace_id: workspace_id.clone(),
        user_id: UserId::new("integration-test"),
        model: config.general.default_model.clone(),
        ..SessionMeta::default()
    };
    let session_id = session_store.create_session(session.clone()).await?;

    let router = Arc::new(
        ToolRouter::new_local(memory_store.clone(), &workspace)
            .await?
            .with_policies(ToolPolicies::from_config(&config))
            .with_session_store(session_store.clone()),
    );
    router
        .remember_workspace_root(workspace_id.clone(), workspace.clone())
        .await;

    let provider = Arc::new(build_scripted_provider());
    let pipeline = build_default_pipeline_with_tools(
        &config,
        counted_session_store.clone(),
        memory_store,
        extend_tool_schemas(router.tool_schemas()),
    );
    let mut replay_snapshots = Vec::new();

    for prompt in [
        "Turn 1: inspect the target range",
        "Turn 2: search for refresh token usage",
        "Turn 3: read the full auth file",
        "Turn 4: apply the auth fix",
        "Turn 5: reread the full auth file",
        "Turn 6: run a noisy command",
        "Turn 7: summarize the state",
    ] {
        session_store
            .emit_event(
                session_id.clone(),
                Event::UserMessage {
                    text: prompt.to_string(),
                    attachments: Vec::new(),
                },
            )
            .await?;

        let turn_counters = Arc::new(TurnReplayCounters::default());
        let result = scope_turn_replay_counters(
            turn_counters.clone(),
            run_brain_turn_with_tools(
                session_id.clone(),
                counted_session_store.clone(),
                provider.clone(),
                &pipeline,
                Some(router.clone()),
            ),
        )
        .await?;
        replay_snapshots.push(turn_counters.snapshot());

        assert_eq!(
            result,
            TurnResult::Complete,
            "turn should complete: {prompt}"
        );
    }

    let events = session_store
        .get_events(session_id.clone(), EventRange::all())
        .await?;
    let requests = provider.recorded_requests().await;
    let tool_runs = collect_tool_runs(&events);
    let final_session = session_store.get_session(session_id.clone()).await?;

    assert_eq!(
        requests.len(),
        13,
        "expected one tool request and one final response per tool turn, plus one final summary turn"
    );
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(record.event, Event::CacheReport { .. }))
            .count(),
        requests.len(),
        "every provider request should emit one CacheReport event"
    );

    let auth_content = tokio::fs::read_to_string(workspace.join("auth.rs")).await?;
    assert!(!auth_content.contains(OLD_SNIPPET));
    assert!(auth_content.contains(NEW_SNIPPET));
    assert_eq!(auth_content.matches(NEW_SNIPPET).count(), 1);

    let partial_read = tool_runs
        .iter()
        .find(|run| {
            run.name == "file_read"
                && run.input["start_line"] == 118
                && run.input["end_line"] == 125
        })
        .expect("expected partial file_read tool run");
    let partial_text = partial_read.output.to_text();
    assert!(partial_text.contains(PARTIAL_READ_HEADER));
    assert!(partial_text.contains("118\t// filler line 113"));
    assert!(partial_text.contains("121\tpub fn issue_session(user_id: &str) -> String {"));
    assert!(!partial_text.contains("126\t// filler line 120"));

    let grep_run = tool_runs
        .iter()
        .find(|run| run.name == "grep")
        .expect("expected grep tool run");
    let grep_text = grep_run.output.to_text();
    assert!(grep_text.contains("auth.rs"));
    assert!(grep_text.contains("lib.rs"));
    assert!(!grep_text.contains(".venv/junk.py"));
    assert!(!grep_text.contains("ignored_dir/ghost.rs"));

    let full_reads = tool_runs
        .iter()
        .filter(|run| {
            run.name == "file_read"
                && run.input["path"] == "auth.rs"
                && run.input.get("start_line").is_none()
                && run.input.get("end_line").is_none()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        full_reads.len(),
        2,
        "expected exactly two full auth.rs reads"
    );
    assert!(full_reads[0].output.to_text().contains(FULL_READ_HEADER));
    assert!(
        full_reads[0]
            .output
            .to_text()
            .contains(FULL_READ_TRUNCATION)
    );
    assert!(full_reads[1].output.to_text().contains(FULL_READ_HEADER));
    assert!(
        full_reads[1]
            .output
            .to_text()
            .contains(FULL_READ_TRUNCATION)
    );

    let str_replace_run = tool_runs
        .iter()
        .find(|run| run.name == "str_replace")
        .expect("expected str_replace tool run");
    assert!(str_replace_run.success);
    assert!(
        str_replace_run
            .output
            .to_text()
            .contains("replaced 2 lines with 2 lines in auth.rs")
    );

    let bash_run = tool_runs
        .iter()
        .find(|run| run.name == "bash")
        .expect("expected bash tool run");
    let bash_text = bash_run.output.to_text();
    assert!(bash_run.success);
    assert!(
        bash_run.output.truncated,
        "bash output should be marked truncated"
    );
    assert!(bash_text.contains("bash-line-1"));
    assert!(bash_text.contains("bash-line-260"));
    assert!(bash_text.contains("[... 60 lines omitted ...]"));
    assert!(!bash_text.contains("bash-line-140"));

    assert!(
        requests
            .iter()
            .all(|request| !request.cache_breakpoints.is_empty()),
        "all scripted requests should carry cache breakpoints"
    );
    let turn_six_request = requests
        .iter()
        .find(|request| {
            last_user_message(request) == Some("Turn 6: run a noisy command")
                && !request
                    .messages
                    .iter()
                    .any(|message| message.content.contains("bash-line-260"))
        })
        .cloned()
        .expect("expected the pre-bash turn-6 request");
    let turn_six_body = debug_build_request_body(&turn_six_request, false)?;
    let system_cache_marker = turn_six_body["system"].as_array().and_then(|blocks| {
        blocks.iter().find_map(|block| {
            block
                .get("cache_control")
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str)
        })
    });
    assert!(
        system_cache_marker == Some("ephemeral"),
        "expected system cache marker on turn-six request; breakpoints={:?}, tool_count={}, body={turn_six_body:#}",
        turn_six_request.cache_breakpoints,
        turn_six_request.tools.len(),
    );
    let tool_cache_marker = turn_six_body["tools"]
        .as_array()
        .and_then(|blocks| blocks.last())
        .and_then(|block| block.get("cache_control"))
        .and_then(|value| value.get("type"))
        .and_then(Value::as_str);
    assert!(
        tool_cache_marker == Some("ephemeral"),
        "expected tool cache marker on turn-six request; breakpoints={:?}, tool_count={}, body={turn_six_body:#}",
        turn_six_request.cache_breakpoints,
        turn_six_request.tools.len(),
    );

    let turn_seven_request = requests
        .iter()
        .find(|request| last_user_message(request) == Some("Turn 7: summarize the state"))
        .cloned()
        .expect("expected the turn-seven summary request");
    let turn_seven_tool_messages = turn_seven_request
        .messages
        .iter()
        .filter(|message| message.role == moa_core::MessageRole::Tool)
        .collect::<Vec<_>>();
    assert!(
        turn_seven_tool_messages
            .iter()
            .any(|message| message.content.contains(FILE_READ_DEDUP_PLACEHOLDER)),
        "older full-file reads should be replaced with the dedup placeholder"
    );
    assert_eq!(
        turn_seven_tool_messages
            .iter()
            .filter(|message| message.content.contains(FULL_READ_HEADER))
            .count(),
        1,
        "only the latest full-file auth.rs read should remain verbatim in replayed history"
    );
    assert!(
        turn_seven_tool_messages
            .iter()
            .any(|message| message.content.contains(PARTIAL_READ_HEADER)),
        "partial reads should never be deduplicated"
    );
    assert!(
        turn_seven_tool_messages.iter().any(|message| {
            message.content.contains(FILE_READ_DEDUP_PLACEHOLDER) && message.tool_use_id.is_some()
        }),
        "deduplicated tool results must preserve tool_use_id for provider replay"
    );
    assert!(
        final_session.total_input_tokens_cache_read > 0,
        "session should accumulate non-zero cache-read tokens"
    );
    assert!(
        final_session.cache_hit_rate() > 0.0,
        "session cache hit rate should be non-zero"
    );
    assert_replay_growth(&replay_snapshots);

    Ok(())
}

fn build_scripted_provider() -> ScriptedProvider {
    ScriptedProvider::new(scripted_capabilities())
        .push_response(ScriptedResponse::tool_call(
            "file_read",
            json!({ "path": "auth.rs", "start_line": 118, "end_line": 125 }),
            "tc_001",
        ))
        .push_response(ScriptedResponse::text("Turn 1 complete."))
        .push_response(
            ScriptedResponse::tool_call(
                "grep",
                json!({ "pattern": "issue_refresh_token", "path": ".", "literal": true }),
                "tc_002",
            )
            .with_usage(cached_usage(72, 24)),
        )
        .push_response(ScriptedResponse::text("Turn 2 complete.").with_usage(cached_usage(80, 32)))
        .push_response(
            ScriptedResponse::tool_call("file_read", json!({ "path": "auth.rs" }), "tc_003")
                .with_usage(cached_usage(96, 40)),
        )
        .push_response(ScriptedResponse::text("Turn 3 complete.").with_usage(cached_usage(104, 48)))
        .push_response(
            ScriptedResponse::tool_call(
                "str_replace",
                json!({
                    "path": "auth.rs",
                    "old_str": OLD_SNIPPET,
                    "new_str": NEW_SNIPPET,
                }),
                "tc_004",
            )
            .with_usage(cached_usage(112, 56)),
        )
        .push_response(ScriptedResponse::text("Turn 4 complete.").with_usage(cached_usage(120, 64)))
        .push_response(
            ScriptedResponse::tool_call("file_read", json!({ "path": "auth.rs" }), "tc_005")
                .with_usage(cached_usage(128, 72)),
        )
        .push_response(ScriptedResponse::text("Turn 5 complete.").with_usage(cached_usage(136, 80)))
        .push_response(
            ScriptedResponse::tool_call(
                "bash",
                json!({ "cmd": "for i in $(seq 1 260); do echo bash-line-$i; done" }),
                "tc_006",
            )
            .with_usage(cached_usage(144, 88)),
        )
        .push_response(ScriptedResponse::text("Turn 6 complete.").with_usage(cached_usage(152, 96)))
        .push_response(
            ScriptedResponse::text("Turn 7 complete.").with_usage(cached_usage(160, 104)),
        )
}

fn cached_usage(total_input_tokens: usize, cache_read_tokens: usize) -> TokenUsage {
    TokenUsage {
        input_tokens_uncached: total_input_tokens.saturating_sub(cache_read_tokens),
        input_tokens_cache_write: 0,
        input_tokens_cache_read: cache_read_tokens,
        output_tokens: 0,
    }
}

fn scripted_capabilities() -> ModelCapabilities {
    ModelCapabilities {
        model_id: "claude-sonnet-4-6".to_string(),
        context_window: 200_000,
        max_output: 8_192,
        supports_tools: true,
        supports_vision: false,
        supports_prefix_caching: true,
        cache_ttl: None,
        tool_call_format: ToolCallFormat::Anthropic,
        pricing: TokenPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.3),
        },
        native_tools: Vec::new(),
    }
}

fn extend_tool_schemas(mut schemas: Vec<Value>) -> Vec<Value> {
    for index in 0..16 {
        schemas.push(json!({
            "name": format!("dummy_tool_{index}"),
            "description": format!("Cache padding tool {index} with a longer description to keep the tool prefix large."),
            "input_schema": {
                "type": "object",
                "properties": {
                    "value": { "type": "string", "description": "unused" }
                }
            }
        }));
    }
    schemas
}

fn build_auth_source() -> String {
    let mut lines = Vec::with_capacity(260);
    for index in 1..=117 {
        lines.push(format!("// filler line {index}"));
    }
    lines.push("// filler line 113".to_string());
    lines.push("// filler line 114".to_string());
    lines.push("// filler line 115".to_string());
    lines.push("pub fn issue_session(user_id: &str) -> String {".to_string());
    lines.push("    let refresh_token = issue_refresh_token(user_id);".to_string());
    lines.push("    format!(\"refresh:{refresh_token}\")".to_string());
    lines.push("}".to_string());
    lines.push("// filler line 120".to_string());
    for index in 126..=260 {
        lines.push(format!("// filler line {index}"));
    }
    lines.join("\n")
}

fn last_user_message(request: &CompletionRequest) -> Option<&str> {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == moa_core::MessageRole::User)
        .map(|message| message.content.as_str())
}

#[derive(Debug, Clone)]
struct ToolRun {
    name: String,
    input: Value,
    output: ToolOutput,
    success: bool,
}

fn collect_tool_runs(events: &[EventRecord]) -> Vec<ToolRun> {
    let mut calls = HashMap::<Uuid, (String, Value)>::new();
    let mut runs = Vec::new();

    for record in events {
        match &record.event {
            Event::ToolCall {
                tool_id,
                tool_name,
                input,
                ..
            } => {
                calls.insert(*tool_id, (tool_name.clone(), input.clone()));
            }
            Event::ToolResult {
                tool_id,
                output,
                success,
                ..
            } => {
                if let Some((name, input)) = calls.get(tool_id) {
                    runs.push(ToolRun {
                        name: name.clone(),
                        input: input.clone(),
                        output: output.clone(),
                        success: *success,
                    });
                }
            }
            _ => {}
        }
    }

    runs
}

fn assert_replay_growth(replay_snapshots: &[TurnReplaySnapshot]) {
    assert_eq!(
        replay_snapshots.len(),
        7,
        "expected one replay snapshot per scripted turn"
    );
    assert!(
        replay_snapshots[0].events_replayed > 0,
        "first turn should replay at least one event"
    );
    assert!(
        replay_snapshots[0].get_events_calls > 0,
        "first turn should call get_events at least once"
    );
    assert!(
        replay_snapshots[6].events_replayed > replay_snapshots[0].events_replayed,
        "later turns should replay more events than early turns"
    );
    assert!(
        replay_snapshots[6].events_bytes > replay_snapshots[0].events_bytes,
        "later turns should deserialize more event bytes than early turns"
    );
}
