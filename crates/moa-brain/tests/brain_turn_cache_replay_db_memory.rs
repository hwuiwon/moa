//! End-to-end regression coverage for brain turn cache and replay behavior.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use moa_brain::{
    BrainTurnRequest, GraphMemoryPipelineOptions, TurnResult,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions, run_brain_turn,
};
use moa_core::{
    error::Result, events::Event, session_replay::TurnReplayCounters,
    session_replay::TurnReplaySnapshot, session_replay::scope_turn_replay_counters,
    traits::SessionStore, types::completion::CompletionRequest, types::completion::TokenUsage,
    types::contact::ContactId, types::contact::ContactRef,
    types::contact::ContactVerificationState, types::contact::SessionActorRef,
    types::events_stream::EventRange, types::events_stream::EventRecord,
    types::identifiers::TenantId, types::model::ModelCapabilities, types::model::TokenPricing,
    types::model::ToolCallFormat, types::session::SessionMeta, types::tools::ToolOutput,
};
use moa_hands::ToolRouter;
use moa_providers::{ScriptedProvider, ScriptedResponse, debug_build_anthropic_request_body};
use moa_security::ActionPolicies;
use moa_test_support::postgres::bootstrap_test_db;
use serde_json::{Value, json};
use tempfile::TempDir;
use tracing::Id;
use tracing::Subscriber;
use tracing::span::Attributes;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

const PARTIAL_READ_HEADER: &str = "[showing lines 118-125 of 260 total in auth.rs]";
const FULL_READ_HEADER: &str = "[showing lines 1-200 of 260 total in auth.rs]";
const FULL_READ_TRUNCATION: &str = "[output truncated to 200 lines; use a narrower range]";
const FILE_READ_DEDUP_PLACEHOLDER: &str = "[file previously read — see latest version below]";
const OLD_SNIPPET: &str = "    let refresh_token = issue_refresh_token(user_id);\n    format!(\"refresh:{refresh_token}\")";
const NEW_SNIPPET: &str = "    let issued_refresh_token = issue_refresh_token(user_id);\n    format!(\"refresh:{issued_refresh_token}\")";

async fn allow_cache_replay_bash(
    store: &moa_session::PostgresSessionStore,
    tenant_id: TenantId,
) -> Result<()> {
    // This replay test needs the deterministic bash fixture to execute, so it
    // opts into Allow without weakening the production AdminReview default.
    store
        .upsert_action_policy_rule(moa_core::types::action_policy::ActionPolicyRule {
            id: uuid::Uuid::now_v7(),
            scope: moa_core::types::action_policy::ActionRuleScope::Tenant { tenant_id },
            tool: "bash".to_string(),
            pattern: "python3 -c *".to_string(),
            effect: moa_core::types::action_policy::ActionPolicyEffect::Allow,
            reason: Some("cache replay test bash opt-in".to_string()),
            created_by: moa_core::types::identifiers::UserId::new("cache-replay-test"),
            created_at: moa_test_support::fixtures::pg_now(),
        })
        .await
}

#[tokio::test]
async fn brain_turn_cache_replay_db_memory() -> Result<()> {
    let span_recorder = SpanRecorder::default();
    let subscriber = tracing_subscriber::registry().with(span_recorder.clone());
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let root = TempDir::new()?;
    let workspace = root.path().join("workspace");
    tokio::fs::create_dir_all(&workspace).await?;
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

    let mut config = moa_config::MoaConfig::default();
    config.models.main = "claude-sonnet-4-6".to_string();
    config.general.workspace_instructions = Some("Cache integration guidance.\n".repeat(200));
    config.compaction.recent_turns_verbatim = 2;

    let test_db = bootstrap_test_db().await?;
    let graph_pool = test_db.store().pool().clone();
    let session_store = Arc::new(test_db.store().clone());
    let dyn_session_store: Arc<dyn SessionStore> = session_store.clone();
    let tenant_id = TenantId::from(uuid::Uuid::now_v7());
    let contact_id = ContactId::new();
    let session = SessionMeta {
        tenant_id,
        contact: Some(contact_ref(tenant_id, contact_id)),
        created_by: Some(SessionActorRef::Contact { id: contact_id }),
        model: config.models.main.clone().into(),
        ..SessionMeta::default()
    };
    let session_id = session_store.create_session(session.clone()).await?;
    allow_cache_replay_bash(&session_store, tenant_id).await?;

    let router = Arc::new(
        ToolRouter::new_local(&workspace)
            .await?
            .with_policies(ActionPolicies::from_config(&config)?)
            .with_rule_store(session_store.clone())
            .with_session_store(session_store.clone()),
    );
    router
        .remember_workspace_root(tenant_id, workspace.clone())
        .await;

    let provider = Arc::new(build_scripted_provider());
    let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
        &config,
        dyn_session_store.clone(),
        GraphMemoryPipelineOptions {
            graph_pool,
            kms: Arc::new(moa_crypto::LocalKmsProvider::new()),
            shared_graph_memory_retriever: None,
            retrieval_embedder: None,
            shared_skill_injector: None,
            segment_store: Some(session_store.clone()),
            compaction_llm_provider: None,
            query_rewrite_llm_provider: None,
            identity_prompt_override: None,
            tool_schemas: extend_tool_schemas(router.tool_schemas()),
            lineage: Arc::new(moa_core::traits::NullLineageHandle),
        },
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
                session_id,
                Event::UserMessage {
                    text: prompt.to_string(),
                    attachments: Vec::new(),
                },
            )
            .await?;

        let turn_counters = Arc::new(TurnReplayCounters::default());
        let result = scope_turn_replay_counters(
            turn_counters.clone(),
            run_brain_turn(BrainTurnRequest {
                identity: test_identity(tenant_id),
                session_id,
                session_store: dyn_session_store.clone(),
                llm_provider: provider.clone(),
                pipeline: &pipeline,
                tool_router: Some(router.clone()),
            }),
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
        .get_events(session_id, EventRange::all())
        .await?;
    let requests = provider.recorded_requests();
    let tool_runs = collect_tool_runs(&events);
    let final_session = session_store.get_session(session_id).await?;
    let final_session_summary = session_store.get_session_summary(session_id).await?;
    let final_snapshot = session_store.get_snapshot(session_id).await?;

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
    let str_replace_text = str_replace_run.output.to_text();
    assert!(str_replace_text.starts_with("--- a/auth.rs\n+++ b/auth.rs\n"));
    assert!(str_replace_text.contains("@@"));
    assert!(!str_replace_text.contains("replaced 2 lines with 2 lines in auth.rs"));

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
    assert!(
        bash_run.output.artifact.is_some(),
        "large bash output should be stored behind an artifact reference"
    );
    assert!(bash_text.contains("bash-line-1"));
    assert!(bash_text.contains("bash-line-260"));
    assert!(bash_text.contains("[full output stored separately:"));
    assert!(!bash_text.contains("bash-line-140"));
    assert!((bash_text.chars().count() as u32).div_ceil(4) <= 1_024);

    assert!(
        requests
            .iter()
            .all(|request| static_prefix_message_count(request) > 0),
        "all scripted requests should carry stable system prefix sections"
    );
    let first_prefix = stable_prefix_bytes(&requests[0])?;
    let last_prefix = stable_prefix_bytes(
        requests
            .last()
            .expect("scripted provider should record at least one request"),
    )?;
    assert_eq!(
        first_prefix, last_prefix,
        "stable prefix bytes should remain identical across turns"
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
    let runtime_context_message = turn_six_request
        .messages
        .iter()
        .find(|message| message.content.contains("<system-reminder>"))
        .expect("expected a runtime context reminder message");
    assert!(runtime_context_message.content.contains(&format!(
        "Current working directory: {}",
        workspace.display()
    )));
    let turn_six_body = debug_build_anthropic_request_body(&turn_six_request, false)?;
    assert_eq!(
        turn_six_body
            .get("cache_control")
            .and_then(|value| value.get("type"))
            .and_then(Value::as_str),
        Some("ephemeral"),
        "Anthropic caching should be enabled by provider-owned top-level cache_control; body={turn_six_body:#}"
    );
    assert_eq!(
        provider_stable_prefix_cache_control_count(&turn_six_body),
        1,
        "Anthropic provider should mark exactly one stable-prefix boundary"
    );
    assert_eq!(
        dynamic_cache_control_count(&turn_six_body),
        1,
        "exactly one message-region marker: the moving frozen-history boundary \
         breakpoint that lets replayed history cache-read across turns"
    );

    let turn_seven_request = requests
        .iter()
        .find(|request| last_user_message(request) == Some("Turn 7: summarize the state"))
        .cloned()
        .expect("expected the turn-seven summary request");
    let turn_seven_tool_messages = turn_seven_request
        .messages
        .iter()
        .filter(|message| message.role == moa_core::types::context::MessageRole::Tool)
        .collect::<Vec<_>>();
    // Turn 5 re-read the file after turn 4 changed it. Between checkpoints,
    // already-compiled history is append-only: the stale older read keeps its
    // bytes (so the provider prompt cache keeps matching the frozen prefix)
    // and the superseding read carries the stale marker. No checkpoint fires
    // in this session, so no dedup placeholder appears.
    assert!(
        turn_seven_tool_messages
            .iter()
            .all(|message| !message.content.contains(FILE_READ_DEDUP_PLACEHOLDER)),
        "no checkpoint fired, so the stale read must keep its bytes"
    );
    assert_eq!(
        turn_seven_tool_messages
            .iter()
            .filter(|message| message.content.contains(FULL_READ_HEADER))
            .count(),
        2,
        "both full auth.rs reads stay verbatim between checkpoints"
    );
    assert_eq!(
        turn_seven_tool_messages
            .iter()
            .filter(|message| message.content.contains("supersedes_stale_read=\"true\""))
            .count(),
        1,
        "the changed-content re-read carries the supersession marker"
    );
    assert!(
        turn_seven_tool_messages
            .iter()
            .any(|message| message.content.contains(PARTIAL_READ_HEADER)),
        "partial reads should never be deduplicated"
    );
    let bash_artifact_message = turn_seven_tool_messages
        .iter()
        .find(|message| message.content.contains("artifact=\"stored\""))
        .expect("expected artifact-backed bash output in replayed history");
    assert!(bash_artifact_message.content.contains("tool_result_read"));
    assert!(bash_artifact_message.content.contains("tool_result_search"));
    assert!(!bash_artifact_message.content.contains("bash-line-140"));
    assert!(
        final_session.total_input_tokens_cache_read > 0,
        "session should accumulate non-zero cache-read tokens"
    );
    assert!(
        final_session_summary.cache_hit_rate > 0.0,
        "session cache hit rate should be non-zero"
    );
    let final_snapshot = final_snapshot.expect("expected a persisted context snapshot");
    assert!(
        final_snapshot.last_sequence_num > 0,
        "snapshot should advance past the initial event once incremental replay is active"
    );
    assert!(
        !final_snapshot.messages.is_empty(),
        "snapshot should retain compiled history messages for reuse"
    );
    assert_replay_flattening(&replay_snapshots);
    assert_turn_latency_spans(&span_recorder);

    Ok(())
}

fn test_identity(tenant_id: TenantId) -> moa_core::traits::Identity {
    moa_core::traits::Identity {
        identity_type: moa_core::traits::IdentityType::Operator,
        id: uuid::Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c414),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
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
                json!({
                    "cmd": "python3 -c \"for i in range(1, 261): print(f'bash-line-{i}-' + ('x' * 120))\""
                }),
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
        model_id: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
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
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
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
        .find(|message| message.role == moa_core::types::context::MessageRole::User)
        .map(|message| message.content.as_str())
}

fn stable_prefix_bytes(request: &CompletionRequest) -> Result<Vec<u8>> {
    let stable_message_count = static_prefix_message_count(request);
    serde_json::to_vec(&json!({
        "messages": request.messages[..stable_message_count],
        "tools": request.tools,
    }))
    .map_err(Into::into)
}

fn static_prefix_message_count(request: &CompletionRequest) -> usize {
    request
        .messages
        .iter()
        .take_while(|message| message.role == moa_core::types::context::MessageRole::System)
        .count()
}

fn provider_stable_prefix_cache_control_count(body: &Value) -> usize {
    let mut count = 0;

    if let Some(system) = body["system"].as_array() {
        for block in system {
            if block.get("cache_control").is_some() {
                count += 1;
            }
        }
    }

    count
}

fn dynamic_cache_control_count(body: &Value) -> usize {
    let mut count = 0;

    if let Some(tools) = body["tools"].as_array() {
        for tool in tools {
            if tool.get("cache_control").is_some() {
                count += 1;
            }
        }
    }

    if let Some(messages) = body["messages"].as_array() {
        for message in messages {
            if let Some(content) = message["content"].as_array() {
                for block in content {
                    if block.get("cache_control").is_some() {
                        count += 1;
                    }
                }
            }
        }
    }

    count
}

#[derive(Debug, Clone)]
struct ToolRun {
    name: String,
    input: Value,
    output: ToolOutput,
    success: bool,
}

fn collect_tool_runs(events: &[EventRecord]) -> Vec<ToolRun> {
    let mut calls = HashMap::<moa_core::types::identifiers::ToolCallId, (String, Value)>::new();
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

fn assert_replay_flattening(replay_snapshots: &[TurnReplaySnapshot]) {
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
        replay_snapshots[6].events_replayed < replay_snapshots[5].events_replayed,
        "final turn should reuse the persisted snapshot instead of replaying the full log"
    );
    assert!(
        replay_snapshots[6].get_events_calls < replay_snapshots[5].get_events_calls,
        "snapshot reuse should reduce event-log reads on the final turn"
    );
    assert!(
        replay_snapshots
            .iter()
            .all(|snapshot| !snapshot.pipeline_compile_duration.is_zero()),
        "each turn should record pipeline compile duration"
    );
}

fn assert_turn_latency_spans(span_recorder: &SpanRecorder) {
    for name in [
        "pipeline_compile",
        "llm_call",
        "tool_dispatch",
        "event_persist",
    ] {
        assert!(
            span_recorder.count(name) >= 7,
            "expected at least one {name} span per scripted turn, got {}",
            span_recorder.count(name),
        );
    }
}

fn contact_ref(tenant_id: TenantId, contact_id: ContactId) -> ContactRef {
    ContactRef {
        contact_id,
        tenant_id,
        state: ContactVerificationState::Verified,
        canonical_contact_id: None,
        linked_contact_ids: Vec::new(),
        scopes: Vec::new(),
        permissions: serde_json::Value::Null,
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    }
}

#[derive(Clone, Default)]
struct SpanRecorder {
    span_names: Arc<Mutex<Vec<String>>>,
}

impl SpanRecorder {
    fn count(&self, name: &str) -> usize {
        self.span_names
            .lock()
            .expect("span recorder lock should succeed")
            .iter()
            .filter(|span_name| span_name.as_str() == name)
            .count()
    }
}

impl<S> Layer<S> for SpanRecorder
where
    S: Subscriber,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
        self.span_names
            .lock()
            .expect("span recorder lock should succeed")
            .push(attrs.metadata().name().to_string());
    }
}
