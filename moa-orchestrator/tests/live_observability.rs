//! Live observability audit for steps 79, 80, and 81.

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use moa_core::{
    BrainOrchestrator, Event, EventRange, LLMProvider, MoaConfig, Platform, Result, SessionSignal,
    SessionStatus, SessionStore, StartSessionRequest, UserId, UserMessage, WorkspaceId,
};
use moa_hands::ToolRouter;
use moa_memory::FileMemoryStore;
use moa_orchestrator::LocalOrchestrator;
use moa_providers::build_provider_from_config;
use moa_session::{PostgresSessionStore, testing};
use tempfile::TempDir;
use tokio::time::{Instant, sleep};
use tracing::field::{Field, Visit};
use tracing::{Event as TracingEvent, Id, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

#[derive(Debug, Clone, Default)]
struct RecordedFields {
    values: BTreeMap<String, String>,
}

impl RecordedFields {
    fn get_u64(&self, key: &str) -> Option<u64> {
        self.values.get(key)?.parse().ok()
    }

    fn get_message(&self) -> Option<&str> {
        self.values.get("message").map(String::as_str)
    }
}

#[derive(Debug, Clone)]
struct RecordedSpan {
    name: String,
    fields: RecordedFields,
}

#[derive(Debug, Clone)]
struct RecordedEvent {
    _name: String,
    fields: RecordedFields,
}

#[derive(Debug, Default)]
struct TraceRecorder {
    spans: Mutex<HashMap<String, RecordedSpan>>,
    events: Mutex<Vec<RecordedEvent>>,
}

#[derive(Clone)]
struct TraceLayer {
    recorder: Arc<TraceRecorder>,
}

impl TraceRecorder {
    fn clear(&self) {
        self.spans.lock().expect("span lock").clear();
        self.events.lock().expect("event lock").clear();
    }

    fn spans_named(&self, name: &str) -> Vec<RecordedSpan> {
        self.spans
            .lock()
            .expect("span lock")
            .values()
            .filter(|span| span.name == name)
            .cloned()
            .collect()
    }

    fn events_with_message(&self, message: &str) -> Vec<RecordedEvent> {
        self.events
            .lock()
            .expect("event lock")
            .iter()
            .filter(|event| event.fields.get_message() == Some(message))
            .cloned()
            .collect()
    }
}

impl<S> Layer<S> for TraceLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &tracing::span::Attributes<'_>, id: &Id, _ctx: Context<'_, S>) {
        let mut visitor = FieldRecorder::default();
        attrs.record(&mut visitor);
        self.recorder.spans.lock().expect("span lock").insert(
            span_key(id),
            RecordedSpan {
                name: attrs.metadata().name().to_string(),
                fields: visitor.finish(),
            },
        );
    }

    fn on_record(&self, id: &Id, values: &tracing::span::Record<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldRecorder::default();
        values.record(&mut visitor);
        let recorded = visitor.finish();
        if let Some(span) = self
            .recorder
            .spans
            .lock()
            .expect("span lock")
            .get_mut(&span_key(id))
        {
            span.fields.values.extend(recorded.values);
        }
    }

    fn on_event(&self, event: &TracingEvent<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldRecorder::default();
        event.record(&mut visitor);
        self.recorder
            .events
            .lock()
            .expect("event lock")
            .push(RecordedEvent {
                _name: event.metadata().name().to_string(),
                fields: visitor.finish(),
            });
    }
}

#[derive(Debug, Default)]
struct FieldRecorder {
    values: BTreeMap<String, String>,
}

impl FieldRecorder {
    fn finish(self) -> RecordedFields {
        RecordedFields {
            values: self.values,
        }
    }
}

impl Visit for FieldRecorder {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.values
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.values
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.values
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.values
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.values
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

fn span_key(id: &Id) -> String {
    format!("{id:?}")
}

fn global_trace_recorder() -> Arc<TraceRecorder> {
    static RECORDER: OnceLock<Arc<TraceRecorder>> = OnceLock::new();
    RECORDER
        .get_or_init(|| {
            let recorder = Arc::new(TraceRecorder::default());
            let subscriber = tracing_subscriber::registry().with(TraceLayer {
                recorder: recorder.clone(),
            });
            tracing::subscriber::set_global_default(subscriber)
                .expect("global tracing subscriber should install once");
            recorder
        })
        .clone()
}

async fn live_orchestrator(
    repo_root: &Path,
) -> Result<(TempDir, Arc<PostgresSessionStore>, LocalOrchestrator)> {
    let dir = tempfile::tempdir()?;
    let mut config = MoaConfig::default();
    config.memory.auto_bootstrap = false;
    config.general.default_provider = "anthropic".to_string();
    config.general.default_model = "claude-sonnet-4-6".to_string();
    config.local.memory_dir = dir.path().join("memory").display().to_string();
    config.local.sandbox_dir = repo_root.display().to_string();

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
    let provider: Arc<dyn LLMProvider> = build_provider_from_config(&config)?;
    let orchestrator = LocalOrchestrator::new(
        config,
        session_store.clone(),
        memory_store,
        provider,
        tool_router,
    )
    .await?;

    Ok((dir, session_store, orchestrator))
}

async fn wait_for_status(
    orchestrator: &LocalOrchestrator,
    session_id: moa_core::SessionId,
    expected: SessionStatus,
) {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let session = orchestrator
            .get_session(session_id)
            .await
            .expect("session metadata");
        if session.status == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for status {:?}; current {:?}",
            expected,
            session.status
        );
        sleep(Duration::from_millis(250)).await;
    }
}

async fn queue_message(
    orchestrator: &LocalOrchestrator,
    session_id: moa_core::SessionId,
    text: &str,
) -> Result<()> {
    orchestrator
        .signal(
            session_id,
            SessionSignal::QueueMessage(UserMessage {
                text: text.to_string(),
                attachments: Vec::new(),
            }),
        )
        .await
}

fn repo_root() -> Result<PathBuf> {
    let cwd = env::current_dir()?;
    for candidate in cwd.ancestors() {
        if candidate.join("Cargo.toml").exists() && candidate.join("moa-orchestrator").exists() {
            return Ok(candidate.to_path_buf());
        }
    }
    Err(moa_core::MoaError::ValidationError(format!(
        "could not locate repo root from {}",
        cwd.display()
    )))
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY and performs live observability audit"]
async fn live_observability_audit_tracks_cache_replay_and_latency() -> Result<()> {
    let recorder = global_trace_recorder();
    recorder.clear();

    let repo_root = repo_root()?;
    let (_dir, session_store, orchestrator) = live_orchestrator(&repo_root).await?;

    let workspace_id = WorkspaceId::new("live-observability");
    orchestrator
        .remember_workspace_root(workspace_id.clone(), repo_root)
        .await;

    let session = orchestrator
        .start_session(StartSessionRequest {
            workspace_id,
            user_id: UserId::new("live-observability-user"),
            platform: Platform::Cli,
            model: moa_core::ModelId::new("claude-sonnet-4-6"),
            initial_message: Some(UserMessage {
                text: "Reply with READY and nothing else.".to_string(),
                attachments: Vec::new(),
            }),
            title: None,
            parent_session_id: None,
        })
        .await?;

    queue_message(
        &orchestrator,
        session.session_id,
        "Use the file_read tool to read moa-brain/Cargo.toml and answer with only the package name.",
    )
    .await?;
    queue_message(
        &orchestrator,
        session.session_id,
        "Reply with STEADY and nothing else.",
    )
    .await?;

    wait_for_status(&orchestrator, session.session_id, SessionStatus::Completed).await;

    let events = session_store
        .get_events(session.session_id, EventRange::all())
        .await?;
    let brain_responses = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::BrainResponse {
                text,
                input_tokens_uncached,
                input_tokens_cache_write,
                input_tokens_cache_read,
                output_tokens,
                ..
            } => Some((
                text.clone(),
                *input_tokens_uncached,
                *input_tokens_cache_write,
                *input_tokens_cache_read,
                *output_tokens,
            )),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert!(
        brain_responses.len() >= 3,
        "expected at least three live turns, found {}",
        brain_responses.len()
    );
    assert!(
        brain_responses
            .iter()
            .skip(1)
            .any(|(_, _, _, cache_read, _)| *cache_read > 0),
        "expected a persisted BrainResponse event with cache-read tokens after the first turn"
    );

    let session_meta = orchestrator.get_session(session.session_id).await?;
    assert!(
        session_meta.total_input_tokens_cache_read > 0,
        "expected session aggregate cache-read tokens to be non-zero"
    );

    let replay_events = recorder.events_with_message("turn event replay summary");
    assert!(
        replay_events.len() >= 3,
        "expected per-turn replay summaries, found {}",
        replay_events.len()
    );
    let replay_counts = replay_events
        .iter()
        .filter_map(|event| event.fields.get_u64("events_replayed"))
        .collect::<Vec<_>>();
    assert!(
        replay_counts.len() >= 3,
        "expected replay summaries to include events_replayed"
    );
    assert!(
        replay_counts.last().copied().unwrap_or_default() > replay_counts[0],
        "expected event replay count to grow across turns: {:?}",
        replay_counts
    );

    let latency_events = recorder.events_with_message("turn latency breakdown");
    assert!(
        latency_events.len() >= 3,
        "expected per-turn latency summaries, found {}",
        latency_events.len()
    );
    assert!(
        latency_events.iter().all(|event| {
            event
                .fields
                .get_u64("pipeline_compile_ms")
                .unwrap_or_default()
                > 0
                && event.fields.get_u64("llm_call_ms").unwrap_or_default() > 0
                && event.fields.get_u64("event_persist_ms").unwrap_or_default() > 0
        }),
        "expected latency summaries to include non-zero compile/llm/persist durations"
    );
    assert!(
        latency_events.iter().any(|event| event
            .fields
            .get_u64("tool_dispatch_ms")
            .unwrap_or_default()
            > 0),
        "expected at least one live turn with non-zero tool dispatch duration"
    );

    for span_name in [
        "pipeline_compile",
        "llm_call",
        "tool_dispatch",
        "event_persist",
    ] {
        assert!(
            !recorder.spans_named(span_name).is_empty(),
            "expected live trace spans named {span_name}"
        );
    }

    let llm_spans = recorder.spans_named("llm_call");
    assert!(
        llm_spans.iter().any(|span| {
            span.fields
                .get_u64("gen_ai.response.first_token_at_ms")
                .unwrap_or_default()
                > 0
        }),
        "expected at least one llm_call span with TTFT recorded"
    );

    let tool_spans = recorder
        .spans
        .lock()
        .expect("span lock")
        .values()
        .filter(|span| span.name == "tool_execution")
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        tool_spans.iter().any(|span| {
            span.fields
                .values
                .get("otel.name")
                .is_some_and(|value| value.starts_with("tool:"))
        }),
        "expected tool_execution spans to export tool-specific otel.name values"
    );

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "brain_responses": brain_responses,
            "session_cache_hit_rate": session_meta.cache_hit_rate(),
            "session_cache_read_tokens": session_meta.total_input_tokens_cache_read,
            "replay_counts": replay_counts,
            "latency_summaries": latency_events
                .iter()
                .map(|event| event.fields.values.clone())
                .collect::<Vec<_>>(),
        }))?
    );

    Ok(())
}
