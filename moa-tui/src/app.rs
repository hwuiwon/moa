//! App state machine and render loop for the multi-session local TUI.

use std::collections::HashMap;
use std::io;
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
use tokio::{sync::mpsc, task::JoinHandle};
use uuid::Uuid;

use moa_core::{
    ApprovalDecision, ApprovalField, ApprovalFileDiff, ApprovalPrompt, ApprovalRequest, Event,
    EventRecord, MoaConfig, Result, RiskLevel, RuntimeEvent, SessionId, SessionMeta, SessionStatus,
    ToolCardStatus, ToolUpdate,
};

use crate::{
    keybindings::{KeyAction, map_key_event},
    runner::{ChatRuntime, SessionPreview, SessionRuntimeEvent},
    views::{
        chat,
        diff::{self, DiffViewState},
        sessions::{self, SessionPickerState},
    },
    widgets::{prompt::PromptWidget, toolbar},
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
    SoftStop,
}

/// Stateful TUI application model.
pub struct App {
    runtime: ChatRuntime,
    mode: AppMode,
    prompt: PromptWidget,
    sessions: Vec<SessionPreview>,
    session_views: HashMap<SessionId, SessionViewState>,
    active_session_id: SessionId,
    viewport_width: u16,
    should_exit: bool,
    pending_chord: Option<PendingChord>,
    session_picker: Option<SessionPickerState>,
    runtime_event_tx: mpsc::UnboundedSender<SessionRuntimeEvent>,
    runtime_event_rx: mpsc::UnboundedReceiver<SessionRuntimeEvent>,
    observation_task: Option<JoinHandle<()>>,
    last_session_refresh: Instant,
}

impl App {
    /// Creates a new TUI app from the loaded MOA config.
    pub async fn new(config: MoaConfig) -> Result<Self> {
        let runtime = ChatRuntime::from_config(config, moa_core::Platform::Tui).await?;
        let active_session_id = runtime.session_id().clone();
        let (runtime_event_tx, runtime_event_rx) = mpsc::unbounded_channel();
        let mut app = Self {
            runtime,
            mode: AppMode::Idle,
            prompt: PromptWidget::new(),
            sessions: Vec::new(),
            session_views: HashMap::new(),
            active_session_id,
            viewport_width: 0,
            should_exit: false,
            pending_chord: None,
            session_picker: None,
            runtime_event_tx,
            runtime_event_rx,
            observation_task: None,
            last_session_refresh: Instant::now() - SESSION_REFRESH_INTERVAL,
        };
        app.refresh_sessions_if_due().await?;
        app.switch_to_session(app.active_session_id.clone()).await?;
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

        match map_key_event(self.mode, key) {
            KeyAction::Submit => self.submit_prompt(false).await?,
            KeyAction::QueuePrompt => self.submit_prompt(true).await?,
            KeyAction::InsertNewline => {
                self.prompt.insert_newline();
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
            KeyAction::NewSession => self.create_new_session().await?,
            KeyAction::NextSession => self.cycle_session(true).await?,
            KeyAction::PreviousSession => self.cycle_session(false).await?,
            KeyAction::SwitchSessionTab(index) => self.switch_tab_by_index(index).await?,
            KeyAction::StartSessionPickerChord => {
                self.pending_chord = Some(PendingChord::OpenSessions);
            }
            KeyAction::StartSoftStopChord => {
                self.pending_chord = Some(PendingChord::SoftStop);
            }
            KeyAction::PickerUp => self.move_picker(false),
            KeyAction::PickerDown => self.move_picker(true),
            KeyAction::PickerSelect => self.confirm_picker_selection().await?,
            KeyAction::PickerBackspace => self.update_picker_query(None),
            KeyAction::SessionPickerInput => self.handle_picker_input(key),
            KeyAction::PromptInput => {
                self.prompt.input(key);
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
            self.runtime.model(),
            self.active_view()
                .map(|view| view.total_tokens)
                .unwrap_or_default(),
        );

        let (entries, scroll) = match self.active_view_mut() {
            Some(view) => {
                if view.auto_scroll {
                    view.scroll =
                        chat::max_scroll(&view.entries, layout[1].width, layout[1].height);
                }
                (&view.entries, view.scroll)
            }
            None => {
                static EMPTY: Vec<ChatEntry> = Vec::new();
                (&EMPTY, 0)
            }
        };
        chat::render_chat(frame, layout[1], entries, scroll);

        self.prompt.render(frame, layout[2], self.mode);

        let footer =
            Paragraph::new(self.footer_text()).block(Block::default().borders(Borders::ALL));
        frame.render_widget(footer, layout[3]);

        if let Some(diff_view) = self.active_view().and_then(|view| view.diff_view.clone()) {
            diff::render_diff_view(frame, size, &diff_view);
        }

        if let Some(picker) = &self.session_picker {
            sessions::render_session_picker(frame, size, picker, &self.sessions);
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
                self.push_status_line(
                    self.active_session_id.clone(),
                    "/help, /model [name], /new, /sessions, /clear, /quit".to_string(),
                );
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
            "/model" => {
                if let Some(model) = parts.next() {
                    let session_id = self.runtime.set_model(model.to_string()).await?;
                    self.prompt.clear();
                    self.refresh_session_list().await?;
                    self.switch_to_session(session_id).await?;
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
            Some(view) if view.loaded && self.active_session_is_live(session_id.clone()) => false,
            Some(view) => !view.loaded || !self.active_session_is_live(session_id.clone()),
            None => true,
        };
        if !should_reload {
            return Ok(());
        }

        let events = self.runtime.session_events(session_id.clone()).await?;
        let meta = self.runtime.session_meta_by_id(session_id.clone()).await?;
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
        if matched {
            self.pending_chord = None;
            match chord {
                PendingChord::OpenSessions => self.open_session_picker(),
                PendingChord::SoftStop => {
                    self.runtime
                        .soft_cancel_session(self.active_session_id.clone())
                        .await?;
                    self.push_status_line(
                        self.active_session_id.clone(),
                        "Stop requested. MOA will stop after the current step.".to_string(),
                    );
                }
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
                PendingChord::SoftStop => "Press S to request a soft stop.".to_string(),
            };
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
            return "Ctrl+Q: queue message  Ctrl+X, S: stop  Ctrl+C/Esc: hard cancel  Alt+[/]: tabs"
                .to_string();
        }
        "Enter: send  Shift+Enter: newline  Ctrl+N: new  Ctrl+O, S: sessions  Alt+[/]: tabs  /help"
            .to_string()
    }

    fn push_status_line(&mut self, session_id: SessionId, text: String) {
        let view = self.session_view_mut(session_id);
        view.entries.push(ChatEntry::Status(text));
        view.auto_scroll = true;
    }

    fn sync_mode_with_state(&mut self) {
        self.mode = if self.session_picker.is_some() {
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
            preview.summary.status = status;
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
                        detail: Some(truncate_detail(output)),
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
                } => {
                    let prompt = if cached_request_id == Some(*request_id) {
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

/// Runs the full-screen TUI until the user exits.
pub async fn run_tui(config: MoaConfig) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(config).await?;

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
    use moa_core::{Platform, SessionSummary, UserId, WorkspaceId};
    use ratatui::{Terminal, backend::TestBackend};
    use tokio::runtime::Runtime;

    use super::*;

    fn test_app() -> App {
        Runtime::new().expect("runtime").block_on(async {
            let runtime = ChatRuntime::for_test(Platform::Tui).await.expect("runtime");
            let active_session_id = runtime.session_id().clone();
            let (runtime_event_tx, runtime_event_rx) = mpsc::unbounded_channel();
            let mut app = App {
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
                session_views: HashMap::from([(
                    active_session_id.clone(),
                    SessionViewState::default(),
                )]),
                active_session_id,
                viewport_width: 100,
                should_exit: false,
                pending_chord: None,
                session_picker: None,
                runtime_event_tx,
                runtime_event_rx,
                observation_task: None,
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
}
