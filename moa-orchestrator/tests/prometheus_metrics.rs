//! Prometheus metrics integration coverage for the local orchestrator.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use moa_core::{
    BrainOrchestrator, CompletionContent, CompletionRequest, CompletionResponse, CompletionStream,
    EventRange, LLMProvider, MoaConfig, ModelId, Platform, Result, SessionHandle, SessionId,
    SessionSignal, SessionStatus, SessionStore, StartSessionRequest, TelemetryConfig, TokenPricing,
    TokenUsage, UserId, UserMessage, WorkspaceId, init_observability,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use moa_providers::ModelRouter;
use moa_session::{PostgresSessionStore, testing};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::time::{Instant, sleep};

use moa_orchestrator::LocalOrchestrator;

#[derive(Clone)]
struct StreamingMockProvider {
    model: String,
}

#[async_trait]
impl LLMProvider for StreamingMockProvider {
    fn name(&self) -> &str {
        "mock"
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
            tool_call_format: moa_core::ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cached_input_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> {
        let prompt = last_user_message(&request.messages).unwrap_or_default();
        let text = format!("assistant:{prompt}");
        let model = self.model.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let completion = tokio::spawn(async move {
            sleep(Duration::from_millis(20)).await;
            let _ = tx.send(Ok(CompletionContent::Text(text.clone()))).await;
            Ok(CompletionResponse {
                text: text.clone(),
                content: vec![CompletionContent::Text(text.clone())],
                stop_reason: moa_core::StopReason::EndTurn,
                model: model.into(),
                input_tokens: 8,
                output_tokens: 4,
                cached_input_tokens: 0,
                usage: TokenUsage {
                    input_tokens_uncached: 8,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 4,
                },
                duration_ms: 20,
                thought_signature: None,
            })
        });
        Ok(CompletionStream::new(rx, completion))
    }
}

fn last_user_message(messages: &[moa_core::ContextMessage]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == moa_core::MessageRole::User)
        .map(|message| message.content.as_str())
}

async fn create_test_orchestrator(
    metrics_listen: String,
) -> Result<(TempDir, Arc<PostgresSessionStore>, LocalOrchestrator)> {
    let dir = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.memory.auto_bootstrap = false;
    config.local.docker_enabled = false;
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = dir.path().join("sandbox").display().to_string();
    config.metrics.enabled = true;
    config.metrics.listen = metrics_listen;

    let _telemetry = init_observability(&config, &TelemetryConfig::default())?;

    let (session_store, _database_url, schema_name) = testing::create_isolated_test_store().await?;
    let session_store = Arc::new(session_store);
    let memory_store = Arc::new(
        FileMemoryStore::from_config_with_pool(
            &config,
            Arc::new(session_store.pool().clone()),
            Some(&schema_name),
        )
        .await?,
    );
    let tool_router = Arc::new(
        ToolRouter::from_config(&config, memory_store.clone())
            .await?
            .with_rule_store(session_store.clone())
            .with_session_store(session_store.clone()),
    );
    let provider: Arc<dyn LLMProvider> = Arc::new(StreamingMockProvider {
        model: config.general.default_model.clone(),
    });
    let orchestrator = LocalOrchestrator::new(
        config,
        session_store.clone(),
        memory_store,
        Arc::new(ModelRouter::new(provider, None)),
        tool_router,
    )
    .await?;

    Ok((dir, session_store, orchestrator))
}

async fn start_session(orchestrator: &LocalOrchestrator) -> Result<SessionHandle> {
    orchestrator
        .start_session(StartSessionRequest {
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Desktop,
            model: ModelId::new(orchestrator.model()),
            initial_message: None,
            title: None,
            parent_session_id: None,
        })
        .await
}

async fn wait_for_status(
    orchestrator: &LocalOrchestrator,
    session_id: SessionId,
    expected: SessionStatus,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let session = orchestrator.get_session(session_id).await?;
        if session.status == expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(moa_core::MoaError::ProviderError(format!(
                "timed out waiting for status {:?}",
                expected
            )));
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn scrape_metrics(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(|error| moa_core::MoaError::ProviderError(error.to_string()))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match client.get(url).send().await {
            Ok(response) if response.status().is_success() => {
                return response
                    .text()
                    .await
                    .map_err(|error| moa_core::MoaError::ProviderError(error.to_string()));
            }
            Ok(_) | Err(_) if Instant::now() < deadline => {
                sleep(Duration::from_millis(50)).await;
            }
            Ok(response) => {
                return Err(moa_core::MoaError::ProviderError(format!(
                    "metrics scrape failed with status {}",
                    response.status()
                )));
            }
            Err(error) => {
                return Err(moa_core::MoaError::ProviderError(error.to_string()));
            }
        }
    }
}

fn metric_sum(scrape: &str, metric_name: &str) -> f64 {
    scrape
        .lines()
        .filter(|line| {
            !line.starts_with('#')
                && (line.starts_with(&format!("{metric_name}{{")) || line.starts_with(metric_name))
        })
        .filter_map(|line| line.split_whitespace().last())
        .filter_map(|value| value.parse::<f64>().ok())
        .sum()
}

async fn free_local_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind ephemeral test port")
        .local_addr()
        .expect("local addr")
        .port()
}

#[tokio::test]
async fn prometheus_endpoint_exports_turn_metrics() -> Result<()> {
    let port = free_local_port().await;
    let metrics_url = format!("http://127.0.0.1:{port}/metrics");
    let (_dir, store, orchestrator) = create_test_orchestrator(format!("127.0.0.1:{port}")).await?;
    let mut total_brain_responses = 0usize;

    for prompt in ["first", "second", "third"] {
        let session = start_session(&orchestrator).await?;
        orchestrator
            .signal(
                session.session_id,
                SessionSignal::QueueMessage(UserMessage {
                    text: prompt.to_string(),
                    attachments: Vec::new(),
                }),
            )
            .await?;
        wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await?;
        let events = store
            .get_events(session.session_id, EventRange::all())
            .await?;
        total_brain_responses += events
            .iter()
            .filter(|record| matches!(record.event, moa_core::Event::BrainResponse { .. }))
            .count();
    }
    assert_eq!(total_brain_responses, 3);

    let scrape = scrape_metrics(&metrics_url).await?;
    assert!(scrape.contains("moa_sessions_total"));
    assert!(scrape.contains("moa_turns_total"));
    assert!(scrape.contains("moa_turn_latency_seconds"));
    assert!(scrape.contains("moa_sessions_active"));

    assert!(metric_sum(&scrape, "moa_turns_total") >= 3.0);
    assert!(metric_sum(&scrape, "moa_sessions_total") >= 3.0);

    #[cfg(tokio_unstable)]
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        let tokio_scrape = loop {
            let body = scrape_metrics(&metrics_url).await?;
            if body.contains("tokio_workers_count")
                && body.contains("tokio_global_queue_depth")
                && body.contains("tokio_worker_mean_poll_time_us")
                && body.contains("moa_session_task_mean_poll_duration_us")
                && body.contains("moa_session_task_mean_first_poll_delay_us")
            {
                break body;
            }

            if Instant::now() >= deadline {
                return Err(moa_core::MoaError::ProviderError(
                    "tokio runtime metrics did not appear in Prometheus scrape".to_string(),
                ));
            }
            sleep(Duration::from_millis(50)).await;
        };

        assert!(tokio_scrape.contains("tokio_workers_count"));
        assert!(tokio_scrape.contains("tokio_global_queue_depth"));
        assert!(tokio_scrape.contains("tokio_worker_mean_poll_time_us"));
        assert!(tokio_scrape.contains("moa_session_task_mean_poll_duration_us"));
        assert!(tokio_scrape.contains("moa_session_task_mean_first_poll_delay_us"));
    }

    Ok(())
}
