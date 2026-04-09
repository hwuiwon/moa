//! Minimal no-UI chat harness for verifying Step 04 end to end.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use moa_brain::{build_default_pipeline_with_tools, run_brain_turn_with_tools};
use moa_core::{
    Event, EventRange, MoaConfig, Result, SessionMeta, SessionStore, UserId, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use moa_providers::AnthropicProvider;
use moa_session::TursoSessionStore;
use tempfile::TempDir;

/// Runs the Step 04 chat harness.
#[tokio::main]
async fn main() -> Result<()> {
    let config = MoaConfig::load()?;
    let (session_db_path, _session_db_guard) = resolve_session_db_path();
    let memory_store = Arc::new(FileMemoryStore::from_config(&config).await?);
    let store = Arc::new(TursoSessionStore::new_local(Path::new(&session_db_path)).await?);
    let provider = Arc::new(AnthropicProvider::from_config(&config)?);
    let tool_router = Arc::new(ToolRouter::from_config(&config, memory_store.clone()).await?);
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: WorkspaceId::new("step-04-harness"),
            user_id: UserId::new("local-user"),
            model: config.general.default_model.clone(),
            ..SessionMeta::default()
        })
        .await?;
    let pipeline = build_default_pipeline_with_tools(
        &config,
        store.clone(),
        memory_store,
        tool_router.tool_schemas(),
    );
    let cli_prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");

    println!("MOA Step 04 chat harness");
    println!("model: {}", config.general.default_model);
    println!("session: {}", session_id);
    println!("database: {}", session_db_path.display());

    if cli_prompt.trim().is_empty() {
        println!("Type a prompt and press enter. Use /quit to exit.");
    } else {
        run_prompt(
            session_id.clone(),
            store.clone(),
            provider.clone(),
            &pipeline,
            tool_router.clone(),
            &cli_prompt,
        )
        .await?;
        return Ok(());
    }

    loop {
        print!("you> ");
        io::stdout().flush()?;

        let mut line = String::new();
        let bytes_read = io::stdin().read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }

        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        if prompt == "/quit" {
            break;
        }

        run_prompt(
            session_id.clone(),
            store.clone(),
            provider.clone(),
            &pipeline,
            tool_router.clone(),
            prompt,
        )
        .await?;
    }

    Ok(())
}

fn resolve_session_db_path() -> (PathBuf, Option<TempDir>) {
    if let Ok(path) = std::env::var("MOA_HARNESS_DB_PATH") {
        return (PathBuf::from(path), None);
    }

    let temp_dir = tempfile::tempdir().ok();
    let path = temp_dir
        .as_ref()
        .map(|dir| dir.path().join("sessions.db"))
        .unwrap_or_else(|| std::env::temp_dir().join("moa-step-04-sessions.db"));

    (path, temp_dir)
}

async fn run_prompt(
    session_id: moa_core::SessionId,
    store: Arc<TursoSessionStore>,
    provider: Arc<AnthropicProvider>,
    pipeline: &moa_brain::ContextPipeline,
    tool_router: Arc<ToolRouter>,
    prompt: &str,
) -> Result<()> {
    let event_count_before = store
        .get_events(session_id.clone(), EventRange::all())
        .await?
        .len();

    store
        .emit_event(
            session_id.clone(),
            Event::UserMessage {
                text: prompt.to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;

    run_brain_turn_with_tools(
        session_id.clone(),
        store.clone(),
        provider,
        pipeline,
        Some(tool_router),
    )
    .await?;

    let response_texts = store
        .get_events(session_id, EventRange::all())
        .await?
        .into_iter()
        .skip(event_count_before)
        .filter_map(|record| match record.event {
            Event::BrainResponse { text, .. } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>();

    if response_texts.is_empty() {
        println!("assistant> [no BrainResponse event emitted]");
        return Ok(());
    }

    for response in response_texts {
        println!("assistant> {response}");
    }

    Ok(())
}
