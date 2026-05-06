//! Non-interactive `moa exec` implementation.

use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use moa_core::{
    ApprovalDecision, ApprovalPrompt, MoaConfig, Platform, RuntimeEvent, SessionStatus, ToolUpdate,
};
use moa_runtime::ChatRuntime;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::time::{Duration, sleep};

/// Runs a one-shot prompt through the shared local chat runtime.
pub async fn run_exec(config: MoaConfig, prompt: String) -> Result<()> {
    let runtime = ChatRuntime::from_config(config, Platform::Cli).await?;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn({
        let runtime = runtime.clone();
        let event_tx = event_tx.clone();
        async move {
            if let Err(error) = runtime.run_turn(prompt, event_tx.clone()).await {
                let _ = event_tx.send(RuntimeEvent::Error(error.to_string()));
                let _ = event_tx.send(RuntimeEvent::TurnCompleted);
            }
        }
    });

    let mut final_output = String::new();
    let mut interrupt_state = InterruptState::Idle;
    let mut terminated_after_interrupt = false;
    loop {
        if interrupt_state.awaiting_shutdown() {
            tokio::select! {
                event = event_rx.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    if handle_exec_event(event, &runtime, &mut final_output, &mut interrupt_state).await? {
                        break;
                    }
                }
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for ctrl-c")?;
                    handle_exec_interrupt(&runtime, &mut interrupt_state).await?;
                }
                _ = sleep(Duration::from_millis(250)) => {
                    if session_has_terminated(&runtime).await? {
                        terminated_after_interrupt = true;
                        break;
                    }
                }
            }
        } else {
            tokio::select! {
                event = event_rx.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    if handle_exec_event(event, &runtime, &mut final_output, &mut interrupt_state).await? {
                        break;
                    }
                }
                signal = tokio::signal::ctrl_c() => {
                    signal.context("failed to listen for ctrl-c")?;
                    handle_exec_interrupt(&runtime, &mut interrupt_state).await?;
                }
            }
        }
    }

    if terminated_after_interrupt && !task.is_finished() {
        task.abort();
    }
    match task.await {
        Ok(()) => {}
        Err(error) if terminated_after_interrupt && error.is_cancelled() => {}
        Err(error) => return Err(error).context("exec task join failure"),
    }
    if let Err(error) = runtime.shutdown_background_workers().await {
        eprintln!("warning: failed to drain background workers: {error}");
    }

    if io::stdout().is_terminal() {
        println!("{final_output}");
    } else {
        print!("{final_output}");
        io::stdout().flush()?;
    }

    Ok(())
}

async fn handle_exec_event(
    event: RuntimeEvent,
    runtime: &ChatRuntime,
    final_output: &mut String,
    interrupt_state: &mut InterruptState,
) -> Result<bool> {
    match event {
        RuntimeEvent::AssistantStarted => {}
        RuntimeEvent::AssistantDelta(ch) => final_output.push(ch),
        RuntimeEvent::AssistantFinished { .. } => {}
        RuntimeEvent::ToolUpdate(update) => {
            eprintln!("{}", format_tool_update(&update));
        }
        RuntimeEvent::ApprovalRequested(prompt) => {
            resolve_exec_approval(runtime, prompt, interrupt_state).await?;
        }
        RuntimeEvent::UsageUpdated { total_tokens } => {
            eprintln!("tokens: {total_tokens}");
        }
        RuntimeEvent::Notice(text) => eprintln!("{text}"),
        RuntimeEvent::Error(text) => eprintln!("error: {text}"),
        RuntimeEvent::TurnCompleted => return Ok(true),
    }

    Ok(false)
}

async fn resolve_exec_approval(
    runtime: &ChatRuntime,
    prompt: ApprovalPrompt,
    interrupt_state: &mut InterruptState,
) -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!(
            "approval required for {} but stdin is not interactive",
            prompt.request.tool_name
        );
    }

    loop {
        eprint!(
            "approval required for {} ({}) [y=allow once / a=always / n=deny]: ",
            prompt.request.tool_name, prompt.request.input_summary
        );
        io::stderr().flush()?;

        let mut input = String::new();
        let mut stdin = BufReader::new(tokio::io::stdin());
        tokio::select! {
            read = stdin.read_line(&mut input) => {
                let bytes_read = read.context("failed to read approval input")?;
                if bytes_read == 0 {
                    bail!("approval input closed unexpectedly");
                }
            }
            signal = tokio::signal::ctrl_c() => {
                signal.context("failed to listen for ctrl-c")?;
                handle_exec_interrupt(runtime, interrupt_state).await?;
                return Ok(());
            }
        }
        let Some(decision) = parse_approval_decision(input.trim(), &prompt.pattern) else {
            eprintln!("expected y, a, or n");
            continue;
        };
        runtime
            .respond_to_approval(prompt.request.request_id, decision)
            .await
            .context("failed to send approval decision")?;
        return Ok(());
    }
}

fn format_tool_update(update: &ToolUpdate) -> String {
    match &update.detail {
        Some(detail) => format!(
            "tool [{}] {}: {} ({detail})",
            format!("{:?}", update.status).to_lowercase(),
            update.tool_name,
            update.summary
        ),
        None => format!(
            "tool [{}] {}: {}",
            format!("{:?}", update.status).to_lowercase(),
            update.tool_name,
            update.summary
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptState {
    Idle,
    SoftCancelRequested,
    HardCancelRequested,
}

impl InterruptState {
    fn awaiting_shutdown(self) -> bool {
        !matches!(self, Self::Idle)
    }
}

async fn handle_exec_interrupt(
    runtime: &ChatRuntime,
    interrupt_state: &mut InterruptState,
) -> Result<()> {
    match interrupt_state {
        InterruptState::Idle => {
            eprintln!("interrupt received, requesting stop...");
            runtime
                .soft_cancel_session(*runtime.session_id())
                .await
                .context("failed to request soft cancel")?;
            *interrupt_state = InterruptState::SoftCancelRequested;
        }
        InterruptState::SoftCancelRequested => {
            eprintln!("interrupt received again, cancelling immediately...");
            runtime
                .cancel_active_generation()
                .await
                .context("failed to request hard cancel")?;
            *interrupt_state = InterruptState::HardCancelRequested;
        }
        InterruptState::HardCancelRequested => {
            eprintln!("interrupt already requested; waiting for shutdown...");
        }
    }

    Ok(())
}

fn parse_approval_decision(input: &str, pattern: &str) -> Option<ApprovalDecision> {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(ApprovalDecision::AllowOnce),
        "a" | "always" => Some(ApprovalDecision::AlwaysAllow {
            pattern: pattern.to_string(),
        }),
        "n" | "no" => Some(ApprovalDecision::Deny { reason: None }),
        _ => None,
    }
}

async fn session_has_terminated(runtime: &ChatRuntime) -> Result<bool> {
    Ok(is_terminal_session_status(
        &runtime.session_meta().await?.status,
    ))
}

fn is_terminal_session_status(status: &SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Paused
            | SessionStatus::Completed
            | SessionStatus::Cancelled
            | SessionStatus::Failed
    )
}

#[cfg(test)]
mod tests {
    use moa_core::{ApprovalDecision, SessionStatus, ToolCardStatus, ToolUpdate};
    use uuid::Uuid;

    use super::{
        InterruptState, format_tool_update, is_terminal_session_status, parse_approval_decision,
    };

    #[test]
    fn exec_mode_formats_tool_updates_compactly() {
        let rendered = format_tool_update(&ToolUpdate {
            tool_id: Uuid::now_v7(),
            tool_name: "bash".to_string(),
            status: ToolCardStatus::Succeeded,
            summary: "bash completed in 12 ms".to_string(),
            detail: Some("hello".to_string()),
        });

        assert!(rendered.contains("tool [succeeded] bash"));
        assert!(rendered.contains("hello"));
    }

    #[test]
    fn approval_input_parser_supports_allow_once() {
        assert_eq!(
            parse_approval_decision("y", "bash *"),
            Some(ApprovalDecision::AllowOnce)
        );
    }

    #[test]
    fn approval_input_parser_supports_always_allow() {
        assert_eq!(
            parse_approval_decision("always", "bash *"),
            Some(ApprovalDecision::AlwaysAllow {
                pattern: "bash *".to_string()
            })
        );
    }

    #[test]
    fn approval_input_parser_rejects_unknown_values() {
        assert_eq!(parse_approval_decision("maybe", "bash *"), None);
    }

    #[test]
    fn interrupt_state_only_polls_for_shutdown_after_interrupt() {
        assert!(!InterruptState::Idle.awaiting_shutdown());
        assert!(InterruptState::SoftCancelRequested.awaiting_shutdown());
        assert!(InterruptState::HardCancelRequested.awaiting_shutdown());
    }

    #[test]
    fn terminal_session_status_helper_matches_expected_states() {
        assert!(!is_terminal_session_status(&SessionStatus::Created));
        assert!(!is_terminal_session_status(&SessionStatus::Running));
        assert!(!is_terminal_session_status(&SessionStatus::WaitingApproval));
        assert!(is_terminal_session_status(&SessionStatus::Paused));
        assert!(is_terminal_session_status(&SessionStatus::Completed));
        assert!(is_terminal_session_status(&SessionStatus::Cancelled));
        assert!(is_terminal_session_status(&SessionStatus::Failed));
    }
}
