//! Shared single-session chat runtime used by the TUI and `moa exec`.

use std::env;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use moa_brain::build_default_pipeline_with_tools;
use moa_core::{
    ApprovalDecision, ApprovalRequest, CompletionContent, Event, LLMProvider, MoaConfig, MoaError,
    Platform, Result, SessionId, SessionMeta, SessionStatus, SessionStore, StopReason,
    ToolInvocation, UserId, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_hands::tools::file_read::resolve_sandbox_path;
use moa_memory::FileMemoryStore;
use moa_providers::AnthropicProvider;
use moa_session::TursoSessionStore;
use serde_json::Value;
use tokio::fs;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Runtime event streamed back to the CLI or TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeEvent {
    /// A new assistant message started streaming.
    AssistantStarted,
    /// One streamed character from the assistant.
    AssistantDelta(char),
    /// A streamed assistant message finished.
    AssistantFinished {
        /// Final text for the completed assistant message.
        text: String,
    },
    /// A tool card should be inserted or updated.
    ToolUpdate(ToolUpdate),
    /// Human approval is required before a tool can execute.
    ApprovalRequested(ApprovalPrompt),
    /// Session token totals changed.
    UsageUpdated {
        /// Aggregate input + output token count for the current session.
        total_tokens: usize,
    },
    /// Informational status line from the runtime.
    Notice(String),
    /// The turn finished without more pending work.
    TurnCompleted,
    /// The runtime hit an error while processing the turn.
    Error(String),
}

/// Control message sent from the CLI or TUI back into a running turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeCommand {
    /// Resolves the current approval request.
    Approval(ApprovalDecision),
}

/// Approval prompt plus the default persistent-rule pattern suggested by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalPrompt {
    /// Approval request displayed to the user.
    pub request: ApprovalRequest,
    /// Suggested rule pattern when the user chooses "Always Allow".
    pub pattern: String,
    /// Structured parameters rendered by the approval widget.
    pub parameters: Vec<ApprovalField>,
    /// Optional file diffs rendered inline and in the full-screen diff viewer.
    pub file_diffs: Vec<ApprovalFileDiff>,
}

/// One rendered approval field shown as `Label: Value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalField {
    /// Field label.
    pub label: String,
    /// Human-readable value.
    pub value: String,
}

/// A text file diff attached to a pending approval request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalFileDiff {
    /// Logical file path shown to the user.
    pub path: String,
    /// Existing file contents before the tool executes.
    pub before: String,
    /// Proposed file contents after the tool executes.
    pub after: String,
    /// Optional syntax hint derived from the file extension.
    pub language_hint: Option<String>,
}

/// Inline tool card state rendered by the TUI and surfaced by `moa exec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCardStatus {
    /// The tool call is known but not yet executed.
    Pending,
    /// The tool is waiting for approval.
    WaitingApproval,
    /// The tool is actively executing.
    Running,
    /// The tool completed successfully.
    Succeeded,
    /// The tool failed or was denied.
    Failed,
}

/// Update payload for a single inline tool card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUpdate {
    /// Stable tool call identifier.
    pub tool_id: Uuid,
    /// Tool name.
    pub tool_name: String,
    /// Current tool card status.
    pub status: ToolCardStatus,
    /// Concise single-line summary.
    pub summary: String,
    /// Optional detail shown below the summary.
    pub detail: Option<String>,
}

/// Stateful single-session runtime for local TUI and exec flows.
#[derive(Clone)]
pub struct ChatRuntime {
    config: MoaConfig,
    store: Arc<TursoSessionStore>,
    memory_store: Arc<FileMemoryStore>,
    tool_router: Arc<ToolRouter>,
    provider: Arc<AnthropicProvider>,
    workspace_id: WorkspaceId,
    user_id: UserId,
    platform: Platform,
    model: String,
    session_id: SessionId,
}

impl ChatRuntime {
    /// Creates a new single-session runtime from the local MOA config.
    pub async fn from_config(config: MoaConfig, platform: Platform) -> Result<Self> {
        let store = Arc::new(TursoSessionStore::new(&config.local.session_db).await?);
        let memory_store = Arc::new(FileMemoryStore::from_config(&config).await?);
        let tool_router = Arc::new(
            ToolRouter::from_config(&config, memory_store.clone())
                .await?
                .with_rule_store(store.clone()),
        );
        let provider = Arc::new(AnthropicProvider::from_config(&config)?);
        let workspace_id = WorkspaceId::new("default");
        let user_id = local_user_id();
        let model = config.general.default_model.clone();
        let session_id =
            create_session(&store, &workspace_id, &user_id, &model, platform.clone()).await?;

        Ok(Self {
            config,
            store,
            memory_store,
            tool_router,
            provider,
            workspace_id,
            user_id,
            platform,
            model,
            session_id,
        })
    }

    /// Returns the currently active session identifier.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Returns the model identifier currently configured for new turns.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Replaces the active session with a fresh empty session.
    pub async fn reset_session(&mut self) -> Result<SessionId> {
        self.session_id = create_session(
            &self.store,
            &self.workspace_id,
            &self.user_id,
            &self.model,
            self.platform.clone(),
        )
        .await?;
        Ok(self.session_id.clone())
    }

    /// Switches models and starts a fresh session using the new default model.
    pub async fn set_model(&mut self, model: impl Into<String>) -> Result<SessionId> {
        let model = model.into();
        let api_key_env = self.config.providers.anthropic.api_key_env.clone();
        let api_key = env::var(&api_key_env)
            .map_err(|_| MoaError::MissingEnvironmentVariable(api_key_env.clone()))?;
        self.provider = Arc::new(AnthropicProvider::new(api_key, model.clone())?);
        self.model = model;
        self.reset_session().await
    }

    /// Loads the current session metadata snapshot.
    pub async fn session_meta(&self) -> Result<SessionMeta> {
        self.store.get_session(self.session_id.clone()).await
    }

    /// Runs one chat turn with streamed assistant output and inline tool updates.
    pub async fn run_turn(
        &self,
        prompt: String,
        event_tx: mpsc::UnboundedSender<RuntimeEvent>,
        mut control_rx: mpsc::UnboundedReceiver<RuntimeCommand>,
    ) -> Result<()> {
        if prompt.trim().is_empty() {
            return Ok(());
        }

        self.store
            .update_status(self.session_id.clone(), SessionStatus::Running)
            .await?;
        self.store
            .emit_event(
                self.session_id.clone(),
                Event::UserMessage {
                    text: prompt,
                    attachments: Vec::new(),
                },
            )
            .await?;

        loop {
            let session = self.store.get_session(self.session_id.clone()).await?;
            let pipeline = build_default_pipeline_with_tools(
                &self.config,
                self.store.clone(),
                self.memory_store.clone(),
                self.tool_router.tool_schemas(),
            );
            let mut ctx = moa_core::WorkingContext::new(&session, self.provider.capabilities());
            let _stage_reports = pipeline.run(&mut ctx).await?;

            let mut stream = self.provider.complete(ctx.into_request()).await?;
            let mut streamed_text = String::new();
            let mut started_assistant = false;

            while let Some(block) = stream.next().await {
                match block? {
                    CompletionContent::Text(delta) => {
                        if !started_assistant {
                            let _ = event_tx.send(RuntimeEvent::AssistantStarted);
                            started_assistant = true;
                        }
                        streamed_text.push_str(&delta);
                        for ch in delta.chars() {
                            let _ = event_tx.send(RuntimeEvent::AssistantDelta(ch));
                        }
                    }
                    CompletionContent::ToolCall(_) => {}
                }
            }

            let response = stream.into_response().await?;
            if !streamed_text.trim().is_empty() {
                self.store
                    .emit_event(
                        self.session_id.clone(),
                        Event::BrainResponse {
                            text: streamed_text.clone(),
                            model: response.model.clone(),
                            input_tokens: response.input_tokens,
                            output_tokens: response.output_tokens,
                            cost_cents: 0,
                            duration_ms: response.duration_ms,
                        },
                    )
                    .await?;
                let _ = event_tx.send(RuntimeEvent::AssistantFinished {
                    text: streamed_text,
                });
            }

            let mut saw_tool_request = false;
            let mut executed_tool = false;
            for block in &response.content {
                if let CompletionContent::ToolCall(call) = block {
                    saw_tool_request = true;
                    if self
                        .handle_tool_call(&session, call, &event_tx, &mut control_rx)
                        .await?
                    {
                        executed_tool = true;
                    }
                }
            }

            let session = self.store.get_session(self.session_id.clone()).await?;
            let _ = event_tx.send(RuntimeEvent::UsageUpdated {
                total_tokens: session.total_input_tokens + session.total_output_tokens,
            });

            if executed_tool || saw_tool_request || response.stop_reason == StopReason::ToolUse {
                continue;
            }

            self.store
                .update_status(self.session_id.clone(), SessionStatus::Completed)
                .await?;
            let _ = event_tx.send(RuntimeEvent::TurnCompleted);
            return Ok(());
        }
    }

    async fn handle_tool_call(
        &self,
        session: &SessionMeta,
        call: &ToolInvocation,
        event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
        control_rx: &mut mpsc::UnboundedReceiver<RuntimeCommand>,
    ) -> Result<bool> {
        let tool_id = parse_tool_id(call);
        let policy = self.tool_router.check_policy(session, call).await?;
        let summary = policy.input_summary.clone();
        let pattern = always_allow_pattern(&call.name, &policy.normalized_input);

        match policy.action {
            moa_core::PolicyAction::Allow => {
                let _ = event_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                    tool_id,
                    tool_name: call.name.clone(),
                    status: ToolCardStatus::Running,
                    summary,
                    detail: None,
                }));
                self.execute_tool(session, call, tool_id, true, event_tx)
                    .await
            }
            moa_core::PolicyAction::Deny => {
                self.store
                    .emit_event(
                        self.session_id.clone(),
                        Event::ToolCall {
                            tool_id,
                            tool_name: call.name.clone(),
                            input: call.input.clone(),
                            hand_id: None,
                        },
                    )
                    .await?;
                let message = format!("tool {} denied by policy", call.name);
                self.store
                    .emit_event(
                        self.session_id.clone(),
                        Event::ToolError {
                            tool_id,
                            error: message.clone(),
                            retryable: false,
                        },
                    )
                    .await?;
                let _ = event_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                    tool_id,
                    tool_name: call.name.clone(),
                    status: ToolCardStatus::Failed,
                    summary,
                    detail: Some(message),
                }));
                Ok(false)
            }
            moa_core::PolicyAction::RequireApproval => {
                self.store
                    .emit_event(
                        self.session_id.clone(),
                        Event::ToolCall {
                            tool_id,
                            tool_name: call.name.clone(),
                            input: call.input.clone(),
                            hand_id: None,
                        },
                    )
                    .await?;
                let request = ApprovalRequest {
                    request_id: tool_id,
                    tool_name: call.name.clone(),
                    input_summary: summary.clone(),
                    risk_level: policy.risk_level,
                };
                self.store
                    .emit_event(
                        self.session_id.clone(),
                        Event::ApprovalRequested {
                            request_id: request.request_id,
                            tool_name: request.tool_name.clone(),
                            input_summary: request.input_summary.clone(),
                            risk_level: request.risk_level.clone(),
                        },
                    )
                    .await?;
                self.store
                    .update_status(self.session_id.clone(), SessionStatus::WaitingApproval)
                    .await?;

                let _ = event_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                    tool_id,
                    tool_name: call.name.clone(),
                    status: ToolCardStatus::WaitingApproval,
                    summary: summary.clone(),
                    detail: Some("Press y to allow once, a to always allow, n to deny".to_string()),
                }));
                let _ = event_tx.send(RuntimeEvent::ApprovalRequested(ApprovalPrompt {
                    request,
                    pattern,
                    parameters: approval_fields_for_call(&self.config, call),
                    file_diffs: self.approval_diffs_for_call(call).await?,
                }));

                match wait_for_approval(control_rx).await? {
                    ApprovalDecision::AllowOnce => {
                        self.store
                            .emit_event(
                                self.session_id.clone(),
                                Event::ApprovalDecided {
                                    request_id: tool_id,
                                    decision: ApprovalDecision::AllowOnce,
                                    decided_by: session.user_id.to_string(),
                                    decided_at: Utc::now(),
                                },
                            )
                            .await?;
                        self.store
                            .update_status(self.session_id.clone(), SessionStatus::Running)
                            .await?;
                        let _ = event_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                            tool_id,
                            tool_name: call.name.clone(),
                            status: ToolCardStatus::Running,
                            summary,
                            detail: None,
                        }));
                        self.execute_tool(session, call, tool_id, false, event_tx)
                            .await
                    }
                    ApprovalDecision::AlwaysAllow { pattern } => {
                        self.store
                            .emit_event(
                                self.session_id.clone(),
                                Event::ApprovalDecided {
                                    request_id: tool_id,
                                    decision: ApprovalDecision::AlwaysAllow {
                                        pattern: pattern.clone(),
                                    },
                                    decided_by: session.user_id.to_string(),
                                    decided_at: Utc::now(),
                                },
                            )
                            .await?;
                        self.tool_router
                            .store_approval_rule(
                                session,
                                &call.name,
                                &pattern,
                                moa_core::PolicyAction::Allow,
                                session.user_id.clone(),
                            )
                            .await?;
                        self.store
                            .update_status(self.session_id.clone(), SessionStatus::Running)
                            .await?;
                        let _ = event_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                            tool_id,
                            tool_name: call.name.clone(),
                            status: ToolCardStatus::Running,
                            summary,
                            detail: Some(format!("Always allow rule stored: {pattern}")),
                        }));
                        self.execute_tool(session, call, tool_id, false, event_tx)
                            .await
                    }
                    ApprovalDecision::Deny { reason } => {
                        self.store
                            .emit_event(
                                self.session_id.clone(),
                                Event::ApprovalDecided {
                                    request_id: tool_id,
                                    decision: ApprovalDecision::Deny {
                                        reason: reason.clone(),
                                    },
                                    decided_by: session.user_id.to_string(),
                                    decided_at: Utc::now(),
                                },
                            )
                            .await?;
                        self.store
                            .emit_event(
                                self.session_id.clone(),
                                Event::ToolError {
                                    tool_id,
                                    error: reason.clone().unwrap_or_else(|| {
                                        "tool execution denied by user".to_string()
                                    }),
                                    retryable: false,
                                },
                            )
                            .await?;
                        self.store
                            .update_status(self.session_id.clone(), SessionStatus::Running)
                            .await?;
                        let _ = event_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                            tool_id,
                            tool_name: call.name.clone(),
                            status: ToolCardStatus::Failed,
                            summary,
                            detail: Some(
                                reason.unwrap_or_else(|| "Denied by the user".to_string()),
                            ),
                        }));
                        Ok(false)
                    }
                }
            }
        }
    }

    async fn execute_tool(
        &self,
        session: &SessionMeta,
        call: &ToolInvocation,
        tool_id: Uuid,
        emit_call_event: bool,
        event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) -> Result<bool> {
        match self.tool_router.execute_authorized(session, call).await {
            Ok((hand_id, output)) => {
                if emit_call_event {
                    self.store
                        .emit_event(
                            self.session_id.clone(),
                            Event::ToolCall {
                                tool_id,
                                tool_name: call.name.clone(),
                                input: call.input.clone(),
                                hand_id,
                            },
                        )
                        .await?;
                }
                self.store
                    .emit_event(
                        self.session_id.clone(),
                        Event::ToolResult {
                            tool_id,
                            output: format_tool_output(&output),
                            success: output.exit_code == 0,
                            duration_ms: output.duration.as_millis() as u64,
                        },
                    )
                    .await?;
                let _ = event_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                    tool_id,
                    tool_name: call.name.clone(),
                    status: if output.exit_code == 0 {
                        ToolCardStatus::Succeeded
                    } else {
                        ToolCardStatus::Failed
                    },
                    summary: summarize_tool_completion(call, &output),
                    detail: Some(format_tool_output(&output)),
                }));
                Ok(true)
            }
            Err(error) => {
                self.store
                    .emit_event(
                        self.session_id.clone(),
                        Event::ToolError {
                            tool_id,
                            error: error.to_string(),
                            retryable: false,
                        },
                    )
                    .await?;
                let _ = event_tx.send(RuntimeEvent::ToolUpdate(ToolUpdate {
                    tool_id,
                    tool_name: call.name.clone(),
                    status: ToolCardStatus::Failed,
                    summary: format!("{} failed", call.name),
                    detail: Some(error.to_string()),
                }));
                Ok(false)
            }
        }
    }

    async fn approval_diffs_for_call(
        &self,
        call: &ToolInvocation,
    ) -> Result<Vec<ApprovalFileDiff>> {
        if call.name != "file_write" {
            return Ok(Vec::new());
        }

        let Some(path) = call.input.get("path").and_then(Value::as_str) else {
            return Ok(Vec::new());
        };
        let Some(content) = call.input.get("content").and_then(Value::as_str) else {
            return Ok(Vec::new());
        };

        let sandbox_root = expand_local_path(&self.config.local.sandbox_dir)?;
        let file_path = resolve_sandbox_path(&sandbox_root, path)?;
        let before = read_existing_text_file(&file_path).await?;

        Ok(vec![ApprovalFileDiff {
            path: path.to_string(),
            before,
            after: content.to_string(),
            language_hint: language_hint_for_path(path),
        }])
    }
}

fn summarize_tool_completion(call: &ToolInvocation, output: &moa_core::ToolOutput) -> String {
    if output.exit_code == 0 {
        format!(
            "{} completed in {} ms",
            call.name,
            output.duration.as_millis()
        )
    } else {
        format!("{} exited with code {}", call.name, output.exit_code)
    }
}

fn format_tool_output(output: &moa_core::ToolOutput) -> String {
    let mut sections = Vec::new();
    if !output.stdout.trim().is_empty() {
        sections.push(output.stdout.trim_end().to_string());
    }
    if !output.stderr.trim().is_empty() {
        sections.push(format!("stderr:\n{}", output.stderr.trim_end()));
    }
    if sections.is_empty() {
        format!("exit_code: {}", output.exit_code)
    } else {
        sections.join("\n\n")
    }
}

fn parse_tool_id(call: &ToolInvocation) -> Uuid {
    call.id
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or_else(Uuid::new_v4)
}

async fn wait_for_approval(
    control_rx: &mut mpsc::UnboundedReceiver<RuntimeCommand>,
) -> Result<ApprovalDecision> {
    match control_rx.recv().await {
        Some(RuntimeCommand::Approval(decision)) => Ok(decision),
        None => Err(MoaError::ToolError(
            "approval channel closed before a decision was received".to_string(),
        )),
    }
}

fn always_allow_pattern(tool_name: &str, normalized_input: &str) -> String {
    if tool_name == "bash" {
        let tokens = shell_words::split(normalized_input).unwrap_or_default();
        if let Some(command) = tokens.first() {
            return if tokens.len() == 1 {
                command.clone()
            } else {
                format!("{command} *")
            };
        }
    }

    normalized_input.to_string()
}

fn approval_fields_for_call(config: &MoaConfig, call: &ToolInvocation) -> Vec<ApprovalField> {
    match call.name.as_str() {
        "bash" => {
            let command = call
                .input
                .get("cmd")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let working_dir = expand_local_path(&config.local.sandbox_dir)
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| config.local.sandbox_dir.clone());
            vec![
                ApprovalField {
                    label: "Command".to_string(),
                    value: command,
                },
                ApprovalField {
                    label: "Working dir".to_string(),
                    value: working_dir,
                },
            ]
        }
        "file_write" => {
            let path = call
                .input
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let content_len = call
                .input
                .get("content")
                .and_then(Value::as_str)
                .map(|content| content.chars().count())
                .unwrap_or_default();
            vec![
                ApprovalField {
                    label: "Path".to_string(),
                    value: path,
                },
                ApprovalField {
                    label: "Content".to_string(),
                    value: format!("{content_len} chars"),
                },
            ]
        }
        "file_read" => single_approval_field("Path", &call.input, "path"),
        "file_search" => single_approval_field("Pattern", &call.input, "pattern"),
        "memory_search" | "web_search" => single_approval_field("Query", &call.input, "query"),
        "memory_write" => single_approval_field("Path", &call.input, "path"),
        "web_fetch" => single_approval_field("URL", &call.input, "url"),
        _ => serde_json::to_string_pretty(&call.input)
            .map(|value| {
                vec![ApprovalField {
                    label: "Input".to_string(),
                    value,
                }]
            })
            .unwrap_or_default(),
    }
}

fn single_approval_field(label: &str, input: &Value, field: &str) -> Vec<ApprovalField> {
    let value = input
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    vec![ApprovalField {
        label: label.to_string(),
        value,
    }]
}

fn expand_local_path(path: &str) -> Result<PathBuf> {
    if let Some(relative) = path.strip_prefix("~/") {
        let home = env::var("HOME").map_err(|_| MoaError::HomeDirectoryNotFound)?;
        return Ok(PathBuf::from(home).join(relative));
    }

    Ok(PathBuf::from(path))
}

async fn read_existing_text_file(path: &Path) -> Result<String> {
    match fs::read(path).await {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn language_hint_for_path(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(ToOwned::to_owned)
}

fn local_user_id() -> UserId {
    UserId::new(
        env::var("USER")
            .or_else(|_| env::var("USERNAME"))
            .unwrap_or_else(|_| "local-user".to_string()),
    )
}

async fn create_session(
    store: &Arc<TursoSessionStore>,
    workspace_id: &WorkspaceId,
    user_id: &UserId,
    model: &str,
    platform: Platform,
) -> Result<SessionId> {
    store
        .create_session(SessionMeta {
            workspace_id: workspace_id.clone(),
            user_id: user_id.clone(),
            model: model.to_string(),
            platform,
            ..SessionMeta::default()
        })
        .await
}

#[cfg(test)]
impl ChatRuntime {
    /// Creates a fully local runtime rooted in a unique temporary directory for tests.
    pub async fn for_test(platform: Platform) -> Result<Self> {
        let base = std::env::temp_dir().join(format!("moa-tui-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&base).await?;

        let mut config = MoaConfig::default();
        config.local.session_db = base.join("sessions.db").display().to_string();
        config.local.memory_dir = base.join("memory").display().to_string();
        config.local.sandbox_dir = base.join("sandbox").display().to_string();

        let store = Arc::new(TursoSessionStore::new(&config.local.session_db).await?);
        let memory_store = Arc::new(FileMemoryStore::from_config(&config).await?);
        let tool_router = Arc::new(
            ToolRouter::from_config(&config, memory_store.clone())
                .await?
                .with_rule_store(store.clone()),
        );
        let provider = Arc::new(AnthropicProvider::new(
            "test-key",
            config.general.default_model.clone(),
        )?);
        let workspace_id = WorkspaceId::new("default");
        let user_id = UserId::new("tester");
        let model = config.general.default_model.clone();
        let session_id =
            create_session(&store, &workspace_id, &user_id, &model, platform.clone()).await?;

        Ok(Self {
            config,
            store,
            memory_store,
            tool_router,
            provider,
            workspace_id,
            user_id,
            platform,
            model,
            session_id,
        })
    }
}
