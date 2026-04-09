//! Non-interactive `moa exec` implementation.

use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use moa_core::{ApprovalDecision, ApprovalPrompt, MoaConfig, Platform, RuntimeEvent, ToolUpdate};
use moa_tui::runner::ChatRuntime;
use tokio::sync::mpsc;

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
    while let Some(event) = event_rx.recv().await {
        match event {
            RuntimeEvent::AssistantStarted => {}
            RuntimeEvent::AssistantDelta(ch) => final_output.push(ch),
            RuntimeEvent::AssistantFinished { .. } => {}
            RuntimeEvent::ToolUpdate(update) => {
                eprintln!("{}", format_tool_update(&update));
            }
            RuntimeEvent::ApprovalRequested(prompt) => {
                resolve_exec_approval(&runtime, prompt).await?;
            }
            RuntimeEvent::UsageUpdated { total_tokens } => {
                eprintln!("tokens: {total_tokens}");
            }
            RuntimeEvent::Notice(text) => eprintln!("{text}"),
            RuntimeEvent::Error(text) => eprintln!("error: {text}"),
            RuntimeEvent::TurnCompleted => break,
        }
    }

    task.await.context("exec task join failure")?;

    if io::stdout().is_terminal() {
        println!("{final_output}");
    } else {
        print!("{final_output}");
        io::stdout().flush()?;
    }

    Ok(())
}

async fn resolve_exec_approval(runtime: &ChatRuntime, prompt: ApprovalPrompt) -> Result<()> {
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
        io::stdin().read_line(&mut input)?;
        let decision = match input.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => ApprovalDecision::AllowOnce,
            "a" | "always" => ApprovalDecision::AlwaysAllow {
                pattern: prompt.pattern.clone(),
            },
            "n" | "no" => ApprovalDecision::Deny { reason: None },
            _ => {
                eprintln!("expected y, a, or n");
                continue;
            }
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

#[cfg(test)]
mod tests {
    use moa_core::{ToolCardStatus, ToolUpdate};
    use uuid::Uuid;

    use super::format_tool_update;

    #[test]
    fn exec_mode_formats_tool_updates_compactly() {
        let rendered = format_tool_update(&ToolUpdate {
            tool_id: Uuid::new_v4(),
            tool_name: "bash".to_string(),
            status: ToolCardStatus::Succeeded,
            summary: "bash completed in 12 ms".to_string(),
            detail: Some("hello".to_string()),
        });

        assert!(rendered.contains("tool [succeeded] bash"));
        assert!(rendered.contains("hello"));
    }
}
