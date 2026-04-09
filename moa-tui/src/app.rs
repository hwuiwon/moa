//! App state machine and render loop for the basic Step 08 TUI.

use std::io;
use std::time::Duration;

use crossterm::{
    event::{self, Event as CrosstermEvent, KeyEvent},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};
use tokio::{sync::mpsc, task::JoinHandle};

use moa_core::{ApprovalDecision, MoaConfig, Result};

use crate::{
    keybindings::{KeyAction, map_key_event},
    runner::{
        ApprovalPrompt, ChatRuntime, RuntimeCommand, RuntimeEvent, ToolCardStatus, ToolUpdate,
    },
    views::chat,
    widgets::prompt::PromptWidget,
};

const FRAME_DURATION: Duration = Duration::from_millis(33);

/// Basic TUI app mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    /// No prompt text and no active generation.
    Idle,
    /// Prompt text is being edited.
    Composing,
    /// A turn is actively running.
    Running,
    /// The turn is waiting for a human approval decision.
    WaitingApproval,
}

/// Renderable transcript entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChatEntry {
    User(String),
    Assistant { text: String, streaming: bool },
    Tool(ToolCardEntry),
    Status(String),
}

/// Renderable inline tool card state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolCardEntry {
    pub(crate) tool_id: uuid::Uuid,
    pub(crate) tool_name: String,
    pub(crate) status: ToolCardStatus,
    pub(crate) summary: String,
    pub(crate) detail: Option<String>,
}

/// Stateful TUI application model.
pub struct App {
    runtime: ChatRuntime,
    mode: AppMode,
    prompt: PromptWidget,
    entries: Vec<ChatEntry>,
    total_tokens: usize,
    scroll: u16,
    auto_scroll: bool,
    should_exit: bool,
    pending_approval: Option<ApprovalPrompt>,
    runtime_event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    runtime_event_rx: mpsc::UnboundedReceiver<RuntimeEvent>,
    control_tx: Option<mpsc::UnboundedSender<RuntimeCommand>>,
    active_task: Option<JoinHandle<()>>,
}

impl App {
    /// Creates a new TUI app from the loaded MOA config.
    pub async fn new(config: MoaConfig) -> Result<Self> {
        let runtime = ChatRuntime::from_config(config, moa_core::Platform::Tui).await?;
        let (runtime_event_tx, runtime_event_rx) = mpsc::unbounded_channel();

        Ok(Self {
            runtime,
            mode: AppMode::Idle,
            prompt: PromptWidget::new(),
            entries: Vec::new(),
            total_tokens: 0,
            scroll: 0,
            auto_scroll: true,
            should_exit: false,
            pending_approval: None,
            runtime_event_tx,
            runtime_event_rx,
            control_tx: None,
            active_task: None,
        })
    }

    /// Returns the current high-level app mode.
    pub fn mode(&self) -> AppMode {
        self.mode
    }

    /// Returns whether the app requested clean shutdown.
    pub fn should_exit(&self) -> bool {
        self.should_exit
    }

    /// Processes any pending runtime events.
    pub fn drain_runtime_events(&mut self) {
        while let Ok(event) = self.runtime_event_rx.try_recv() {
            self.handle_runtime_event(event);
        }
    }

    /// Handles a single key press from the terminal loop.
    pub async fn handle_key_event(&mut self, key: KeyEvent) -> Result<()> {
        match map_key_event(self.mode, key) {
            KeyAction::Submit => self.submit_prompt().await?,
            KeyAction::InsertNewline => {
                self.prompt.insert_newline();
                self.sync_mode_with_prompt();
            }
            KeyAction::Cancel => self.cancel_or_exit(),
            KeyAction::ApproveOnce => {
                self.send_approval(ApprovalDecision::AllowOnce);
            }
            KeyAction::AlwaysAllow => {
                if let Some(prompt) = &self.pending_approval {
                    self.send_approval(ApprovalDecision::AlwaysAllow {
                        pattern: prompt.pattern.clone(),
                    });
                }
            }
            KeyAction::Deny => {
                self.send_approval(ApprovalDecision::Deny { reason: None });
            }
            KeyAction::ScrollUp => {
                self.auto_scroll = false;
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyAction::ScrollDown => {
                self.auto_scroll = false;
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyAction::ScrollEnd => {
                self.auto_scroll = true;
            }
            KeyAction::PromptInput => {
                self.prompt.input(key);
                self.sync_mode_with_prompt();
            }
            KeyAction::Noop => {}
        }

        Ok(())
    }

    /// Renders the full app into a frame.
    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        let size = frame.area();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(8),
                Constraint::Length(4),
                Constraint::Length(1),
            ])
            .split(size);

        let header = Paragraph::new(Line::from(format!(
            "MOA  model: {}  tokens: {}  mode: {:?}",
            self.runtime.model(),
            self.total_tokens,
            self.mode
        )))
        .block(Block::default().borders(Borders::ALL));
        frame.render_widget(header, layout[0]);

        if self.auto_scroll {
            self.scroll = chat::max_scroll(&self.entries, layout[1].width, layout[1].height);
        }
        chat::render_chat(frame, layout[1], &self.entries, self.scroll);

        self.prompt.render(frame, layout[2], self.mode);

        let footer = Paragraph::new(Line::from(
            "Enter: send  Shift+Enter: newline  Ctrl+C/Esc: cancel  /help  y/n/a: approval",
        ))
        .block(Block::default().borders(Borders::ALL));
        frame.render_widget(footer, layout[3]);
    }

    async fn submit_prompt(&mut self) -> Result<()> {
        let prompt = self.prompt.text();
        let trimmed = prompt.trim();
        if trimmed.is_empty() {
            return Ok(());
        }

        if trimmed.starts_with('/') {
            self.prompt.clear();
            self.sync_mode_with_prompt();
            self.handle_slash_command(trimmed).await?;
            return Ok(());
        }

        self.entries.push(ChatEntry::User(trimmed.to_string()));
        self.prompt.clear();
        self.auto_scroll = true;
        self.mode = AppMode::Running;

        let runtime = self.runtime.clone();
        let prompt = trimmed.to_string();
        let event_tx = self.runtime_event_tx.clone();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        self.control_tx = Some(control_tx);
        self.active_task = Some(tokio::spawn(async move {
            if let Err(error) = runtime.run_turn(prompt, event_tx.clone(), control_rx).await {
                let _ = event_tx.send(RuntimeEvent::Error(error.to_string()));
                let _ = event_tx.send(RuntimeEvent::TurnCompleted);
            }
        }));
        Ok(())
    }

    async fn handle_slash_command(&mut self, command: &str) -> Result<()> {
        let mut parts = command.split_whitespace();
        match parts.next().unwrap_or_default() {
            "/help" => {
                self.entries.push(ChatEntry::Status(
                    "/help, /model [name], /clear, /quit".to_string(),
                ));
            }
            "/quit" => {
                self.should_exit = true;
            }
            "/clear" => {
                self.cancel_active_turn();
                self.entries.clear();
                self.total_tokens = 0;
                let _session_id = self.runtime.reset_session().await?;
            }
            "/model" => {
                if let Some(model) = parts.next() {
                    if self.mode == AppMode::Running || self.mode == AppMode::WaitingApproval {
                        self.entries.push(ChatEntry::Status(
                            "Cannot switch models while a turn is active.".to_string(),
                        ));
                    } else {
                        self.runtime.set_model(model.to_string()).await?;
                        self.entries.clear();
                        self.total_tokens = 0;
                        self.entries.push(ChatEntry::Status(format!(
                            "Switched model to {model} and started a fresh session."
                        )));
                    }
                } else {
                    self.entries.push(ChatEntry::Status(format!(
                        "Current model: {}",
                        self.runtime.model()
                    )));
                }
            }
            other => {
                self.entries.push(ChatEntry::Status(format!(
                    "Unknown command: {other}. Try /help."
                )));
            }
        }

        self.auto_scroll = true;
        self.sync_mode_with_prompt();
        Ok(())
    }

    fn handle_runtime_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::AssistantStarted => {
                self.entries.push(ChatEntry::Assistant {
                    text: String::new(),
                    streaming: true,
                });
                self.mode = AppMode::Running;
            }
            RuntimeEvent::AssistantDelta(ch) => {
                if let Some(ChatEntry::Assistant { text, .. }) = self.entries.last_mut() {
                    text.push(ch);
                } else {
                    self.entries.push(ChatEntry::Assistant {
                        text: ch.to_string(),
                        streaming: true,
                    });
                }
                self.auto_scroll = true;
            }
            RuntimeEvent::AssistantFinished { text } => {
                if let Some(ChatEntry::Assistant {
                    text: current,
                    streaming,
                }) = self.entries.last_mut()
                {
                    *current = text;
                    *streaming = false;
                }
                self.auto_scroll = true;
            }
            RuntimeEvent::ToolUpdate(update) => {
                self.upsert_tool_card(update);
                self.auto_scroll = true;
            }
            RuntimeEvent::ApprovalRequested(prompt) => {
                self.pending_approval = Some(prompt);
                self.mode = AppMode::WaitingApproval;
                self.auto_scroll = true;
            }
            RuntimeEvent::UsageUpdated { total_tokens } => {
                self.total_tokens = total_tokens;
            }
            RuntimeEvent::Notice(text) | RuntimeEvent::Error(text) => {
                self.entries.push(ChatEntry::Status(text));
                self.auto_scroll = true;
            }
            RuntimeEvent::TurnCompleted => {
                self.pending_approval = None;
                self.control_tx = None;
                self.active_task = None;
                self.sync_mode_with_prompt();
            }
        }
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

    fn send_approval(&mut self, decision: ApprovalDecision) {
        if let Some(control_tx) = &self.control_tx {
            let _ = control_tx.send(RuntimeCommand::Approval(decision));
            self.pending_approval = None;
            self.mode = AppMode::Running;
        }
    }

    fn cancel_or_exit(&mut self) {
        if self.mode == AppMode::Running || self.mode == AppMode::WaitingApproval {
            self.cancel_active_turn();
            self.entries.push(ChatEntry::Status(
                "Cancelled current generation.".to_string(),
            ));
        } else {
            self.should_exit = true;
        }
    }

    fn cancel_active_turn(&mut self) {
        if let Some(task) = self.active_task.take() {
            task.abort();
        }
        self.control_tx = None;
        self.pending_approval = None;
        self.sync_mode_with_prompt();
    }

    fn sync_mode_with_prompt(&mut self) {
        if self.active_task.is_some() {
            if self.pending_approval.is_some() {
                self.mode = AppMode::WaitingApproval;
            } else {
                self.mode = AppMode::Running;
            }
            return;
        }

        self.mode = if self.prompt.text().trim().is_empty() {
            AppMode::Idle
        } else {
            AppMode::Composing
        };
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
    use crossterm::event::KeyCode;
    use ratatui::{Terminal, backend::TestBackend};
    use tokio::runtime::Runtime;

    use super::*;

    fn test_app() -> App {
        Runtime::new().unwrap().block_on(async {
            let (runtime_event_tx, runtime_event_rx) = mpsc::unbounded_channel();
            App {
                runtime: ChatRuntime::for_test(moa_core::Platform::Tui)
                    .await
                    .unwrap(),
                mode: AppMode::Idle,
                prompt: PromptWidget::new(),
                entries: Vec::new(),
                total_tokens: 0,
                scroll: 0,
                auto_scroll: true,
                should_exit: false,
                pending_approval: None,
                runtime_event_tx,
                runtime_event_rx,
                control_tx: None,
                active_task: None,
            }
        })
    }

    #[test]
    fn app_state_transitions_follow_idle_composing_running_idle() {
        let mut app = test_app();
        assert_eq!(app.mode(), AppMode::Idle);

        app.prompt.input(KeyEvent::from(KeyCode::Char('h')));
        app.sync_mode_with_prompt();
        assert_eq!(app.mode(), AppMode::Composing);

        let runtime = Runtime::new().unwrap();
        runtime.block_on(async {
            app.active_task = Some(tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }));
            app.sync_mode_with_prompt();
            assert_eq!(app.mode(), AppMode::Running);
            if let Some(task) = app.active_task.take() {
                task.abort();
            }
        });

        app.prompt.clear();
        app.sync_mode_with_prompt();
        assert_eq!(app.mode(), AppMode::Idle);
    }

    #[test]
    fn rendering_smoke_test_does_not_panic() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = test_app();
        app.entries.push(ChatEntry::User("Hello".to_string()));
        app.entries.push(ChatEntry::Assistant {
            text: "Hi there".to_string(),
            streaming: false,
        });

        terminal.draw(|frame| app.draw(frame)).unwrap();
    }
}
