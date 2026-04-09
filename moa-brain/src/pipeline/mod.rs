//! Context pipeline runner and stage assembly.

use std::sync::Arc;
use std::time::Instant;

use moa_core::{
    ContextProcessor, EventRange, EventRecord, MoaConfig, ProcessorOutput, Result, SessionStore,
    WorkingContext,
};

pub mod cache;
pub mod history;
pub mod identity;
pub mod instructions;
pub mod memory;
pub mod skills;
pub mod tools;

use cache::CacheOptimizer;
use history::HistoryCompiler;
use identity::IdentityProcessor;
use instructions::InstructionProcessor;
use memory::MemoryRetriever;
use skills::SkillInjector;
use tools::ToolDefinitionProcessor;

pub(crate) const HISTORY_EVENTS_METADATA_KEY: &str = "moa.pipeline.history_events";

/// Per-stage pipeline execution report.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineStageReport {
    /// Stable stage number.
    pub stage: u8,
    /// Human-readable stage name.
    pub name: String,
    /// Stage output metrics.
    pub output: ProcessorOutput,
}

/// Ordered context compilation pipeline.
pub struct ContextPipeline {
    session_store: Arc<dyn SessionStore>,
    stages: Vec<Box<dyn ContextProcessor>>,
}

impl ContextPipeline {
    /// Creates a pipeline from an ordered list of processors.
    pub fn new(
        session_store: Arc<dyn SessionStore>,
        stages: Vec<Box<dyn ContextProcessor>>,
    ) -> Self {
        Self {
            session_store,
            stages,
        }
    }

    /// Runs the configured pipeline against a working context.
    pub async fn run(&self, ctx: &mut WorkingContext) -> Result<Vec<PipelineStageReport>> {
        let mut reports = Vec::with_capacity(self.stages.len());

        for stage in &self.stages {
            if stage.stage() == 6 && !ctx.metadata.contains_key(HISTORY_EVENTS_METADATA_KEY) {
                let events = self
                    .session_store
                    .get_events(ctx.session_id.clone(), EventRange::all())
                    .await?;
                ctx.metadata.insert(
                    HISTORY_EVENTS_METADATA_KEY.to_string(),
                    serde_json::to_value(events)?,
                );
            }

            let started_at = Instant::now();
            let tokens_before = ctx.token_count;
            let mut output = stage.process(ctx)?;
            output.duration = started_at.elapsed();

            tracing::info!(
                stage = stage.stage(),
                name = stage.name(),
                tokens_before,
                tokens_after = ctx.token_count,
                tokens_added = output.tokens_added,
                tokens_removed = output.tokens_removed,
                items_included = ?output.items_included,
                items_excluded = ?output.items_excluded,
                duration_ms = output.duration.as_millis(),
                "pipeline stage completed"
            );

            reports.push(PipelineStageReport {
                stage: stage.stage(),
                name: stage.name().to_string(),
                output,
            });
        }

        Ok(reports)
    }
}

/// Builds the default seven-stage context pipeline.
pub fn build_default_pipeline(
    config: &MoaConfig,
    session_store: Arc<dyn SessionStore>,
) -> ContextPipeline {
    ContextPipeline::new(
        session_store,
        vec![
            Box::new(IdentityProcessor),
            Box::new(InstructionProcessor::from_config(config)),
            Box::new(ToolDefinitionProcessor::default()),
            Box::new(SkillInjector),
            Box::new(MemoryRetriever),
            Box::new(HistoryCompiler),
            Box::new(CacheOptimizer),
        ],
    )
}

pub(crate) fn estimate_tokens(text: &str) -> usize {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0
    } else {
        trimmed.chars().count().div_ceil(4)
    }
}

pub(crate) fn load_history_events(ctx: &WorkingContext) -> Result<Vec<EventRecord>> {
    match ctx.metadata.get(HISTORY_EVENTS_METADATA_KEY) {
        Some(value) => serde_json::from_value(value.clone()).map_err(Into::into),
        None => Ok(Vec::new()),
    }
}

pub(crate) fn sort_json_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                sort_json_keys(item);
            }
        }
        serde_json::Value::Object(map) => {
            let mut ordered = map
                .iter()
                .map(|(key, value)| {
                    let mut value = value.clone();
                    sort_json_keys(&mut value);
                    (key.clone(), value)
                })
                .collect::<Vec<_>>();
            ordered.sort_by(|left, right| left.0.cmp(&right.0));

            map.clear();
            for (key, value) in ordered {
                map.insert(key, value);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use chrono::Utc;
    use moa_core::{
        ContextProcessor, Event, EventFilter, EventRange, EventRecord, MoaError, ModelCapabilities,
        Platform, ProcessorOutput, Result, SequenceNum, SessionFilter, SessionId, SessionMeta,
        SessionStatus, SessionStore, SessionSummary, TokenPricing, ToolCallFormat, UserId,
        WorkingContext, WorkspaceId,
    };
    use serde_json::json;
    use tokio::sync::Mutex;

    use super::{ContextPipeline, PipelineStageReport, estimate_tokens};

    #[derive(Clone)]
    struct MockSessionStore {
        session: Arc<Mutex<SessionMeta>>,
        events: Arc<Mutex<Vec<EventRecord>>>,
    }

    impl MockSessionStore {
        fn new(session: SessionMeta, events: Vec<EventRecord>) -> Self {
            Self {
                session: Arc::new(Mutex::new(session)),
                events: Arc::new(Mutex::new(events)),
            }
        }
    }

    #[async_trait]
    impl SessionStore for MockSessionStore {
        async fn create_session(&self, meta: SessionMeta) -> Result<SessionId> {
            let id = meta.id.clone();
            *self.session.lock().await = meta;
            Ok(id)
        }

        async fn emit_event(&self, _session_id: SessionId, event: Event) -> Result<SequenceNum> {
            let mut events = self.events.lock().await;
            let sequence_num = events.len() as SequenceNum;
            let session = self.session.lock().await.clone();
            events.push(EventRecord {
                id: uuid::Uuid::new_v4(),
                session_id: session.id,
                sequence_num,
                event_type: event.event_type(),
                event,
                timestamp: Utc::now(),
                brain_id: None,
                hand_id: None,
                token_count: None,
            });
            Ok(sequence_num)
        }

        async fn get_events(
            &self,
            _session_id: SessionId,
            _range: EventRange,
        ) -> Result<Vec<EventRecord>> {
            Ok(self.events.lock().await.clone())
        }

        async fn get_session(&self, _session_id: SessionId) -> Result<SessionMeta> {
            Ok(self.session.lock().await.clone())
        }

        async fn update_status(&self, _session_id: SessionId, status: SessionStatus) -> Result<()> {
            self.session.lock().await.status = status;
            Ok(())
        }

        async fn search_events(
            &self,
            _query: &str,
            _filter: EventFilter,
        ) -> Result<Vec<EventRecord>> {
            Ok(Vec::new())
        }

        async fn list_sessions(&self, _filter: SessionFilter) -> Result<Vec<SessionSummary>> {
            Ok(Vec::new())
        }
    }

    struct TestStage {
        stage: u8,
        name: &'static str,
    }

    impl TestStage {
        fn new(stage: u8, name: &'static str) -> Self {
            Self { stage, name }
        }
    }

    impl ContextProcessor for TestStage {
        fn name(&self) -> &str {
            self.name
        }

        fn stage(&self) -> u8 {
            self.stage
        }

        fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
            let mut order = ctx
                .metadata
                .get("stage_order")
                .cloned()
                .unwrap_or_else(|| json!([]));
            let Some(order_items) = order.as_array_mut() else {
                return Err(MoaError::ValidationError(
                    "stage order metadata must be an array".to_string(),
                ));
            };
            order_items.push(json!(self.name));
            ctx.metadata.insert("stage_order".to_string(), order);

            Ok(ProcessorOutput {
                tokens_added: estimate_tokens(self.name),
                ..ProcessorOutput::default()
            })
        }
    }

    #[tokio::test]
    async fn pipeline_runner_executes_stages_in_order() {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Tui,
            model: "claude-sonnet-4-6".to_string(),
            ..SessionMeta::default()
        };
        let store = Arc::new(MockSessionStore::new(session.clone(), Vec::new()));
        let pipeline = ContextPipeline::new(
            store,
            vec![
                Box::new(TestStage::new(1, "identity")),
                Box::new(TestStage::new(2, "instructions")),
                Box::new(TestStage::new(3, "tools")),
            ],
        );
        let capabilities = ModelCapabilities {
            model_id: "claude-sonnet-4-6".to_string(),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
            },
        };
        let mut ctx = WorkingContext::new(&session, capabilities);

        let reports = pipeline.run(&mut ctx).await.unwrap();

        assert_eq!(
            reports
                .iter()
                .map(|report: &PipelineStageReport| report.name.as_str())
                .collect::<Vec<_>>(),
            vec!["identity", "instructions", "tools"]
        );
        assert_eq!(
            ctx.metadata["stage_order"],
            json!(["identity", "instructions", "tools"])
        );
    }
}
