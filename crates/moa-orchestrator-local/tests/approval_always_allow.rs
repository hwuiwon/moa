//! End-to-end approval-rule tests for `AlwaysAllow` decisions.

mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    ApprovalDecision, BrainOrchestrator, CompletionContent, CompletionRequest, CompletionResponse,
    CompletionStream, ContextMessage, Event, EventRange, EventRecord, LLMProvider, MessageRole,
    MoaConfig, ModelCapabilities, ModelId, Platform, PolicyAction, PolicyScope, Result,
    SessionHandle, SessionId, SessionSignal, SessionStatus, SessionStore, StartSessionRequest,
    StopReason, TokenPricing, TokenUsage, ToolCallContent, ToolCallFormat, ToolInvocation, UserId,
    UserMessage, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_orchestrator_local::LocalOrchestrator;
use moa_providers::ModelRouter;
use moa_session::PostgresSessionStore;
use moa_test_support::postgres::bootstrap_test_db;
use serde_json::json;
use support::orchestrator_contract::{
    ApprovalEventCounts, OrchestratorContractHarness, count_approval_events,
    count_approval_events_in_session, status_sequence,
};
use tempfile::TempDir;
use tokio::time::{Instant, sleep};

const WAIT_TIMEOUT: Duration = Duration::from_secs(20);
const TOOL_PATTERN: &str = "npm test*";
const WORKSPACE: &str = "always-allow-workspace";
const USER: &str = "always-allow-user";

struct LocalHarness<'a> {
    orchestrator: &'a LocalOrchestrator,
}

#[async_trait]
impl OrchestratorContractHarness for LocalHarness<'_> {
    fn harness_name(&self) -> &'static str {
        "local-always-allow"
    }

    fn default_model(&self) -> ModelId {
        ModelId::new(self.orchestrator.model())
    }

    fn platform(&self) -> Platform {
        Platform::Cli
    }

    async fn start_session(&self, req: StartSessionRequest) -> Result<SessionHandle> {
        self.orchestrator.start_session(req).await
    }

    async fn signal(&self, session_id: SessionId, signal: SessionSignal) -> Result<()> {
        self.orchestrator.signal(session_id, signal).await
    }

    async fn session_status(&self, session_id: SessionId) -> Result<Option<SessionStatus>> {
        self.orchestrator
            .get_session(session_id)
            .await
            .map(|session| Some(session.status))
    }

    async fn session_events(&self, session_id: SessionId) -> Result<Vec<EventRecord>> {
        self.orchestrator
            .session_store()
            .get_events(session_id, EventRange::all())
            .await
    }
}

struct ApprovalFixture {
    store: Arc<PostgresSessionStore>,
    _database_url: String,
    _schema_name: String,
    _dir: TempDir,
    config: MoaConfig,
    provider: Arc<ScriptedBashProvider>,
}

impl ApprovalFixture {
    async fn maybe_new(commands: Vec<&str>) -> Result<Option<Self>> {
        if std::env::var_os("MOA_TEST_POSTGRES_URL").is_none() {
            return Ok(None);
        }

        let (store, database_url, schema_name) = bootstrap_test_db().await?.into_parts();
        let store = Arc::new(store);
        let dir = tempfile::tempdir()?;
        let mut config = MoaConfig::default();
        config.query_rewrite.enabled = false;
        config.memory.auto_bootstrap = false;
        config.local.docker_enabled = false;
        config.local.memory_dir = dir.path().join("memory").display().to_string();
        config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
        let provider = Arc::new(ScriptedBashProvider::new(
            config.models.main.clone(),
            commands,
        ));

        Ok(Some(Self {
            store,
            _database_url: database_url,
            _schema_name: schema_name,
            _dir: dir,
            config,
            provider,
        }))
    }

    async fn orchestrator(&self) -> Result<LocalOrchestrator> {
        let tool_router = Arc::new(
            ToolRouter::from_config(&self.config)
                .await?
                .with_rule_store(self.store.clone())
                .with_session_store(self.store.clone()),
        );
        LocalOrchestrator::new(
            self.config.clone(),
            self.store.clone(),
            Arc::new(ModelRouter::new(self.provider.clone(), None)),
            tool_router,
        )
        .await
    }
}

#[derive(Debug)]
struct ScriptedBashProvider {
    model: String,
    commands: Mutex<VecDeque<String>>,
}

impl ScriptedBashProvider {
    fn new(model: String, commands: Vec<&str>) -> Self {
        Self {
            model,
            commands: Mutex::new(commands.into_iter().map(str::to_string).collect()),
        }
    }
}

#[async_trait]
impl LLMProvider for ScriptedBashProvider {
    fn name(&self) -> &str {
        "scripted-bash"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            model_id: self.model.clone().into(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cached_input_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let response = if latest_relevant_role(&request.messages) == Some(MessageRole::User) {
            let command = self
                .commands
                .lock()
                .expect("scripted command lock poisoned")
                .pop_front()
                .expect("scripted provider missing a bash command for this turn");
            CompletionResponse {
                text: String::new(),
                content: vec![CompletionContent::ToolCall(ToolCallContent {
                    invocation: ToolInvocation {
                        id: Some(format!("scripted-{command}")),
                        name: "bash".to_string(),
                        input: json!({
                            "cmd": command,
                            "timeout_secs": 1,
                        }),
                    },
                    provider_metadata: None,
                })],
                stop_reason: StopReason::ToolUse,
                model: self.model.clone().into(),
                usage: token_usage(),
                duration_ms: 1,
                thought_signature: None,
            }
        } else {
            let prompt = last_user_message(&request.messages).unwrap_or_default();
            CompletionResponse {
                text: format!("assistant:{prompt}"),
                content: vec![CompletionContent::Text(format!("assistant:{prompt}"))],
                stop_reason: StopReason::EndTurn,
                model: self.model.clone().into(),
                usage: token_usage(),
                duration_ms: 1,
                thought_signature: None,
            }
        };

        Ok(CompletionStream::from_response(response))
    }
}

#[tokio::test]
async fn always_allow_decision_persists_rule_and_skips_next_matching_call_in_same_session()
-> Result<()> {
    let Some(fixture) = ApprovalFixture::maybe_new(vec!["npm test", "npm test --watch"]).await?
    else {
        return Ok(());
    };
    let orchestrator = fixture.orchestrator().await?;
    let harness = LocalHarness {
        orchestrator: &orchestrator,
    };

    let session = start_session(&harness, WORKSPACE, USER).await?;
    queue_message(&harness, session.session_id, "run npm test").await?;
    let request_id = wait_for_approval_request(&harness, session.session_id).await?;
    decide_always_allow(&harness, session.session_id, request_id).await?;
    wait_for_brain_response_count(&harness, session.session_id, 1).await?;
    wait_for_status(&harness, session.session_id, SessionStatus::Completed).await?;

    let first_events = wait_for_status_sequence(
        &harness,
        session.session_id,
        &[
            SessionStatus::Running,
            SessionStatus::WaitingApproval,
            SessionStatus::Running,
            SessionStatus::Completed,
        ],
    )
    .await?;
    assert_eq!(
        status_sequence(&first_events),
        vec![
            SessionStatus::Running,
            SessionStatus::WaitingApproval,
            SessionStatus::Running,
            SessionStatus::Completed,
        ],
        "approval flow status sequence changed: {:?}",
        event_labels(&first_events)
    );
    assert_eq!(
        count_approval_events_in_session(&harness, session.session_id).await?,
        ApprovalEventCounts {
            requested: 1,
            decided: 1,
        }
    );
    assert_rule_store_contains_one_allow_rule(&fixture.store, WORKSPACE, USER).await?;

    let before_second_turn = last_sequence_num(&first_events);
    queue_message(&harness, session.session_id, "run npm test watch").await?;
    wait_for_brain_response_count(&harness, session.session_id, 2).await?;
    wait_for_status(&harness, session.session_id, SessionStatus::Completed).await?;
    let all_events = wait_for_status_sequence_after(
        &harness,
        session.session_id,
        before_second_turn,
        &[SessionStatus::Running, SessionStatus::Completed],
    )
    .await?;
    let second_turn_events = events_after(&all_events, before_second_turn);

    assert_eq!(
        status_sequence(&second_turn_events),
        vec![SessionStatus::Running, SessionStatus::Completed],
        "auto-approved turn status sequence changed: {:?}",
        event_labels(&second_turn_events)
    );
    assert_eq!(
        count_approval_events(&second_turn_events),
        ApprovalEventCounts {
            requested: 0,
            decided: 0,
        },
        "matching AlwaysAllow rule should skip approval entirely"
    );
    assert_eq!(
        bash_tool_calls(&second_turn_events),
        vec![(before_second_turn + 4, "npm test --watch".to_string())],
        "second matching bash tool call should execute without approval interleaving"
    );

    Ok(())
}

#[tokio::test]
async fn always_allow_rule_persists_across_session_completion_and_new_session_in_same_workspace()
-> Result<()> {
    let Some(fixture) = ApprovalFixture::maybe_new(vec!["npm test", "npm test"]).await? else {
        return Ok(());
    };
    let orchestrator = fixture.orchestrator().await?;
    let harness = LocalHarness {
        orchestrator: &orchestrator,
    };
    establish_always_allow_rule(&harness, &fixture.store).await?;

    let session = start_session(&harness, WORKSPACE, USER).await?;
    queue_message(
        &harness,
        session.session_id,
        "run npm test in a fresh session",
    )
    .await?;
    wait_for_brain_response_count(&harness, session.session_id, 1).await?;
    wait_for_status(&harness, session.session_id, SessionStatus::Completed).await?;
    let events = wait_for_status_sequence(
        &harness,
        session.session_id,
        &[SessionStatus::Running, SessionStatus::Completed],
    )
    .await?;

    assert_eq!(
        status_sequence(&events),
        vec![SessionStatus::Running, SessionStatus::Completed],
        "auto-approved fresh-session status sequence changed: {:?}",
        event_labels(&events)
    );
    assert_eq!(
        count_approval_events(&events),
        ApprovalEventCounts {
            requested: 0,
            decided: 0,
        }
    );
    assert_eq!(
        bash_tool_calls(&events),
        vec![(4, "npm test".to_string())],
        "fresh session should execute matching bash command at the first tool-call slot"
    );
    assert_rule_store_contains_one_allow_rule(&fixture.store, WORKSPACE, USER).await?;

    Ok(())
}

#[tokio::test]
async fn always_allow_rule_survives_local_orchestrator_restart() -> Result<()> {
    let Some(fixture) = ApprovalFixture::maybe_new(vec!["npm test", "npm test"]).await? else {
        return Ok(());
    };
    let orchestrator = fixture.orchestrator().await?;
    {
        let harness = LocalHarness {
            orchestrator: &orchestrator,
        };
        establish_always_allow_rule(&harness, &fixture.store).await?;
    }
    drop(orchestrator);

    let restarted = fixture.orchestrator().await?;
    let restarted_harness = LocalHarness {
        orchestrator: &restarted,
    };
    let session = start_session(&restarted_harness, WORKSPACE, USER).await?;
    queue_message(
        &restarted_harness,
        session.session_id,
        "run npm test after restart",
    )
    .await?;
    wait_for_brain_response_count(&restarted_harness, session.session_id, 1).await?;
    wait_for_status(
        &restarted_harness,
        session.session_id,
        SessionStatus::Completed,
    )
    .await?;
    let events = wait_for_status_sequence(
        &restarted_harness,
        session.session_id,
        &[SessionStatus::Running, SessionStatus::Completed],
    )
    .await?;

    assert_eq!(
        status_sequence(&events),
        vec![SessionStatus::Running, SessionStatus::Completed],
        "auto-approved post-restart status sequence changed: {:?}",
        event_labels(&events)
    );
    assert_eq!(
        count_approval_events(&events),
        ApprovalEventCounts {
            requested: 0,
            decided: 0,
        }
    );
    assert_eq!(
        bash_tool_calls(&events),
        vec![(4, "npm test".to_string())],
        "post-restart session should execute matching bash command at the first tool-call slot"
    );
    assert_rule_store_contains_one_allow_rule(&fixture.store, WORKSPACE, USER).await?;

    Ok(())
}

#[tokio::test]
async fn always_allow_with_pattern_mismatch_still_prompts_for_approval() -> Result<()> {
    let Some(fixture) = ApprovalFixture::maybe_new(vec!["npm test", "rm -rf node_modules"]).await?
    else {
        return Ok(());
    };
    let orchestrator = fixture.orchestrator().await?;
    let harness = LocalHarness {
        orchestrator: &orchestrator,
    };
    establish_always_allow_rule(&harness, &fixture.store).await?;

    let session = start_session(&harness, WORKSPACE, USER).await?;
    queue_message(&harness, session.session_id, "try a different command").await?;
    let request_id = wait_for_approval_request(&harness, session.session_id).await?;
    wait_for_status(&harness, session.session_id, SessionStatus::WaitingApproval).await?;
    let events = wait_for_status_sequence(
        &harness,
        session.session_id,
        &[SessionStatus::Running, SessionStatus::WaitingApproval],
    )
    .await?;

    assert_eq!(
        status_sequence(&events),
        vec![SessionStatus::Running, SessionStatus::WaitingApproval],
        "mismatch should stop at WaitingApproval: {:?}",
        event_labels(&events)
    );
    assert_eq!(
        count_approval_events(&events),
        ApprovalEventCounts {
            requested: 1,
            decided: 0,
        },
        "mismatched command should emit exactly one approval request and no decision"
    );
    assert_eq!(
        bash_tool_calls(&events),
        vec![(4, "rm -rf node_modules".to_string())],
        "mismatched bash command should be recorded before the approval prompt"
    );

    deny_request(&harness, session.session_id, request_id).await?;
    wait_for_brain_response_count(&harness, session.session_id, 1).await?;
    wait_for_status(&harness, session.session_id, SessionStatus::Completed).await?;
    let final_events = wait_for_status_sequence(
        &harness,
        session.session_id,
        &[
            SessionStatus::Running,
            SessionStatus::WaitingApproval,
            SessionStatus::Running,
            SessionStatus::Completed,
        ],
    )
    .await?;
    assert_eq!(
        count_approval_events(&final_events),
        ApprovalEventCounts {
            requested: 1,
            decided: 1,
        },
        "denying the mismatched command should add exactly one approval decision"
    );

    Ok(())
}

#[tokio::test]
async fn always_allow_with_shell_chaining_bypass_attempt_still_prompts_for_approval() -> Result<()>
{
    let Some(fixture) =
        ApprovalFixture::maybe_new(vec!["npm test", "npm test && rm -rf /"]).await?
    else {
        return Ok(());
    };
    let orchestrator = fixture.orchestrator().await?;
    let harness = LocalHarness {
        orchestrator: &orchestrator,
    };
    establish_always_allow_rule(&harness, &fixture.store).await?;

    let session = start_session(&harness, WORKSPACE, USER).await?;
    queue_message(&harness, session.session_id, "try chained npm test").await?;
    let request_id = wait_for_approval_request(&harness, session.session_id).await?;
    wait_for_status(&harness, session.session_id, SessionStatus::WaitingApproval).await?;
    let events = wait_for_status_sequence(
        &harness,
        session.session_id,
        &[SessionStatus::Running, SessionStatus::WaitingApproval],
    )
    .await?;

    assert_eq!(
        status_sequence(&events),
        vec![SessionStatus::Running, SessionStatus::WaitingApproval],
        "chained command should stop at WaitingApproval: {:?}",
        event_labels(&events)
    );
    assert_eq!(
        count_approval_events(&events),
        ApprovalEventCounts {
            requested: 1,
            decided: 0,
        },
        "shell chaining bypass attempt should emit exactly one approval request and no decision"
    );
    assert_eq!(
        bash_tool_calls(&events),
        vec![(4, "npm test && rm -rf /".to_string())],
        "chained bash command should be recorded before the approval prompt"
    );

    deny_request(&harness, session.session_id, request_id).await?;
    wait_for_brain_response_count(&harness, session.session_id, 1).await?;
    wait_for_status(&harness, session.session_id, SessionStatus::Completed).await?;
    let final_events = wait_for_status_sequence(
        &harness,
        session.session_id,
        &[
            SessionStatus::Running,
            SessionStatus::WaitingApproval,
            SessionStatus::Running,
            SessionStatus::Completed,
        ],
    )
    .await?;
    assert_eq!(
        count_approval_events(&final_events),
        ApprovalEventCounts {
            requested: 1,
            decided: 1,
        },
        "denying the chained command should add exactly one approval decision"
    );

    Ok(())
}

async fn establish_always_allow_rule(
    harness: &LocalHarness<'_>,
    store: &PostgresSessionStore,
) -> Result<()> {
    let session = start_session(harness, WORKSPACE, USER).await?;
    queue_message(harness, session.session_id, "run npm test").await?;
    let request_id = wait_for_approval_request(harness, session.session_id).await?;
    decide_always_allow(harness, session.session_id, request_id).await?;
    wait_for_brain_response_count(harness, session.session_id, 1).await?;
    wait_for_status(harness, session.session_id, SessionStatus::Completed).await?;
    let events = wait_for_status_sequence(
        harness,
        session.session_id,
        &[
            SessionStatus::Running,
            SessionStatus::WaitingApproval,
            SessionStatus::Running,
            SessionStatus::Completed,
        ],
    )
    .await?;

    assert_eq!(
        status_sequence(&events),
        vec![
            SessionStatus::Running,
            SessionStatus::WaitingApproval,
            SessionStatus::Running,
            SessionStatus::Completed,
        ],
        "setup approval flow status sequence changed: {:?}",
        event_labels(&events)
    );
    assert_eq!(
        count_approval_events(&events),
        ApprovalEventCounts {
            requested: 1,
            decided: 1,
        }
    );
    assert_rule_store_contains_one_allow_rule(store, WORKSPACE, USER).await?;
    Ok(())
}

async fn start_session(
    harness: &LocalHarness<'_>,
    workspace: &str,
    user: &str,
) -> Result<SessionHandle> {
    harness
        .start_session(StartSessionRequest {
            workspace_id: WorkspaceId::new(workspace),
            user_id: UserId::new(user),
            platform: Platform::Cli,
            model: harness.default_model(),
            initial_message: None,
            title: None,
            parent_session_id: None,
        })
        .await
}

async fn queue_message(
    harness: &LocalHarness<'_>,
    session_id: SessionId,
    text: &str,
) -> Result<()> {
    harness
        .signal(
            session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: text.to_string(),
                attachments: Vec::new(),
            }),
        )
        .await
}

async fn decide_always_allow(
    harness: &LocalHarness<'_>,
    session_id: SessionId,
    request_id: uuid::Uuid,
) -> Result<()> {
    harness
        .signal(
            session_id,
            SessionSignal::ApprovalDecided {
                request_id,
                decision: ApprovalDecision::AlwaysAllow {
                    pattern: TOOL_PATTERN.to_string(),
                },
            },
        )
        .await
}

async fn deny_request(
    harness: &LocalHarness<'_>,
    session_id: SessionId,
    request_id: uuid::Uuid,
) -> Result<()> {
    harness
        .signal(
            session_id,
            SessionSignal::ApprovalDecided {
                request_id,
                decision: ApprovalDecision::Deny { reason: None },
            },
        )
        .await
}

async fn wait_for_approval_request(
    harness: &LocalHarness<'_>,
    session_id: SessionId,
) -> Result<uuid::Uuid> {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        let events = harness.session_events(session_id).await?;
        let request_ids = approval_request_ids(&events);
        match request_ids.as_slice() {
            [request_id] => return Ok(*request_id),
            [] => {}
            other => panic!(
                "expected exactly one approval request, got {}: {:?}",
                other.len(),
                event_labels(&events)
            ),
        }

        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for exactly one approval request: {:?}",
                event_labels(&events)
            );
        }

        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_status(
    harness: &LocalHarness<'_>,
    session_id: SessionId,
    expected: SessionStatus,
) -> Result<()> {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        let status = harness.session_status(session_id).await?;
        if status == Some(expected.clone()) {
            return Ok(());
        }

        if Instant::now() >= deadline {
            let events = harness.session_events(session_id).await?;
            panic!(
                "timed out waiting for status {expected:?}; current={status:?}; events={:?}",
                event_labels(&events)
            );
        }

        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_status_sequence(
    harness: &LocalHarness<'_>,
    session_id: SessionId,
    expected: &[SessionStatus],
) -> Result<Vec<EventRecord>> {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        let events = harness.session_events(session_id).await?;
        let observed = status_sequence(&events);
        if observed == expected {
            return Ok(events);
        }
        assert_status_sequence_can_still_match(&observed, expected, &events);

        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for status sequence {expected:?}; observed={observed:?}; events={:?}",
                event_labels(&events)
            );
        }

        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_status_sequence_after(
    harness: &LocalHarness<'_>,
    session_id: SessionId,
    after_sequence_num: u64,
    expected: &[SessionStatus],
) -> Result<Vec<EventRecord>> {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        let events = harness.session_events(session_id).await?;
        let window = events_after(&events, after_sequence_num);
        let observed = status_sequence(&window);
        if observed == expected {
            return Ok(events);
        }
        assert_status_sequence_can_still_match(&observed, expected, &window);

        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for status sequence {expected:?} after seq {after_sequence_num}; observed={observed:?}; events={:?}",
                event_labels(&window)
            );
        }

        sleep(Duration::from_millis(20)).await;
    }
}

fn assert_status_sequence_can_still_match(
    observed: &[SessionStatus],
    expected: &[SessionStatus],
    events: &[EventRecord],
) {
    if observed.len() > expected.len() || !expected.starts_with(observed) {
        panic!(
            "status sequence diverged; expected={expected:?}; observed={observed:?}; events={:?}",
            event_labels(events)
        );
    }
}

async fn wait_for_brain_response_count(
    harness: &LocalHarness<'_>,
    session_id: SessionId,
    expected: usize,
) -> Result<Vec<EventRecord>> {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    loop {
        let events = harness.session_events(session_id).await?;
        let count = events
            .iter()
            .filter(|record| matches!(record.event, Event::BrainResponse { .. }))
            .count();
        if count == expected {
            return Ok(events);
        }
        if count > expected {
            panic!(
                "brain response count exceeded expected {expected}: {:?}",
                event_labels(&events)
            );
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for exactly {expected} brain responses: {:?}",
                event_labels(&events)
            );
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn assert_rule_store_contains_one_allow_rule(
    store: &PostgresSessionStore,
    workspace: &str,
    user: &str,
) -> Result<()> {
    let rules = store
        .list_approval_rules(&WorkspaceId::new(workspace))
        .await?;
    let [rule] = rules.as_slice() else {
        panic!("expected exactly one approval rule, got {rules:#?}");
    };

    assert_eq!(rule.workspace_id, WorkspaceId::new(workspace));
    assert_eq!(rule.tool, "bash");
    assert_eq!(rule.pattern, TOOL_PATTERN);
    assert_eq!(rule.action, PolicyAction::Allow);
    assert_eq!(rule.scope, PolicyScope::Workspace);
    assert_eq!(rule.created_by, UserId::new(user));
    Ok(())
}

fn token_usage() -> TokenUsage {
    TokenUsage {
        input_tokens_uncached: 1,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens: 1,
    }
}

fn latest_relevant_role(messages: &[ContextMessage]) -> Option<MessageRole> {
    messages
        .iter()
        .rev()
        .find(|message| {
            matches!(message.role, MessageRole::User | MessageRole::Tool)
                && !message.content.starts_with("<system-reminder>")
                && !message.content.starts_with("<memory-reminder>")
        })
        .map(|message| message.role.clone())
}

fn last_user_message(messages: &[ContextMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|message| {
            matches!(message.role, MessageRole::User)
                && !message.content.starts_with("<system-reminder>")
                && !message.content.starts_with("<memory-reminder>")
        })
        .map(|message| message.content.clone())
}

fn approval_request_ids(events: &[EventRecord]) -> Vec<uuid::Uuid> {
    events
        .iter()
        .filter_map(|record| match record.event {
            Event::ApprovalRequested { request_id, .. } => Some(request_id),
            _ => None,
        })
        .collect()
}

fn bash_tool_calls(events: &[EventRecord]) -> Vec<(u64, String)> {
    events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall {
                tool_name, input, ..
            } if tool_name == "bash" => Some((
                record.sequence_num,
                input
                    .get("cmd")
                    .and_then(serde_json::Value::as_str)
                    .expect("bash tool input should include string cmd")
                    .to_string(),
            )),
            _ => None,
        })
        .collect()
}

fn last_sequence_num(events: &[EventRecord]) -> u64 {
    events
        .last()
        .map(|record| record.sequence_num)
        .expect("event stream should not be empty")
}

fn events_after(events: &[EventRecord], sequence_num: u64) -> Vec<EventRecord> {
    events
        .iter()
        .filter(|record| record.sequence_num > sequence_num)
        .cloned()
        .collect()
}

fn event_labels(events: &[EventRecord]) -> Vec<String> {
    events
        .iter()
        .map(|record| match &record.event {
            Event::SessionCreated { .. } => {
                format!("{}:SessionCreated", record.sequence_num)
            }
            Event::SessionStatusChanged { from, to } => {
                format!(
                    "{}:SessionStatusChanged({from:?}->{to:?})",
                    record.sequence_num
                )
            }
            Event::UserMessage { text, .. } => {
                format!("{}:UserMessage({text})", record.sequence_num)
            }
            Event::BrainResponse { text, .. } => {
                format!("{}:BrainResponse({text})", record.sequence_num)
            }
            Event::ToolCall {
                tool_name, input, ..
            } => {
                format!("{}:ToolCall({tool_name},{input})", record.sequence_num)
            }
            Event::ToolResult { success, .. } => {
                format!("{}:ToolResult(success={success})", record.sequence_num)
            }
            Event::ToolError {
                tool_name, error, ..
            } => format!("{}:ToolError({tool_name},{error})", record.sequence_num),
            Event::ApprovalRequested {
                request_id,
                tool_name,
                ..
            } => format!(
                "{}:ApprovalRequested({tool_name},{request_id})",
                record.sequence_num
            ),
            Event::ApprovalDecided {
                request_id,
                decision,
                ..
            } => format!(
                "{}:ApprovalDecided({request_id},{decision:?})",
                record.sequence_num
            ),
            other => format!("{}:{}", record.sequence_num, other.type_name()),
        })
        .collect()
}
