//! Manual helper binary for Temporal worker restart integration tests.

#[cfg(not(feature = "temporal"))]
fn main() {
    eprintln!("temporal_worker_helper requires the `temporal` feature");
}

#[cfg(feature = "temporal")]
mod temporal_helper {
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use moa_core::{
        BrainOrchestrator, CompletionContent, CompletionRequest, CompletionResponse,
        CompletionStream, LLMProvider, MessageRole, MoaConfig, Platform, Result,
        StartSessionRequest, StopReason, TokenPricing, TokenUsage, ToolCallFormat, UserId,
        UserMessage, WorkspaceId,
    };
    use moa_hands::ToolRouter;
    use moa_memory::FileMemoryStore;
    use moa_orchestrator::TemporalOrchestrator;
    use moa_session::{create_session_store, testing};
    use tokio::time::sleep;

    #[derive(Clone)]
    struct HelperProvider {
        model: String,
        delay: Duration,
    }

    #[async_trait]
    impl LLMProvider for HelperProvider {
        fn name(&self) -> &str {
            "temporal-helper"
        }

        fn capabilities(&self) -> moa_core::ModelCapabilities {
            moa_core::ModelCapabilities {
                model_id: self.model.clone().into(),
                context_window: 200_000,
                max_output: 8_192,
                supports_tools: false,
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
            let prompt = request
                .messages
                .iter()
                .rev()
                .find(|message| message.role == MessageRole::User)
                .map(|message| message.content.as_str())
                .unwrap_or_default()
                .to_string();
            let response = CompletionResponse {
                text: format!("assistant:{prompt}"),
                content: vec![CompletionContent::Text(format!("assistant:{prompt}"))],
                stop_reason: StopReason::EndTurn,
                model: self.model.clone().into(),
                input_tokens: 4,
                output_tokens: 2,
                cached_input_tokens: 0,
                usage: TokenUsage {
                    input_tokens_uncached: 4,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 2,
                },
                duration_ms: self.delay.as_millis() as u64,
                thought_signature: None,
            };
            let (tx, rx) = tokio::sync::mpsc::channel(4);
            let delay = self.delay;
            let completion = tokio::spawn(async move {
                sleep(delay).await;
                let _ = tx
                    .send(Ok(CompletionContent::Text(response.text.clone())))
                    .await;
                Ok(response)
            });
            Ok(CompletionStream::new(rx, completion))
        }
    }

    fn helper_config(root: &std::path::Path, port: u16, task_queue: &str) -> MoaConfig {
        let mut config = MoaConfig::default();
        config.database.url = testing::test_database_url();
        config.local.memory_dir = root.join("memory").display().to_string();
        config.local.sandbox_dir = root.join("sandbox").display().to_string();
        config.cloud.enabled = true;
        if let Some(hands) = config.cloud.hands.as_mut() {
            hands.default_provider = Some("local".to_string());
        }
        config
            .cloud
            .temporal
            .as_mut()
            .expect("temporal config")
            .address = Some(format!("127.0.0.1:{port}"));
        config
            .cloud
            .temporal
            .as_mut()
            .expect("temporal config")
            .namespace = Some("default".to_string());
        config
            .cloud
            .temporal
            .as_mut()
            .expect("temporal config")
            .task_queue = task_queue.to_string();
        config
            .cloud
            .temporal
            .as_mut()
            .expect("temporal config")
            .api_key_env = None;
        config
    }

    #[tokio::main(flavor = "multi_thread", worker_threads = 2)]
    pub(crate) async fn main_impl() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut args = std::env::args().skip(1);
        let mode = args.next().ok_or("missing mode")?;
        let root = std::path::PathBuf::from(args.next().ok_or("missing root")?);
        let port = args
            .next()
            .ok_or("missing port")?
            .parse::<u16>()
            .map_err(|error| format!("invalid port: {error}"))?;
        let task_queue = args.next().ok_or("missing task queue")?;
        let delay_ms = args
            .next()
            .ok_or("missing delay")?
            .parse::<u64>()
            .map_err(|error| format!("invalid delay: {error}"))?;

        eprintln!(
            "temporal helper: mode={mode} root={} port={port}",
            root.display()
        );
        tokio::fs::create_dir_all(&root).await?;
        let config = helper_config(&root, port, &task_queue);
        let provider: Arc<dyn LLMProvider> = Arc::new(HelperProvider {
            model: config.general.default_model.clone(),
            delay: Duration::from_millis(delay_ms),
        });
        eprintln!("temporal helper: opening stores");
        let session_store = create_session_store(&config).await?;
        let memory_store = Arc::new(
            FileMemoryStore::from_config_with_pool(
                &config,
                Arc::new(session_store.pool().clone()),
                session_store.schema_name(),
            )
            .await?,
        );
        let tool_router = Arc::new(
            ToolRouter::from_config(&config, memory_store.clone())
                .await?
                .with_rule_store(session_store.clone())
                .with_session_store(session_store.clone()),
        );
        eprintln!("temporal helper: creating orchestrator");
        let orchestrator = TemporalOrchestrator::new(
            config.clone(),
            session_store,
            memory_store,
            provider,
            tool_router,
        )
        .await?;
        eprintln!("temporal helper: orchestrator ready");

        if mode == "start" {
            eprintln!("temporal helper: starting session");
            let session = orchestrator
                .start_session(StartSessionRequest {
                    workspace_id: WorkspaceId::new("ws-helper"),
                    user_id: UserId::new("u-helper"),
                    platform: Platform::Cli,
                    model: config.general.default_model.clone().into(),
                    initial_message: Some(UserMessage {
                        text: "recover me".to_string(),
                        attachments: Vec::new(),
                    }),
                    title: None,
                    parent_session_id: None,
                })
                .await?;
            tokio::fs::write(root.join("session_id.txt"), session.session_id.to_string()).await?;
            eprintln!("temporal helper: wrote session id {}", session.session_id);
        }

        loop {
            sleep(Duration::from_secs(60)).await;
        }
    }
}

#[cfg(feature = "temporal")]
fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    temporal_helper::main_impl()
}
