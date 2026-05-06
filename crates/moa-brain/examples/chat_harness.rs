//! Minimal no-UI chat harness for verifying Step 04 end to end.

use std::io::{self, Write};
use std::sync::Arc;

use moa_brain::{
    GraphMemoryPipelineOptions,
    build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions,
    run_brain_turn_with_tools,
};
use moa_core::{
    Event, EventRange, LLMProvider, MoaConfig, Result, SessionMeta, SessionStore, UserId,
    WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_providers::build_provider_from_config;
use moa_session::{PostgresSessionStore, testing};

/// Runs the Step 04 chat harness.
#[tokio::main]
async fn main() -> Result<()> {
    let config = MoaConfig::load()?;
    let (store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    let store = Arc::new(store);
    let provider = build_provider_from_config(&config)?;
    let tool_router = Arc::new(
        ToolRouter::from_config(&config)
            .await?
            .with_session_store(store.clone()),
    );
    let session_id = store
        .create_session(SessionMeta {
            workspace_id: WorkspaceId::new("step-04-harness"),
            user_id: UserId::new("local-user"),
            model: config.general.default_model.clone().into(),
            ..SessionMeta::default()
        })
        .await?;
    let pipeline = build_default_graph_memory_pipeline_with_rewriter_runtime_and_instructions(
        &config,
        store.clone(),
        GraphMemoryPipelineOptions {
            graph_pool: store.pool().clone(),
            compaction_llm_provider: Some(provider.clone()),
            query_rewrite_llm_provider: Some(provider.clone()),
            discovered_workspace_instructions: None,
            tool_schemas: tool_router.tool_schemas(),
        },
    );
    let cli_prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");

    println!("MOA Step 04 chat harness");
    println!("model: {}", config.general.default_model);
    println!("session: {}", session_id);
    println!("database: {} schema={}", database_url, schema_name);

    if cli_prompt.trim().is_empty() {
        println!("Type a prompt and press enter. Use /quit to exit.");
    } else {
        run_prompt(
            session_id,
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
            session_id,
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

async fn run_prompt(
    session_id: moa_core::SessionId,
    store: Arc<PostgresSessionStore>,
    provider: Arc<dyn LLMProvider>,
    pipeline: &moa_brain::ContextPipeline,
    tool_router: Arc<ToolRouter>,
    prompt: &str,
) -> Result<()> {
    let event_count_before = store.get_events(session_id, EventRange::all()).await?.len();

    store
        .emit_event(
            session_id,
            Event::UserMessage {
                text: prompt.to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;

    run_brain_turn_with_tools(
        session_id,
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
