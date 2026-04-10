//! App state machine and render loop for the multi-session local TUI.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event as CrosstermEvent, KeyCode, KeyEvent},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    widgets::{Block, Borders, Paragraph},
};
use tokio::fs;
use tokio::{sync::mpsc, task::JoinHandle};
use uuid::Uuid;

use moa_core::{
    ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt, ApprovalRequest, Event,
    EventRecord, MemoryPath, MoaConfig, PageSummary, Result, RiskLevel, RuntimeEvent, SessionId,
    SessionMeta, SessionStatus, ToolCardStatus, ToolUpdate, WorkspaceId,
};

use crate::{
    keybindings::{KeyAction, map_key_event},
    runner::{ChatRuntime, SessionPreview, SessionRuntimeEvent},
    views::{
        chat,
        diff::{self, DiffViewState},
        memory::{self, MemoryViewState},
        palette::{self, PaletteAction, PaletteState},
        sessions::{self, SessionPickerState},
        settings::{self, SettingsMutation, SettingsViewState},
    },
    widgets::{
        prompt::{
            PromptCompletionKind, PromptCompletionState, PromptWidget, render_completion_menu,
        },
        sidebar, toolbar,
    },
};

const FRAME_DURATION: Duration = Duration::from_millis(33);
const SESSION_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// High-level UI mode for the currently selected session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    /// No prompt text and no active generation.
    Idle,
    /// Prompt text is being edited.
    Composing,
    /// The active session is currently running.
    Running,
    /// The active session is blocked on approval.
    WaitingApproval,
    /// The full-screen diff viewer is open.
    ViewingDiff,
    /// The session picker overlay is open.
    PickingSession,
    /// The command palette overlay is open.
    CommandPalette,
    /// The memory browser is open.
    MemoryBrowser,
    /// The settings overlay is open.
    Settings,
    /// The help overlay is open.
    Help,
}

/// Renderable transcript entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChatEntry {
    User(String),
    Assistant { text: String, streaming: bool },
    Approval(ApprovalEntry),
    Tool(ToolCardEntry),
    Status(String),
}

/// Renderable inline approval card state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApprovalEntry {
    pub(crate) prompt: ApprovalPrompt,
    pub(crate) status: ApprovalCardStatus,
    pub(crate) note: Option<String>,
}

/// Current approval outcome shown on the inline approval card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalCardStatus {
    Pending,
    AllowedOnce,
    AllowedAlways,
    Denied,
}

/// Renderable inline tool card state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolCardEntry {
    pub(crate) tool_id: Uuid,
    pub(crate) tool_name: String,
    pub(crate) status: ToolCardStatus,
    pub(crate) summary: String,
    pub(crate) detail: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct SessionViewState {
    entries: Vec<ChatEntry>,
    total_tokens: usize,
    scroll: u16,
    auto_scroll: bool,
    pending_approval: Option<ApprovalPrompt>,
    diff_view: Option<DiffViewState>,
    loaded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingChord {
    OpenSessions,
    Leader,
}

/// Stateful TUI application model.
pub struct App {
    config: MoaConfig,
    config_path: PathBuf,
    runtime: ChatRuntime,
    mode: AppMode,
    prompt: PromptWidget,
    sessions: Vec<SessionPreview>,
    session_meta: HashMap<SessionId, SessionMeta>,
    session_views: HashMap<SessionId, SessionViewState>,
    memory_view: Option<MemoryViewState>,
    settings_view: Option<SettingsViewState>,
    command_palette: Option<PaletteState>,
    prompt_completion: Option<PromptCompletionState>,
    sidebar_override: Option<bool>,
    show_help: bool,
    recent_memory: Vec<PageSummary>,
    tool_names: Vec<String>,
    file_frecency: HashMap<String, u32>,
    known_files: Vec<String>,
    active_session_id: SessionId,
    viewport_width: u16,
    should_exit: bool,
    pending_chord: Option<PendingChord>,
    session_picker: Option<SessionPickerState>,
    runtime_event_tx: mpsc::UnboundedSender<SessionRuntimeEvent>,
    runtime_event_rx: mpsc::UnboundedReceiver<SessionRuntimeEvent>,
    observation_task: Option<JoinHandle<()>>,
    observed_session_id: Option<SessionId>,
    last_session_refresh: Instant,
}

/// Options used when launching the full-screen TUI.
#[derive(Debug, Clone, Default)]
pub struct RunTuiOptions {
    /// Session to attach to after startup.
    pub attach_session_id: Option<SessionId>,
    /// Prompt to prefill and submit on startup.
    pub initial_prompt: Option<String>,
    /// When true, force the runtime to connect through the daemon.
    pub force_daemon: bool,
}

impl App {
    /// Creates a new TUI app from the loaded MOA config.
    pub async fn new(config: MoaConfig) -> Result<Self> {
        let runtime = ChatRuntime::from_config(config.clone(), moa_core::Platform::Tui).await?;
        Self::from_runtime(config, runtime, RunTuiOptions::default()).await
    }

    /// Creates a new TUI app from an explicit runtime and launch options.
    pub async fn from_runtime(
        config: MoaConfig,
        runtime: ChatRuntime,
        options: RunTuiOptions,
    ) -> Result<Self> {
        let config_path = default_config_path()?;
        let active_session_id = options
            .attach_session_id
            .clone()
            .unwrap_or_else(|| runtime.session_id().clone());
        let tool_names = runtime.tool_names_async().await.unwrap_or_default();
        let recent_memory = runtime.recent_memory_entries(6).await.unwrap_or_default();
        let known_files = collect_sandbox_files(&runtime.sandbox_root())
            .await
            .unwrap_or_default();
        let (runtime_event_tx, runtime_event_rx) = mpsc::unbounded_channel();
        let mut app = Self {
            config,
            config_path,
            runtime,
            mode: AppMode::Idle,
            prompt: PromptWidget::new(),
            sessions: Vec::new(),
            session_meta: HashMap::new(),
            session_views: HashMap::new(),
            memory_view: None,
            settings_view: None,
            command_palette: None,
            prompt_completion: None,
            sidebar_override: None,
            show_help: false,
            recent_memory,
            tool_names,
            file_frecency: HashMap::new(),
            known_files,
            active_session_id: active_session_id.clone(),
            viewport_width: 0,
            should_exit: false,
            pending_chord: None,
            session_picker: None,
            runtime_event_tx,
            runtime_event_rx,
            observation_task: None,
            observed_session_id: None,
            last_session_refresh: Instant::now() - SESSION_REFRESH_INTERVAL,
        };
        app.refresh_sessions_if_due().await?;
        app.switch_to_session(active_session_id).await?;
        if let Some(prompt) = options.initial_prompt {
            app.prompt.replace_text(prompt);
            app.submit_prompt(false).await?;
        }
        Ok(app)
    }

    /// Returns the current high-level app mode.
    pub fn mode(&self) -> AppMode {
        self.mode
    }

    /// Returns whether the app requested clean shutdown.
    pub fn should_exit(&self) -> bool {
        self.should_exit
    }

    /// Refreshes the session list if the polling interval has elapsed.
    pub async fn refresh_sessions_if_due(&mut self) -> Result<()> {
        if self.last_session_refresh.elapsed() < SESSION_REFRESH_INTERVAL {
            return Ok(());
        }

        self.refresh_session_list().await
    }

    /// Processes any pending runtime events.
    pub fn drain_runtime_events(&mut self) {
        while let Ok(event) = self.runtime_event_rx.try_recv() {
            self.handle_runtime_event(event);
        }
    }

    /// Handles a single key press from the terminal loop.
    pub async fn handle_key_event(&mut self, key: KeyEvent) -> Result<()> {
        if self.handle_pending_chord(key).await? {
            return Ok(());
        }

        if matches!(key.code, KeyCode::Char('d'))
            && key
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            if self.prompt.text().trim().is_empty() && !self.active_session_is_busy() {
                self.should_exit = true;
            } else {
                self.half_page_scroll(true);
            }
            self.sync_mode_with_state();
            return Ok(());
        }

        match map_key_event(self.mode, key) {
            KeyAction::Submit => self.submit_prompt(false).await?,
            KeyAction::QueuePrompt => self.submit_prompt(true).await?,
            KeyAction::InsertNewline => {
                self.prompt.insert_newline();
                self.refresh_prompt_completion().await?;
                self.sync_mode_with_state();
            }
            KeyAction::Cancel => self.cancel_or_exit().await?,
            KeyAction::ApproveOnce => self.send_approval(ApprovalDecision::AllowOnce).await?,
            KeyAction::OpenDiff => self.open_diff_view(),
            KeyAction::CloseDiff => self.close_diff_view(),
            KeyAction::ToggleDiffMode => self.toggle_diff_mode(),
            KeyAction::NextDiffFile => self.move_diff_file(true),
            KeyAction::PreviousDiffFile => self.move_diff_file(false),
            KeyAction::NextDiffHunk => self.move_diff_hunk(true),
            KeyAction::PreviousDiffHunk => self.move_diff_hunk(false),
            KeyAction::AlwaysAllow => {
                if let Some(prompt) = self
                    .active_view()
                    .and_then(|view| view.pending_approval.clone())
                {
                    self.send_approval(ApprovalDecision::AlwaysAllow {
                        pattern: prompt.pattern,
                    })
                    .await?;
                }
            }
            KeyAction::Deny => {
                self.send_approval(ApprovalDecision::Deny { reason: None })
                    .await?
            }
            KeyAction::EditApproval => {
                self.push_status_line(
                    self.active_session_id.clone(),
                    "Editing approval parameters is not implemented yet.".to_string(),
                );
            }
            KeyAction::ScrollUp => {
                let view = self.session_view_mut(self.active_session_id.clone());
                view.auto_scroll = false;
                view.scroll = view.scroll.saturating_sub(1);
            }
            KeyAction::ScrollDown => {
                let view = self.session_view_mut(self.active_session_id.clone());
                view.auto_scroll = false;
                view.scroll = view.scroll.saturating_add(1);
            }
            KeyAction::ScrollEnd => {
                self.session_view_mut(self.active_session_id.clone())
                    .auto_scroll = true;
            }
            KeyAction::ScrollHome => {
                let view = self.session_view_mut(self.active_session_id.clone());
                view.auto_scroll = false;
                view.scroll = 0;
            }
            KeyAction::HalfPageUp => self.half_page_scroll(false),
            KeyAction::HalfPageDown => self.half_page_scroll(true),
            KeyAction::ClearScreen => self.reload_active_session().await?,
            KeyAction::NewSession => self.create_new_session().await?,
            KeyAction::NextSession => self.cycle_session(true).await?,
            KeyAction::PreviousSession => self.cycle_session(false).await?,
            KeyAction::SwitchSessionTab(index) => self.switch_tab_by_index(index).await?,
            KeyAction::StartSessionPickerChord => {
                self.pending_chord = Some(PendingChord::OpenSessions);
            }
            KeyAction::StartSoftStopChord => {
                self.pending_chord = Some(PendingChord::Leader);
            }
            KeyAction::PickerUp => self.move_picker(false),
            KeyAction::PickerDown => self.move_picker(true),
            KeyAction::PickerSelect => self.confirm_picker_selection().await?,
            KeyAction::PickerBackspace => self.update_picker_query(None),
            KeyAction::SessionPickerInput => self.handle_picker_input(key),
            KeyAction::OpenCommandPalette => self.open_command_palette(),
            KeyAction::OpenMemoryBrowser => self.open_memory_browser().await?,
            KeyAction::OpenSettings => self.open_settings(),
            KeyAction::AcceptCompletion => self.accept_prompt_completion().await?,
            KeyAction::PaletteUp => self.move_palette(false),
            KeyAction::PaletteDown => self.move_palette(true),
            KeyAction::PaletteSelect => self.select_palette_action().await?,
            KeyAction::PaletteInput => self.handle_palette_input(key),
            KeyAction::PaletteBackspace => self.handle_palette_backspace(),
            KeyAction::MemorySearch => self.start_memory_search(),
            KeyAction::MemorySearchBackspace => self.memory_search_backspace().await?,
            KeyAction::MemorySearchInput => self.memory_search_input(key).await?,
            KeyAction::MemoryUp => self.move_memory(false),
            KeyAction::MemoryDown => self.move_memory(true),
            KeyAction::MemoryOpen => self.open_selected_memory_item().await?,
            KeyAction::MemoryBack => self.navigate_memory_history(false).await?,
            KeyAction::MemoryForward => self.navigate_memory_history(true).await?,
            KeyAction::MemoryEdit => self.open_memory_page_in_editor().await?,
            KeyAction::MemoryDelete => self.delete_memory_page().await?,
            KeyAction::SettingsUp => self.move_settings(false),
            KeyAction::SettingsDown => self.move_settings(true),
            KeyAction::SettingsCategoryLeft => self.move_settings_category(false),
            KeyAction::SettingsCategoryRight => self.move_settings_category(true),
            KeyAction::SettingsApply => self.apply_settings_change(true).await?,
            KeyAction::SettingsReverse => self.apply_settings_change(false).await?,
            KeyAction::CycleVerbosity => {
                self.push_status_line(
                    self.active_session_id.clone(),
                    "Observation verbosity cycling is not implemented yet.".to_string(),
                );
            }
            KeyAction::PromptInput => {
                self.prompt.input(key);
                self.refresh_prompt_completion().await?;
                self.sync_mode_with_state();
            }
            KeyAction::Noop => {}
        }

        Ok(())
    }

    /// Renders the full app into a frame.
    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        let size = frame.area();
        self.viewport_width = size.width;
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(4),
                Constraint::Length(1),
            ])
            .split(size);

        toolbar::render_toolbar(
            frame,
            layout[0],
            &self.sessions,
            &self.active_session_id,
            toolbar::ToolbarMetrics {
                workspace_name: self.runtime.workspace_id().as_str(),
                model: self.runtime.model(),
                total_tokens: self
                    .active_view()
                    .map(|view| view.total_tokens)
                    .unwrap_or_default(),
                total_cost_cents: self
                    .active_session_meta()
                    .map(|meta| meta.total_cost_cents)
                    .unwrap_or_default(),
            },
        );

        let show_sidebar = sidebar::should_show_sidebar(
            size.width,
            self.config.tui.sidebar_auto,
            self.sidebar_override,
        );
        let body = if show_sidebar {
            let split = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(72), Constraint::Percentage(28)])
                .split(layout[1]);
            sidebar::render_sidebar(
                frame,
                split[1],
                self.active_session_meta(),
                &self.tool_names,
                &self.recent_memory,
            );
            split[0]
        } else {
            layout[1]
        };

        let (entries, scroll) = match self.active_view_mut() {
            Some(view) => {
                if view.auto_scroll {
                    view.scroll = chat::max_scroll(&view.entries, body.width, body.height);
                }
                (&view.entries, view.scroll)
            }
            None => {
                static EMPTY: Vec<ChatEntry> = Vec::new();
                (&EMPTY, 0)
            }
        };
        chat::render_chat(frame, body, entries, scroll);

        self.prompt.render(frame, layout[2], self.mode);
        if let Some(completion) = &self.prompt_completion {
            render_completion_menu(frame, layout[2], completion);
        }

        let footer =
            Paragraph::new(self.footer_text()).block(Block::default().borders(Borders::ALL));
        frame.render_widget(footer, layout[3]);

        if let Some(diff_view) = self.active_view().and_then(|view| view.diff_view.clone()) {
            diff::render_diff_view(frame, size, &diff_view);
        }

        if let Some(picker) = &self.session_picker {
            sessions::render_session_picker(frame, size, picker, &self.sessions);
        }
        if let Some(memory_view) = &self.memory_view {
            memory::render_memory_view(frame, size, memory_view);
        }
        if let Some(settings_view) = &self.settings_view {
            settings::render_settings_view(frame, size, settings_view, &self.config);
        }
        if let Some(palette) = &self.command_palette {
            palette::render_palette(frame, size, palette, &palette_actions());
        }
        if self.show_help {
            render_help_overlay(frame, size);
        }
    }

    async fn submit_prompt(&mut self, queue_only: bool) -> Result<()> {
        let prompt = self.prompt.text();
        let trimmed = prompt.trim();
        if trimmed.is_empty() {
            return Ok(());
        }

        if trimmed.starts_with('/') && !queue_only {
            self.prompt.clear();
            self.prompt_completion = None;
            self.sync_mode_with_state();
            self.handle_slash_command(trimmed).await?;
            return Ok(());
        }

        if self.active_session_is_busy() && !queue_only {
            self.push_status_line(
                self.active_session_id.clone(),
                "Session is busy. Press Ctrl+Q to queue the message or Ctrl+X, S to stop."
                    .to_string(),
            );
            return Ok(());
        }

        let session_id = self.active_session_id.clone();
        self.session_view_mut(session_id.clone())
            .entries
            .push(ChatEntry::User(trimmed.to_string()));
        self.session_view_mut(session_id.clone()).auto_scroll = true;
        self.prompt.clear();
        self.prompt_completion = None;

        self.runtime
            .queue_message(session_id.clone(), trimmed.to_string())
            .await?;
        self.update_session_status(&session_id, SessionStatus::Running);
        self.start_active_observer().await?;
        self.sync_mode_with_state();
        Ok(())
    }

    async fn handle_slash_command(&mut self, command: &str) -> Result<()> {
        let mut parts = command.split_whitespace();
        match parts.next().unwrap_or_default() {
            "/help" => {
                self.show_help = true;
            }
            "/quit" => {
                self.should_exit = true;
            }
            "/clear" | "/new" => {
                self.create_new_session().await?;
            }
            "/sessions" => {
                self.open_session_picker();
            }
            "/memory" => {
                self.open_memory_browser().await?;
            }
            "/settings" => self.open_settings(),
            "/model" => {
                if let Some(model) = parts.next() {
                    let session_id = self.runtime.set_model(model.to_string()).await?;
                    self.config.general.default_model = self.runtime.model().to_string();
                    self.prompt.clear();
                    self.refresh_session_list().await?;
                    self.switch_to_session(session_id).await?;
                    self.persist_config().await?;
                    self.push_status_line(
                        self.active_session_id.clone(),
                        format!("Switched model to {model} and started a fresh session."),
                    );
                } else {
                    self.push_status_line(
                        self.active_session_id.clone(),
                        format!("Current model: {}", self.runtime.model()),
                    );
                }
            }
            "/workspace" => {
                if let Some(workspace) = parts.next() {
                    let workspace_id = workspace_name_from_input(workspace);
                    let session_id = self.runtime.set_workspace(workspace_id.clone()).await?;
                    self.refresh_session_list().await?;
                    self.switch_to_session(session_id).await?;
                    self.push_status_line(
                        self.active_session_id.clone(),
                        format!("Switched workspace to {}.", workspace_id.as_str()),
                    );
                } else {
                    self.push_status_line(
                        self.active_session_id.clone(),
                        format!(
                            "Current workspace: {}",
                            self.runtime.workspace_id().as_str()
                        ),
                    );
                }
            }
            "/status" => {
                if let Some(meta) = self.active_session_meta() {
                    self.push_status_line(
                        self.active_session_id.clone(),
                        format!(
                            "status={} tokens={} cost=${:.2}",
                            format!("{:?}", meta.status).to_lowercase(),
                            meta.total_input_tokens + meta.total_output_tokens,
                            meta.total_cost_cents as f32 / 100.0
                        ),
                    );
                }
            }
            other => {
                self.push_status_line(
                    self.active_session_id.clone(),
                    format!("Unknown command: {other}. Try /help."),
                );
            }
        }

        self.sync_mode_with_state();
        Ok(())
    }

    async fn refresh_session_list(&mut self) -> Result<()> {
        self.sessions = self.runtime.list_session_previews().await?;
        self.refresh_sidebar_data().await?;
        if !self
            .sessions
            .iter()
            .any(|preview| preview.summary.session_id == self.active_session_id)
        {
            let meta = self
                .runtime
                .session_meta_by_id(self.active_session_id.clone())
                .await?;
            self.sessions.insert(
                0,
                SessionPreview {
                    summary: session_summary_from_meta(&meta),
                    last_message: None,
                },
            );
            self.session_meta
                .insert(self.active_session_id.clone(), meta);
        }

        if let Some(picker) = &mut self.session_picker {
            picker
                .clamp_selection(sessions::filtered_sessions(picker.query(), &self.sessions).len());
        }

        self.last_session_refresh = Instant::now();
        self.sync_mode_with_state();
        Ok(())
    }

    async fn create_new_session(&mut self) -> Result<()> {
        let session_id = self.runtime.create_session().await?;
        self.prompt.clear();
        self.refresh_session_list().await?;
        self.switch_to_session(session_id).await
    }

    async fn switch_to_session(&mut self, session_id: SessionId) -> Result<()> {
        self.active_session_id = session_id.clone();
        self.load_session_if_needed(session_id).await?;
        self.start_active_observer().await?;
        self.sync_mode_with_state();
        Ok(())
    }

    async fn switch_tab_by_index(&mut self, index: usize) -> Result<()> {
        if let Some(session_id) = self
            .sessions
            .get(index)
            .map(|preview| preview.summary.session_id.clone())
        {
            self.switch_to_session(session_id).await?;
        }
        Ok(())
    }

    async fn cycle_session(&mut self, forward: bool) -> Result<()> {
        if self.sessions.is_empty() {
            return Ok(());
        }

        let current = self
            .sessions
            .iter()
            .position(|preview| preview.summary.session_id == self.active_session_id)
            .unwrap_or_default();
        let next = if forward {
            (current + 1) % self.sessions.len()
        } else if current == 0 {
            self.sessions.len().saturating_sub(1)
        } else {
            current.saturating_sub(1)
        };
        self.switch_to_session(self.sessions[next].summary.session_id.clone())
            .await
    }

    async fn load_session_if_needed(&mut self, session_id: SessionId) -> Result<()> {
        let should_reload = match self.session_views.get(&session_id) {
            Some(view) if view.loaded && self.active_session_is_live(session_id.clone()) => {
                self.observed_session_id.as_ref() != Some(&session_id)
            }
            Some(view) => !view.loaded || !self.active_session_is_live(session_id.clone()),
            None => true,
        };
        if !should_reload {
            return Ok(());
        }

        let events = self.runtime.session_events(session_id.clone()).await?;
        let meta = self.runtime.session_meta_by_id(session_id.clone()).await?;
        self.session_meta.insert(session_id.clone(), meta.clone());
        let cached_prompt = self
            .session_views
            .get(&session_id)
            .and_then(|view| view.pending_approval.clone());
        self.session_views.insert(
            session_id,
            SessionViewState::from_history(&meta, &events, cached_prompt),
        );
        Ok(())
    }

    async fn start_active_observer(&mut self) -> Result<()> {
        if let Some(task) = self.observation_task.take() {
            task.abort();
        }

        let runtime = self.runtime.clone();
        let session_id = self.active_session_id.clone();
        let event_tx = self.runtime_event_tx.clone();
        self.observed_session_id = Some(session_id.clone());
        self.observation_task = Some(tokio::spawn(async move {
            let _ = runtime.observe_session(session_id, event_tx).await;
        }));
        Ok(())
    }

    fn handle_runtime_event(&mut self, envelope: SessionRuntimeEvent) {
        let session_id = envelope.session_id.clone();
        let is_active = session_id == self.active_session_id;
        let view = self.session_view_mut(session_id.clone());
        match envelope.event {
            RuntimeEvent::AssistantStarted => {
                view.entries.push(ChatEntry::Assistant {
                    text: String::new(),
                    streaming: true,
                });
                self.update_session_status(&session_id, SessionStatus::Running);
                if let Some(meta) = self.session_meta.get_mut(&session_id) {
                    meta.status = SessionStatus::Running;
                }
            }
            RuntimeEvent::AssistantDelta(ch) => {
                if let Some(ChatEntry::Assistant { text, .. }) = view.entries.last_mut() {
                    text.push(ch);
                } else {
                    view.entries.push(ChatEntry::Assistant {
                        text: ch.to_string(),
                        streaming: true,
                    });
                }
                view.auto_scroll = true;
            }
            RuntimeEvent::AssistantFinished { text } => {
                if let Some(ChatEntry::Assistant {
                    text: current,
                    streaming,
                }) = view.entries.last_mut()
                {
                    *current = text;
                    *streaming = false;
                } else {
                    view.entries.push(ChatEntry::Assistant {
                        text,
                        streaming: false,
                    });
                }
                view.auto_scroll = true;
            }
            RuntimeEvent::ToolUpdate(update) => {
                view.handle_tool_update(update);
                self.update_session_status(&session_id, SessionStatus::Running);
            }
            RuntimeEvent::ApprovalRequested(prompt) => {
                view.upsert_approval_card(prompt.clone());
                view.pending_approval = Some(prompt);
                view.auto_scroll = true;
                self.update_session_status(&session_id, SessionStatus::WaitingApproval);
                if let Some(meta) = self.session_meta.get_mut(&session_id) {
                    meta.status = SessionStatus::WaitingApproval;
                }
            }
            RuntimeEvent::UsageUpdated { total_tokens } => {
                view.total_tokens = total_tokens;
            }
            RuntimeEvent::Notice(text) | RuntimeEvent::Error(text) => {
                view.entries.push(ChatEntry::Status(text));
                view.auto_scroll = true;
            }
            RuntimeEvent::TurnCompleted => {
                view.pending_approval = None;
                view.diff_view = None;
                view.auto_scroll = true;
                if self
                    .active_session_preview(&session_id)
                    .map(|preview| preview.summary.status == SessionStatus::Running)
                    .unwrap_or(false)
                {
                    self.update_session_status(&session_id, SessionStatus::Completed);
                    if let Some(meta) = self.session_meta.get_mut(&session_id) {
                        meta.status = SessionStatus::Completed;
                    }
                }
            }
        }

        if is_active {
            self.sync_mode_with_state();
        }
    }

    async fn send_approval(&mut self, decision: ApprovalDecision) -> Result<()> {
        let Some(prompt) = self
            .active_view()
            .and_then(|view| view.pending_approval.clone())
        else {
            return Ok(());
        };

        let request_id = prompt.request.request_id;
        let (status, note) = approval_status_and_note(&decision);
        let session_id = self.active_session_id.clone();
        let view = self.session_view_mut(session_id.clone());
        view.update_approval_entry(request_id, status, note);
        view.pending_approval = None;
        view.diff_view = None;
        self.runtime
            .respond_to_session_approval(session_id.clone(), request_id, decision)
            .await?;
        self.update_session_status(&session_id, SessionStatus::Running);
        self.sync_mode_with_state();
        Ok(())
    }

    async fn cancel_or_exit(&mut self) -> Result<()> {
        if self
            .active_view()
            .and_then(|view| view.diff_view.as_ref())
            .is_some()
        {
            self.close_diff_view();
            return Ok(());
        }
        if self.session_picker.is_some() {
            self.close_session_picker();
            return Ok(());
        }
        if self.command_palette.is_some() {
            self.command_palette = None;
            self.sync_mode_with_state();
            return Ok(());
        }
        if self.memory_view.is_some() {
            self.memory_view = None;
            self.sync_mode_with_state();
            return Ok(());
        }
        if self.settings_view.is_some() {
            self.settings_view = None;
            self.sync_mode_with_state();
            return Ok(());
        }
        if self.show_help {
            self.show_help = false;
            self.sync_mode_with_state();
            return Ok(());
        }
        if self.active_session_is_busy() {
            self.runtime
                .hard_cancel_session(self.active_session_id.clone())
                .await?;
            self.push_status_line(
                self.active_session_id.clone(),
                "Cancelled current generation.".to_string(),
            );
            self.update_session_status(&self.active_session_id.clone(), SessionStatus::Cancelled);
        } else {
            self.should_exit = true;
        }
        self.sync_mode_with_state();
        Ok(())
    }

    async fn handle_pending_chord(&mut self, key: KeyEvent) -> Result<bool> {
        let Some(chord) = self.pending_chord else {
            return Ok(false);
        };

        if matches!(key.code, KeyCode::Esc) {
            self.pending_chord = None;
            self.sync_mode_with_state();
            return Ok(true);
        }

        let matched = matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S'));
        let help_matched = matches!(key.code, KeyCode::Char('h') | KeyCode::Char('H'));
        let sidebar_matched = matches!(key.code, KeyCode::Char('b') | KeyCode::Char('B'));
        if matched || help_matched || sidebar_matched {
            self.pending_chord = None;
            match chord {
                PendingChord::OpenSessions if matched => self.open_session_picker(),
                PendingChord::Leader if matched => {
                    self.runtime
                        .soft_cancel_session(self.active_session_id.clone())
                        .await?;
                    self.push_status_line(
                        self.active_session_id.clone(),
                        "Stop requested. MOA will stop after the current step.".to_string(),
                    );
                }
                PendingChord::Leader if help_matched => {
                    self.show_help = !self.show_help;
                }
                PendingChord::Leader if sidebar_matched => {
                    let visible = sidebar::should_show_sidebar(
                        self.viewport_width,
                        self.config.tui.sidebar_auto,
                        self.sidebar_override,
                    );
                    self.sidebar_override = Some(!visible);
                }
                _ => {}
            }
            self.sync_mode_with_state();
            return Ok(true);
        }

        self.pending_chord = None;
        Ok(false)
    }

    fn move_picker(&mut self, down: bool) {
        let Some(picker) = &mut self.session_picker else {
            return;
        };
        let len = sessions::filtered_sessions(picker.query(), &self.sessions).len();
        if down {
            picker.move_down(len);
        } else {
            picker.move_up(len);
        }
        self.sync_mode_with_state();
    }

    fn handle_picker_input(&mut self, key: KeyEvent) {
        let Some(picker) = &mut self.session_picker else {
            return;
        };
        match key.code {
            KeyCode::Char(ch) => picker.push_char(ch),
            KeyCode::Backspace | KeyCode::Delete => picker.backspace(),
            _ => {}
        }
        picker.clamp_selection(sessions::filtered_sessions(picker.query(), &self.sessions).len());
        self.sync_mode_with_state();
    }

    fn update_picker_query(&mut self, ch: Option<char>) {
        let Some(picker) = &mut self.session_picker else {
            return;
        };
        match ch {
            Some(ch) => picker.push_char(ch),
            None => picker.backspace(),
        }
        picker.clamp_selection(sessions::filtered_sessions(picker.query(), &self.sessions).len());
        self.sync_mode_with_state();
    }

    async fn confirm_picker_selection(&mut self) -> Result<()> {
        let Some(picker) = &self.session_picker else {
            return Ok(());
        };
        if let Some(session_id) = picker.selected_session_id(&self.sessions) {
            self.close_session_picker();
            self.switch_to_session(session_id).await?;
        }
        Ok(())
    }

    fn open_session_picker(&mut self) {
        self.show_help = false;
        self.command_palette = None;
        self.memory_view = None;
        self.settings_view = None;
        self.prompt_completion = None;
        let mut picker = SessionPickerState::new();
        picker.clamp_selection(self.sessions.len());
        self.session_picker = Some(picker);
        self.sync_mode_with_state();
    }

    fn close_session_picker(&mut self) {
        self.session_picker = None;
        self.sync_mode_with_state();
    }

    fn open_diff_view(&mut self) {
        let session_id = self.active_session_id.clone();
        let Some(prompt) = self
            .session_views
            .get(&session_id)
            .and_then(|view| view.pending_approval.clone())
        else {
            return;
        };
        self.session_view_mut(session_id).diff_view =
            DiffViewState::from_file_diffs(&prompt.file_diffs, self.viewport_width);
        self.sync_mode_with_state();
    }

    fn close_diff_view(&mut self) {
        self.session_view_mut(self.active_session_id.clone())
            .diff_view = None;
        self.sync_mode_with_state();
    }

    fn toggle_diff_mode(&mut self) {
        let viewport_width = self.viewport_width;
        if let Some(diff_view) = &mut self
            .session_view_mut(self.active_session_id.clone())
            .diff_view
        {
            diff_view.toggle_mode(viewport_width);
        }
    }

    fn move_diff_file(&mut self, forward: bool) {
        if let Some(diff_view) = &mut self
            .session_view_mut(self.active_session_id.clone())
            .diff_view
        {
            if forward {
                diff_view.next_file();
            } else {
                diff_view.previous_file();
            }
        }
    }

    fn move_diff_hunk(&mut self, forward: bool) {
        if let Some(diff_view) = &mut self
            .session_view_mut(self.active_session_id.clone())
            .diff_view
        {
            if forward {
                diff_view.next_hunk();
            } else {
                diff_view.previous_hunk();
            }
        }
    }

    fn footer_text(&self) -> String {
        if let Some(chord) = self.pending_chord {
            return match chord {
                PendingChord::OpenSessions => "Press S to open the session picker.".to_string(),
                PendingChord::Leader => "Ctrl+X chord: S stop  H help  B sidebar".to_string(),
            };
        }
        if self.show_help {
            return "Esc close  Ctrl+P palette  Ctrl+M memory  Ctrl+, settings  Ctrl+X, B sidebar"
                .to_string();
        }
        if self.command_palette.is_some() {
            return "Enter run  Esc close  ↑/↓ select  Type to filter".to_string();
        }
        if self.memory_view.is_some() {
            return "/ search  Enter open  Alt+←/→ history  e edit  d delete  Esc close"
                .to_string();
        }
        if self.settings_view.is_some() {
            return "←/→ adjust  h/l category  ↑/↓ field  Enter apply  Esc close".to_string();
        }
        if self.session_picker.is_some() {
            return "Enter: open  Esc: close  ↑/↓: select  Type to filter".to_string();
        }
        if self
            .active_view()
            .and_then(|view| view.diff_view.as_ref())
            .is_some()
        {
            return "t: toggle  n/N: files  j/k: hunks  a: accept  r: reject  Esc: close"
                .to_string();
        }
        if self.active_session_is_busy() {
            return "Ctrl+Q queue  Ctrl+X, S stop  Ctrl+P palette  Ctrl+M memory  Ctrl+C/Esc cancel"
                .to_string();
        }
        "Enter send  Shift+Enter newline  Tab complete  Ctrl+P palette  Ctrl+M memory  /help"
            .to_string()
    }

    fn push_status_line(&mut self, session_id: SessionId, text: String) {
        let view = self.session_view_mut(session_id);
        view.entries.push(ChatEntry::Status(text));
        view.auto_scroll = true;
    }

    fn sync_mode_with_state(&mut self) {
        self.mode = if self.show_help {
            AppMode::Help
        } else if self.command_palette.is_some() {
            AppMode::CommandPalette
        } else if self.memory_view.is_some() {
            AppMode::MemoryBrowser
        } else if self.settings_view.is_some() {
            AppMode::Settings
        } else if self.session_picker.is_some() {
            AppMode::PickingSession
        } else if self
            .active_view()
            .and_then(|view| view.diff_view.as_ref())
            .is_some()
        {
            AppMode::ViewingDiff
        } else if self
            .active_view()
            .and_then(|view| view.pending_approval.as_ref())
            .is_some()
        {
            AppMode::WaitingApproval
        } else if self.active_session_is_busy() {
            AppMode::Running
        } else if self.prompt.text().trim().is_empty() {
            AppMode::Idle
        } else {
            AppMode::Composing
        };
    }

    fn active_session_is_busy(&self) -> bool {
        matches!(
            self.active_session_preview(&self.active_session_id)
                .map(|preview| &preview.summary.status),
            Some(SessionStatus::Running | SessionStatus::WaitingApproval | SessionStatus::Paused)
        )
    }

    fn active_session_is_live(&self, session_id: SessionId) -> bool {
        matches!(
            self.active_session_preview(&session_id)
                .map(|preview| &preview.summary.status),
            Some(SessionStatus::Running | SessionStatus::WaitingApproval | SessionStatus::Paused)
        )
    }

    fn update_session_status(&mut self, session_id: &SessionId, status: SessionStatus) {
        if let Some(preview) = self
            .sessions
            .iter_mut()
            .find(|preview| preview.summary.session_id == *session_id)
        {
            preview.summary.status = status.clone();
        }
        if let Some(meta) = self.session_meta.get_mut(session_id) {
            meta.status = status;
        }
    }

    fn active_session_preview(&self, session_id: &SessionId) -> Option<&SessionPreview> {
        self.sessions
            .iter()
            .find(|preview| preview.summary.session_id == *session_id)
    }

    fn active_view(&self) -> Option<&SessionViewState> {
        self.session_views.get(&self.active_session_id)
    }

    fn active_view_mut(&mut self) -> Option<&mut SessionViewState> {
        self.session_views.get_mut(&self.active_session_id)
    }

    fn session_view_mut(&mut self, session_id: SessionId) -> &mut SessionViewState {
        self.session_views.entry(session_id).or_default()
    }

    fn active_session_meta(&self) -> Option<&SessionMeta> {
        self.session_meta.get(&self.active_session_id)
    }

    async fn refresh_sidebar_data(&mut self) -> Result<()> {
        self.recent_memory = self
            .runtime
            .recent_memory_entries(6)
            .await
            .unwrap_or_default();
        self.tool_names = self.runtime.tool_names_async().await.unwrap_or_default();
        self.known_files = collect_sandbox_files(&self.runtime.sandbox_root())
            .await
            .unwrap_or_else(|_| self.known_files.clone());
        if let Some(memory_view) = &mut self.memory_view {
            memory_view.set_pages(
                self.runtime
                    .list_memory_pages(None)
                    .await
                    .unwrap_or_default(),
            );
        }
        Ok(())
    }

    async fn refresh_prompt_completion(&mut self) -> Result<()> {
        self.prompt_completion =
            completion_for_prompt(&self.prompt.text(), &self.known_files, &self.file_frecency);
        Ok(())
    }

    async fn accept_prompt_completion(&mut self) -> Result<()> {
        let Some(completion) = &mut self.prompt_completion else {
            return Ok(());
        };
        let Some(item) = completion.items.get(completion.selected).cloned() else {
            return Ok(());
        };
        let prompt = self.prompt.text();
        let replaced = match completion.kind {
            PromptCompletionKind::Slash => complete_slash_command(&prompt, &item),
            PromptCompletionKind::File => {
                *self.file_frecency.entry(item.clone()).or_insert(0) += 1;
                complete_file_reference(&prompt, &item)
            }
        };
        self.prompt.replace_text(replaced);
        self.prompt_completion =
            completion_for_prompt(&self.prompt.text(), &self.known_files, &self.file_frecency);
        self.sync_mode_with_state();
        Ok(())
    }

    fn half_page_scroll(&mut self, down: bool) {
        let delta = 8;
        let view = self.session_view_mut(self.active_session_id.clone());
        view.auto_scroll = false;
        if down {
            view.scroll = view.scroll.saturating_add(delta);
        } else {
            view.scroll = view.scroll.saturating_sub(delta);
        }
    }

    async fn reload_active_session(&mut self) -> Result<()> {
        self.session_views.remove(&self.active_session_id.clone());
        self.load_session_if_needed(self.active_session_id.clone())
            .await?;
        self.sync_mode_with_state();
        Ok(())
    }

    fn open_command_palette(&mut self) {
        self.show_help = false;
        self.memory_view = None;
        self.settings_view = None;
        self.session_picker = None;
        self.prompt_completion = None;
        self.command_palette = Some(PaletteState::new());
        self.sync_mode_with_state();
    }

    fn move_palette(&mut self, down: bool) {
        let Some(palette) = &mut self.command_palette else {
            return;
        };
        let len = palette::filtered_actions(palette.query(), &palette_actions()).len();
        if down {
            palette.move_down(len);
        } else {
            palette.move_up(len);
        }
        self.sync_mode_with_state();
    }

    fn handle_palette_input(&mut self, key: KeyEvent) {
        let Some(palette) = &mut self.command_palette else {
            return;
        };
        if let KeyCode::Char(ch) = key.code {
            palette.push_char(ch);
        }
        self.sync_mode_with_state();
    }

    fn handle_palette_backspace(&mut self) {
        if let Some(palette) = &mut self.command_palette {
            palette.backspace();
        }
        self.sync_mode_with_state();
    }

    async fn select_palette_action(&mut self) -> Result<()> {
        let Some(action) = self
            .command_palette
            .as_ref()
            .and_then(|palette| palette.selected_action(&palette_actions()))
        else {
            return Ok(());
        };
        self.command_palette = None;
        self.execute_palette_action(action).await?;
        self.sync_mode_with_state();
        Ok(())
    }

    async fn execute_palette_action(&mut self, action: PaletteAction) -> Result<()> {
        match action.id {
            "new_session" => self.create_new_session().await?,
            "sessions" => self.open_session_picker(),
            "memory" => self.open_memory_browser().await?,
            "settings" => self.open_settings(),
            "help" => self.show_help = true,
            "toggle_sidebar" => {
                let visible = sidebar::should_show_sidebar(
                    self.viewport_width,
                    self.config.tui.sidebar_auto,
                    self.sidebar_override,
                );
                self.sidebar_override = Some(!visible);
            }
            "clear" => self.reload_active_session().await?,
            "quit" => self.should_exit = true,
            _ => {}
        }
        Ok(())
    }

    async fn open_memory_browser(&mut self) -> Result<()> {
        self.show_help = false;
        self.command_palette = None;
        self.settings_view = None;
        self.session_picker = None;
        self.prompt_completion = None;
        let pages = self
            .runtime
            .list_memory_pages(None)
            .await
            .unwrap_or_default();
        let mut state = MemoryViewState::new(pages);
        let initial_path = state
            .selected_path()
            .or_else(|| Some(MemoryPath::new("MEMORY.md")));
        if let Some(path) = initial_path
            && let Ok(page) = self.runtime.read_memory_page(&path).await
        {
            state.set_current_page(page);
        }
        self.memory_view = Some(state);
        self.sync_mode_with_state();
        Ok(())
    }

    fn start_memory_search(&mut self) {
        if let Some(memory_view) = &mut self.memory_view {
            memory_view.start_search();
        }
        self.sync_mode_with_state();
    }

    async fn memory_search_backspace(&mut self) -> Result<()> {
        if let Some(memory_view) = &mut self.memory_view
            && memory_view.search_mode()
        {
            memory_view.backspace_query();
            self.refresh_memory_search_results().await?;
        }
        self.sync_mode_with_state();
        Ok(())
    }

    async fn memory_search_input(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(memory_view) = &mut self.memory_view
            && memory_view.search_mode()
            && let KeyCode::Char(ch) = key.code
        {
            memory_view.push_query_char(ch);
            self.refresh_memory_search_results().await?;
        }
        self.sync_mode_with_state();
        Ok(())
    }

    async fn refresh_memory_search_results(&mut self) -> Result<()> {
        let Some(memory_view) = &mut self.memory_view else {
            return Ok(());
        };
        if memory_view.query().trim().is_empty() {
            memory_view.set_search_results(Vec::new());
        } else {
            memory_view.set_search_results(
                self.runtime
                    .search_memory(memory_view.query(), 24)
                    .await
                    .unwrap_or_default(),
            );
        }
        Ok(())
    }

    fn move_memory(&mut self, down: bool) {
        let Some(memory_view) = &mut self.memory_view else {
            return;
        };
        if down {
            memory_view.move_down();
        } else {
            memory_view.move_up();
        }
        self.sync_mode_with_state();
    }

    async fn open_selected_memory_item(&mut self) -> Result<()> {
        let Some(memory_view) = &mut self.memory_view else {
            return Ok(());
        };
        let Some(path) = memory_view.selected_path() else {
            return Ok(());
        };
        memory_view.record_open(&path);
        if let Ok(page) = self.runtime.read_memory_page(&path).await {
            memory_view.set_current_page(page);
            memory_view.stop_search();
        }
        self.sync_mode_with_state();
        Ok(())
    }

    async fn navigate_memory_history(&mut self, forward: bool) -> Result<()> {
        let Some(memory_view) = &mut self.memory_view else {
            return Ok(());
        };
        let path = if forward {
            memory_view.go_forward()
        } else {
            memory_view.go_back()
        };
        if let Some(path) = path
            && let Ok(page) = self.runtime.read_memory_page(&path).await
        {
            memory_view.set_current_page(page);
        }
        self.sync_mode_with_state();
        Ok(())
    }

    async fn open_memory_page_in_editor(&mut self) -> Result<()> {
        self.push_status_line(
            self.active_session_id.clone(),
            "Opening memory pages in an external editor is not implemented yet.".to_string(),
        );
        self.sync_mode_with_state();
        Ok(())
    }

    async fn delete_memory_page(&mut self) -> Result<()> {
        self.push_status_line(
            self.active_session_id.clone(),
            "Deleting memory pages from the TUI is not implemented yet.".to_string(),
        );
        self.sync_mode_with_state();
        Ok(())
    }

    fn open_settings(&mut self) {
        self.show_help = false;
        self.command_palette = None;
        self.memory_view = None;
        self.session_picker = None;
        self.prompt_completion = None;
        self.settings_view = Some(SettingsViewState::new());
        self.sync_mode_with_state();
    }

    fn move_settings(&mut self, down: bool) {
        let Some(settings_view) = &mut self.settings_view else {
            return;
        };
        if down {
            settings_view.move_down(&self.config);
        } else {
            settings_view.move_up(&self.config);
        }
        self.sync_mode_with_state();
    }

    fn move_settings_category(&mut self, forward: bool) {
        let Some(settings_view) = &mut self.settings_view else {
            return;
        };
        if forward {
            settings_view.move_right();
        } else {
            settings_view.move_left();
        }
        self.sync_mode_with_state();
    }

    async fn apply_settings_change(&mut self, forward: bool) -> Result<()> {
        let Some(settings_view) = &self.settings_view else {
            return Ok(());
        };
        let mutation = if forward {
            settings_view.step_forward(&mut self.config)
        } else {
            settings_view.step_backward(&mut self.config)
        };
        self.persist_config().await?;
        if mutation == SettingsMutation::ModelChanged {
            let session_id = self
                .runtime
                .set_model(self.config.general.default_model.clone())
                .await?;
            self.refresh_session_list().await?;
            self.switch_to_session(session_id).await?;
        }
        self.sync_mode_with_state();
        Ok(())
    }

    async fn persist_config(&self) -> Result<()> {
        if let Some(parent) = self.config_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let content = toml::to_string_pretty(&self.config)
            .map_err(|error| moa_core::MoaError::ConfigError(error.to_string()))?;
        fs::write(&self.config_path, content).await?;
        Ok(())
    }
}

impl SessionViewState {
    fn from_history(
        meta: &SessionMeta,
        events: &[EventRecord],
        cached_prompt: Option<ApprovalPrompt>,
    ) -> Self {
        let mut view = Self {
            entries: Vec::new(),
            total_tokens: meta.total_input_tokens + meta.total_output_tokens,
            scroll: 0,
            auto_scroll: true,
            pending_approval: None,
            diff_view: None,
            loaded: true,
        };
        let cached_request_id = cached_prompt
            .as_ref()
            .map(|prompt| prompt.request.request_id);

        for record in events {
            match &record.event {
                Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
                    view.entries.push(ChatEntry::User(text.clone()));
                }
                Event::BrainResponse { text, .. } => view.entries.push(ChatEntry::Assistant {
                    text: text.clone(),
                    streaming: false,
                }),
                Event::ToolCall {
                    tool_id, tool_name, ..
                } => {
                    view.upsert_tool_card(ToolUpdate {
                        tool_id: *tool_id,
                        tool_name: tool_name.clone(),
                        status: ToolCardStatus::Running,
                        summary: format!("{tool_name} requested"),
                        detail: None,
                    });
                }
                Event::ToolResult {
                    tool_id,
                    output,
                    success,
                    duration_ms,
                } => {
                    view.upsert_tool_card(ToolUpdate {
                        tool_id: *tool_id,
                        tool_name: existing_tool_name(&view.entries, tool_id),
                        status: if *success {
                            ToolCardStatus::Succeeded
                        } else {
                            ToolCardStatus::Failed
                        },
                        summary: format!("completed in {duration_ms} ms"),
                        detail: Some(truncate_detail(&output.to_text())),
                    });
                }
                Event::ToolError { tool_id, error, .. } => {
                    view.upsert_tool_card(ToolUpdate {
                        tool_id: *tool_id,
                        tool_name: existing_tool_name(&view.entries, tool_id),
                        status: ToolCardStatus::Failed,
                        summary: "tool failed".to_string(),
                        detail: Some(error.clone()),
                    });
                }
                Event::ApprovalRequested {
                    request_id,
                    tool_name,
                    input_summary,
                    risk_level,
                    prompt,
                } => {
                    let prompt = if let Some(prompt) = prompt.clone() {
                        prompt
                    } else if cached_request_id == Some(*request_id) {
                        cached_prompt.clone().unwrap_or_else(|| {
                            minimal_approval_prompt(
                                *request_id,
                                tool_name,
                                input_summary,
                                risk_level,
                            )
                        })
                    } else {
                        minimal_approval_prompt(*request_id, tool_name, input_summary, risk_level)
                    };
                    view.upsert_approval_card(prompt.clone());
                    view.pending_approval = Some(prompt);
                }
                Event::ApprovalDecided {
                    request_id,
                    decision,
                    ..
                } => {
                    let (status, note) = approval_status_and_note(decision);
                    view.update_approval_entry(*request_id, status, note);
                    if view
                        .pending_approval
                        .as_ref()
                        .map(|prompt| prompt.request.request_id == *request_id)
                        .unwrap_or(false)
                    {
                        view.pending_approval = None;
                    }
                }
                Event::Error { message, .. } => {
                    view.entries.push(ChatEntry::Status(message.clone()));
                }
                Event::Warning { message } => {
                    view.entries
                        .push(ChatEntry::Status(format!("Warning: {message}")));
                }
                Event::SessionCompleted { summary, .. } => {
                    view.entries
                        .push(ChatEntry::Status(format!("Completed: {summary}")));
                }
                _ => {}
            }
        }

        view
    }

    fn handle_tool_update(&mut self, update: ToolUpdate) {
        if update.status == ToolCardStatus::WaitingApproval {
            return;
        }

        if update.status == ToolCardStatus::Failed
            && self.approval_status(update.tool_id) == Some(ApprovalCardStatus::Denied)
        {
            return;
        }

        self.upsert_tool_card(update);
        self.auto_scroll = true;
    }

    fn upsert_tool_card(&mut self, update: ToolUpdate) {
        let entry = ToolCardEntry {
            tool_id: update.tool_id,
            tool_name: update.tool_name,
            status: update.status,
            summary: update.summary,
            detail: update.detail,
        };

        if let Some(ChatEntry::Tool(existing)) = self
            .entries
            .iter_mut()
            .find(|entry| matches!(entry, ChatEntry::Tool(card) if card.tool_id == update.tool_id))
        {
            *existing = entry;
        } else {
            self.entries.push(ChatEntry::Tool(entry));
        }
    }

    fn upsert_approval_card(&mut self, prompt: ApprovalPrompt) {
        let request_id = prompt.request.request_id;
        let entry = ApprovalEntry {
            prompt,
            status: ApprovalCardStatus::Pending,
            note: None,
        };

        if let Some(ChatEntry::Approval(existing)) = self.entries.iter_mut().find(|entry| {
            matches!(
                entry,
                ChatEntry::Approval(card) if card.prompt.request.request_id == request_id
            )
        }) {
            *existing = entry;
        } else {
            self.entries.push(ChatEntry::Approval(entry));
        }
    }

    fn update_approval_entry(
        &mut self,
        request_id: Uuid,
        status: ApprovalCardStatus,
        note: Option<String>,
    ) {
        if let Some(ChatEntry::Approval(entry)) = self.entries.iter_mut().find(|entry| {
            matches!(
                entry,
                ChatEntry::Approval(card) if card.prompt.request.request_id == request_id
            )
        }) {
            entry.status = status;
            entry.note = note;
        }
    }

    fn approval_status(&self, request_id: Uuid) -> Option<ApprovalCardStatus> {
        self.entries.iter().find_map(|entry| match entry {
            ChatEntry::Approval(approval) if approval.prompt.request.request_id == request_id => {
                Some(approval.status)
            }
            _ => None,
        })
    }
}

fn existing_tool_name(entries: &[ChatEntry], tool_id: &Uuid) -> String {
    entries
        .iter()
        .find_map(|entry| match entry {
            ChatEntry::Tool(card) if card.tool_id == *tool_id => Some(card.tool_name.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "tool".to_string())
}

fn truncate_detail(detail: &str) -> String {
    const MAX_DETAIL_CHARS: usize = 240;
    let mut truncated = detail.chars().take(MAX_DETAIL_CHARS).collect::<String>();
    if detail.chars().count() > MAX_DETAIL_CHARS {
        truncated.push('…');
    }
    truncated
}

fn minimal_approval_prompt(
    request_id: Uuid,
    tool_name: &str,
    input_summary: &str,
    risk_level: &RiskLevel,
) -> ApprovalPrompt {
    ApprovalPrompt {
        request: ApprovalRequest {
            request_id,
            tool_name: tool_name.to_string(),
            input_summary: input_summary.to_string(),
            risk_level: risk_level.clone(),
        },
        pattern: input_summary.to_string(),
        parameters: vec![ApprovalField {
            label: "Request".to_string(),
            value: input_summary.to_string(),
        }],
        file_diffs: Vec::<ApprovalFileDiff>::new(),
    }
}

fn session_summary_from_meta(meta: &SessionMeta) -> moa_core::SessionSummary {
    moa_core::SessionSummary {
        session_id: meta.id.clone(),
        workspace_id: meta.workspace_id.clone(),
        user_id: meta.user_id.clone(),
        title: meta.title.clone(),
        status: meta.status.clone(),
        platform: meta.platform.clone(),
        model: meta.model.clone(),
        updated_at: meta.updated_at,
    }
}

fn approval_status_and_note(decision: &ApprovalDecision) -> (ApprovalCardStatus, Option<String>) {
    match decision {
        ApprovalDecision::AllowOnce => (
            ApprovalCardStatus::AllowedOnce,
            Some("Allowed once".to_string()),
        ),
        ApprovalDecision::AlwaysAllow { pattern } => (
            ApprovalCardStatus::AllowedAlways,
            Some(format!("Always allow rule stored: {pattern}")),
        ),
        ApprovalDecision::Deny { reason } => (
            ApprovalCardStatus::Denied,
            Some(
                reason
                    .clone()
                    .unwrap_or_else(|| "Denied by the user".to_string()),
            ),
        ),
    }
}

fn palette_actions() -> Vec<PaletteAction> {
    vec![
        PaletteAction {
            id: "new_session",
            label: "New Session",
            shortcut: "Ctrl+N",
            description: "Start a fresh chat session",
        },
        PaletteAction {
            id: "sessions",
            label: "Open Sessions",
            shortcut: "Ctrl+O, S",
            description: "Open the fuzzy session picker",
        },
        PaletteAction {
            id: "memory",
            label: "Open Memory Browser",
            shortcut: "Ctrl+M",
            description: "Browse workspace memory pages",
        },
        PaletteAction {
            id: "settings",
            label: "Open Settings",
            shortcut: "Ctrl+,",
            description: "Edit configuration values",
        },
        PaletteAction {
            id: "toggle_sidebar",
            label: "Toggle Sidebar",
            shortcut: "Ctrl+X, B",
            description: "Show or hide the sidebar",
        },
        PaletteAction {
            id: "help",
            label: "Show Help",
            shortcut: "Ctrl+X, H",
            description: "Open the shortcut reference",
        },
        PaletteAction {
            id: "clear",
            label: "Clear Screen",
            shortcut: "Ctrl+L",
            description: "Reload the active session view",
        },
        PaletteAction {
            id: "quit",
            label: "Quit",
            shortcut: "/quit",
            description: "Exit the TUI",
        },
    ]
}

fn render_help_overlay(frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    use ratatui::{
        layout::{Constraint, Direction, Layout},
        widgets::{Block, Borders, Clear, Paragraph},
    };

    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(12),
            Constraint::Percentage(76),
            Constraint::Percentage(12),
        ])
        .split(area)[1];
    let popup = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(15),
            Constraint::Percentage(70),
            Constraint::Percentage(15),
        ])
        .split(popup)[1];

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(
            "Ctrl+P command palette\n\
             Ctrl+M memory browser\n\
             Ctrl+, settings\n\
             Ctrl+X, H help\n\
             Ctrl+X, B toggle sidebar\n\
             Ctrl+O, S session picker\n\
             Ctrl+Q queue prompt\n\
             Ctrl+X, S soft stop\n\
             Tab autocomplete (/command, @file)\n\
             Alt+1-9 switch tabs\n\
             Alt+[ / Alt+] cycle tabs\n\
             Esc closes overlays",
        )
        .block(Block::default().borders(Borders::ALL).title("Help")),
        popup,
    );
}

fn completion_for_prompt(
    prompt: &str,
    known_files: &[String],
    file_frecency: &HashMap<String, u32>,
) -> Option<PromptCompletionState> {
    let trimmed = prompt.trim_start();
    if trimmed.starts_with('/') && !trimmed.contains(char::is_whitespace) {
        let prefix = trimmed.trim_start_matches('/');
        let mut items = slash_commands()
            .into_iter()
            .filter(|command| command.trim_start_matches('/').starts_with(prefix))
            .map(str::to_string)
            .collect::<Vec<_>>();
        if items.is_empty() {
            return None;
        }
        items.sort();
        return Some(PromptCompletionState {
            kind: PromptCompletionKind::Slash,
            items,
            selected: 0,
        });
    }

    let token = prompt
        .split_whitespace()
        .last()
        .filter(|token| token.starts_with('@'))?;
    let prefix = token.trim_start_matches('@');
    let mut items = known_files
        .iter()
        .filter(|path| path.starts_with(prefix))
        .cloned()
        .collect::<Vec<_>>();
    if items.is_empty() {
        return None;
    }
    items.sort_by(|left, right| {
        file_frecency
            .get(right)
            .unwrap_or(&0)
            .cmp(file_frecency.get(left).unwrap_or(&0))
            .then_with(|| left.cmp(right))
    });
    Some(PromptCompletionState {
        kind: PromptCompletionKind::File,
        items,
        selected: 0,
    })
}

fn complete_slash_command(prompt: &str, command: &str) -> String {
    let _ = prompt;
    format!("{command} ")
}

fn complete_file_reference(prompt: &str, path: &str) -> String {
    let token = prompt.split_whitespace().last().unwrap_or_default();
    if !token.starts_with('@') {
        return prompt.to_string();
    }

    let replacement = format!("@{path}");
    let trimmed = prompt.trim_end();
    let prefix = trimmed.strip_suffix(token).unwrap_or(trimmed).to_string();
    format!("{prefix}{replacement} ")
}

fn slash_commands() -> Vec<&'static str> {
    vec![
        "/new",
        "/sessions",
        "/resume",
        "/model",
        "/memory",
        "/workspace",
        "/tools",
        "/settings",
        "/compact",
        "/export",
        "/undo",
        "/redo",
        "/clear",
        "/editor",
        "/status",
        "/help",
        "/quit",
    ]
}

fn default_config_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or(moa_core::MoaError::HomeDirectoryNotFound)?;
    Ok(Path::new(&home).join(".moa").join("config.toml"))
}

fn workspace_name_from_input(input: &str) -> WorkspaceId {
    let normalized = Path::new(input)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(input)
        .trim();
    WorkspaceId::new(if normalized.is_empty() {
        "default"
    } else {
        normalized
    })
}

async fn collect_sandbox_files(root: &Path) -> Result<Vec<String>> {
    let mut results = Vec::new();
    let root = root.to_path_buf();
    if !fs::try_exists(&root).await? {
        return Ok(results);
    }

    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                pending.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if let Ok(relative) = path.strip_prefix(&root) {
                results.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    results.sort();
    Ok(results)
}

/// Runs the full-screen TUI until the user exits.
pub async fn run_tui(config: MoaConfig) -> Result<()> {
    run_tui_with_options(config, RunTuiOptions::default()).await
}

/// Runs the full-screen TUI with explicit launch options.
pub async fn run_tui_with_options(config: MoaConfig, options: RunTuiOptions) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let runtime = if options.force_daemon {
        if let Some(session_id) = options.attach_session_id.clone() {
            ChatRuntime::attach_to_daemon_session(
                config.clone(),
                moa_core::Platform::Tui,
                session_id,
            )
            .await?
        } else {
            ChatRuntime::from_daemon_config(config.clone(), moa_core::Platform::Tui).await?
        }
    } else if let Some(session_id) = options.attach_session_id.clone() {
        ChatRuntime::attach_to_local_session(config.clone(), moa_core::Platform::Tui, session_id)
            .await?
    } else {
        ChatRuntime::from_config(config.clone(), moa_core::Platform::Tui).await?
    };
    let mut app = App::from_runtime(config, runtime, options).await?;

    let result = run_event_loop(&mut terminal, &mut app).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    loop {
        app.drain_runtime_events();
        app.refresh_sessions_if_due().await?;
        terminal.draw(|frame| app.draw(frame))?;

        if app.should_exit() {
            return Ok(());
        }

        if event::poll(FRAME_DURATION)?
            && let CrosstermEvent::Key(key) = event::read()?
        {
            app.handle_key_event(key).await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use crossterm::event::KeyCode;
    use moa_core::{Platform, SessionStore, SessionSummary, UserId, WorkspaceId};
    use ratatui::{Terminal, backend::TestBackend};
    use tokio::runtime::Runtime;

    use super::*;

    fn test_app() -> App {
        Runtime::new().expect("runtime").block_on(async {
            let runtime = ChatRuntime::for_test(Platform::Tui).await.expect("runtime");
            let active_session_id = runtime.session_id().clone();
            let config = runtime.config().clone();
            let config_path =
                std::env::temp_dir().join(format!("moa-config-{}.toml", Uuid::new_v4()));
            let (runtime_event_tx, runtime_event_rx) = mpsc::unbounded_channel();
            let mut app = App {
                config,
                config_path,
                runtime,
                mode: AppMode::Idle,
                prompt: PromptWidget::new(),
                sessions: vec![SessionPreview {
                    summary: SessionSummary {
                        session_id: active_session_id.clone(),
                        workspace_id: WorkspaceId::new("default"),
                        user_id: UserId::new("tester"),
                        title: Some("Session 1".to_string()),
                        status: SessionStatus::Created,
                        platform: Platform::Tui,
                        model: "claude-sonnet-4-6".to_string(),
                        updated_at: Utc::now(),
                    },
                    last_message: None,
                }],
                session_meta: HashMap::new(),
                session_views: HashMap::from([(
                    active_session_id.clone(),
                    SessionViewState::default(),
                )]),
                memory_view: None,
                settings_view: None,
                command_palette: None,
                prompt_completion: None,
                sidebar_override: None,
                show_help: false,
                recent_memory: Vec::new(),
                tool_names: vec!["bash".to_string(), "file_read".to_string()],
                file_frecency: HashMap::new(),
                known_files: vec![
                    "src/main.rs".to_string(),
                    "src/lib.rs".to_string(),
                    "README.md".to_string(),
                ],
                active_session_id,
                viewport_width: 100,
                should_exit: false,
                pending_chord: None,
                session_picker: None,
                runtime_event_tx,
                runtime_event_rx,
                observation_task: None,
                observed_session_id: None,
                last_session_refresh: Instant::now(),
            };
            app.sync_mode_with_state();
            app
        })
    }

    #[test]
    fn app_state_transitions_follow_idle_composing_running_waiting_idle() {
        let mut app = test_app();
        assert_eq!(app.mode(), AppMode::Idle);

        app.prompt.input(KeyEvent::from(KeyCode::Char('h')));
        app.sync_mode_with_state();
        assert_eq!(app.mode(), AppMode::Composing);

        app.update_session_status(&app.active_session_id.clone(), SessionStatus::Running);
        app.sync_mode_with_state();
        assert_eq!(app.mode(), AppMode::Running);

        app.session_view_mut(app.active_session_id.clone())
            .pending_approval = Some(minimal_approval_prompt(
            Uuid::new_v4(),
            "bash",
            "pwd",
            &RiskLevel::High,
        ));
        app.sync_mode_with_state();
        assert_eq!(app.mode(), AppMode::WaitingApproval);

        app.session_view_mut(app.active_session_id.clone())
            .pending_approval = None;
        app.update_session_status(&app.active_session_id.clone(), SessionStatus::Completed);
        app.prompt.clear();
        app.sync_mode_with_state();
        assert_eq!(app.mode(), AppMode::Idle);
    }

    #[test]
    fn rendering_smoke_test_does_not_panic() {
        let backend = TestBackend::new(120, 35);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = test_app();
        let session_id = app.active_session_id.clone();
        let view = app.session_view_mut(session_id);
        view.entries.push(ChatEntry::User("Hello".to_string()));
        view.entries.push(ChatEntry::Assistant {
            text: "Hi there".to_string(),
            streaming: false,
        });

        terminal.draw(|frame| app.draw(frame)).expect("draw");
    }

    #[test]
    fn diff_overlay_renders_for_file_write_approval() {
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut app = test_app();
        let prompt = ApprovalPrompt {
            request: ApprovalRequest {
                request_id: Uuid::new_v4(),
                tool_name: "file_write".to_string(),
                input_summary: "Path: scratch-step09.txt".to_string(),
                risk_level: RiskLevel::Medium,
            },
            pattern: "scratch-step09.txt".to_string(),
            parameters: vec![
                ApprovalField {
                    label: "Path".to_string(),
                    value: "scratch-step09.txt".to_string(),
                },
                ApprovalField {
                    label: "Content".to_string(),
                    value: "11 chars".to_string(),
                },
            ],
            file_diffs: vec![ApprovalFileDiff {
                path: "scratch-step09.txt".to_string(),
                before: String::new(),
                after: "alpha\nbeta\n".to_string(),
                language_hint: Some("txt".to_string()),
            }],
        };

        let session_id = app.active_session_id.clone();
        let view = app.session_view_mut(session_id);
        view.pending_approval = Some(prompt.clone());
        view.upsert_approval_card(prompt);
        app.open_diff_view();
        assert_eq!(app.mode(), AppMode::ViewingDiff);

        terminal.draw(|frame| app.draw(frame)).expect("draw");
    }

    #[test]
    fn switching_back_to_live_session_reloads_pending_approval_from_history() {
        let runtime = Runtime::new().expect("runtime");
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = MoaConfig::default();
        config.database.url = dir.path().join("sessions.db").display().to_string();
        config.local.memory_dir = dir.path().join("memory").display().to_string();
        config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();

        runtime.block_on(async move {
            let mut app = App::new(config.clone()).await.expect("app");
            let first_session_id = app.active_session_id.clone();
            app.create_new_session().await.expect("new session");
            let second_session_id = app.active_session_id.clone();
            assert_ne!(first_session_id, second_session_id);

            let store = moa_session::create_session_store(&config)
                .await
                .expect("session store");
            let request_id = Uuid::new_v4();
            store
                .emit_event(
                    first_session_id.clone(),
                    Event::ToolCall {
                        tool_id: request_id,
                        tool_name: "bash".to_string(),
                        input: serde_json::json!({ "cmd": "pwd" }),
                        hand_id: None,
                    },
                )
                .await
                .expect("tool call");
            store
                .emit_event(
                    first_session_id.clone(),
                    Event::ApprovalRequested {
                        request_id,
                        tool_name: "bash".to_string(),
                        input_summary: "pwd".to_string(),
                        risk_level: RiskLevel::High,
                        prompt: Some(ApprovalPrompt {
                            request: ApprovalRequest {
                                request_id,
                                tool_name: "bash".to_string(),
                                input_summary: "pwd".to_string(),
                                risk_level: RiskLevel::High,
                            },
                            pattern: "pwd".to_string(),
                            parameters: vec![ApprovalField {
                                label: "Command".to_string(),
                                value: "pwd".to_string(),
                            }],
                            file_diffs: vec![ApprovalFileDiff {
                                path: "scratch.txt".to_string(),
                                before: String::new(),
                                after: "pwd\n".to_string(),
                                language_hint: Some("txt".to_string()),
                            }],
                        }),
                    },
                )
                .await
                .expect("approval request");
            store
                .update_status(first_session_id.clone(), SessionStatus::WaitingApproval)
                .await
                .expect("status");

            app.refresh_session_list().await.expect("refresh");
            app.switch_to_session(first_session_id.clone())
                .await
                .expect("switch");

            assert_eq!(app.mode(), AppMode::WaitingApproval);
            let view = app.active_view().expect("active view");
            assert_eq!(
                view.pending_approval
                    .as_ref()
                    .map(|prompt| prompt.request.request_id),
                Some(request_id)
            );
            assert_eq!(
                view.pending_approval
                    .as_ref()
                    .map(|prompt| prompt.parameters[0].value.clone()),
                Some("pwd".to_string())
            );
            assert_eq!(
                view.pending_approval
                    .as_ref()
                    .map(|prompt| prompt.file_diffs.len()),
                Some(1)
            );
            assert!(view.entries.iter().any(|entry| matches!(
                entry,
                ChatEntry::Approval(card) if card.prompt.request.request_id == request_id
            )));
        });
    }

    #[test]
    fn slash_completion_and_sidebar_toggle_modes_are_reachable() {
        let mut app = test_app();
        app.prompt.replace_text("/me");
        Runtime::new()
            .expect("runtime")
            .block_on(async { app.refresh_prompt_completion().await.expect("completion") });
        assert!(app.prompt_completion.is_some());

        app.sidebar_override = Some(true);
        app.sync_mode_with_state();
        assert_eq!(app.mode(), AppMode::Composing);
    }

    #[test]
    fn memory_and_settings_overlays_change_app_mode() {
        let mut app = test_app();
        app.memory_view = Some(MemoryViewState::default());
        app.sync_mode_with_state();
        assert_eq!(app.mode(), AppMode::MemoryBrowser);

        app.memory_view = None;
        app.settings_view = Some(SettingsViewState::new());
        app.sync_mode_with_state();
        assert_eq!(app.mode(), AppMode::Settings);
    }

    #[test]
    fn settings_persist_to_disk_and_reload() {
        let runtime = Runtime::new().expect("runtime");
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = test_app();
        app.config_path = dir.path().join("config.toml");
        app.config.general.default_provider = "anthropic".to_string();
        app.config.general.default_model = "claude-sonnet-4-6".to_string();
        app.config.tui.sidebar_auto = false;
        runtime.block_on(async { app.persist_config().await.expect("persist config") });

        let reloaded = MoaConfig::load_from_path(&app.config_path).expect("reload config");
        assert_eq!(reloaded.general.default_provider, "anthropic");
        assert_eq!(reloaded.general.default_model, "claude-sonnet-4-6");
        assert!(!reloaded.tui.sidebar_auto);
    }

    #[test]
    fn file_completion_prefers_frecency_and_replaces_active_token() {
        let runtime = Runtime::new().expect("runtime");
        let mut app = test_app();
        app.known_files = vec![
            "src/main.rs".to_string(),
            "src/lib.rs".to_string(),
            "docs/guide.md".to_string(),
        ];
        app.file_frecency.insert("src/lib.rs".to_string(), 5);
        app.prompt.replace_text("@src/l");
        runtime.block_on(async {
            app.refresh_prompt_completion()
                .await
                .expect("refresh completion");
        });

        let completion = app.prompt_completion.clone().expect("completion");
        assert_eq!(completion.kind, PromptCompletionKind::File);
        assert_eq!(
            completion.items.first().map(String::as_str),
            Some("src/lib.rs")
        );

        runtime.block_on(async {
            app.accept_prompt_completion()
                .await
                .expect("accept completion");
        });
        assert_eq!(app.prompt.text(), "@src/lib.rs ");
    }
}
