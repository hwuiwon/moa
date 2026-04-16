//! Context pipeline runner and stage assembly.

use std::sync::Arc;
use std::time::Instant;

use moa_core::{
    ContextProcessor, LLMProvider, MemoryStore, MoaConfig, ProcessorOutput, Result, SessionStore,
    WorkingContext,
};
use tracing::Instrument;

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
    stages: Vec<Box<dyn ContextProcessor>>,
    daily_workspace_budget_cents: u32,
}

impl ContextPipeline {
    /// Creates a pipeline from an ordered list of processors.
    pub fn new(stages: Vec<Box<dyn ContextProcessor>>) -> Self {
        Self::with_daily_workspace_budget(stages, 0)
    }

    /// Creates a pipeline from an ordered list of processors and a workspace budget limit.
    pub fn with_daily_workspace_budget(
        stages: Vec<Box<dyn ContextProcessor>>,
        daily_workspace_budget_cents: u32,
    ) -> Self {
        Self {
            stages,
            daily_workspace_budget_cents,
        }
    }

    /// Returns the configured daily workspace budget limit in cents.
    pub fn daily_workspace_budget_cents(&self) -> u32 {
        self.daily_workspace_budget_cents
    }

    /// Runs the configured pipeline against a working context.
    pub async fn run(&self, ctx: &mut WorkingContext) -> Result<Vec<PipelineStageReport>> {
        let pipeline_span = tracing::info_span!(
            "context_compilation",
            moa.session.id = %ctx.session_id,
            moa.user.id = %ctx.user_id,
            moa.workspace.id = %ctx.workspace_id,
            moa.model = %ctx.model_capabilities.model_id,
            langfuse.session.id = %ctx.session_id,
            langfuse.user.id = %ctx.user_id,
            langfuse.trace.metadata.workspace_id = %ctx.workspace_id,
            langfuse.trace.metadata.model = %ctx.model_capabilities.model_id,
            moa.pipeline.stage_count = self.stages.len() as i64,
            moa.pipeline.total_tokens = tracing::field::Empty,
            moa.pipeline.cache_ratio = tracing::field::Empty,
            moa.pipeline.cache_breakpoints = tracing::field::Empty,
        );

        let instrument_pipeline_span = pipeline_span.clone();
        async {
            let mut reports = Vec::with_capacity(self.stages.len());

            for stage in &self.stages {
                let stage_name = stage.name().to_string();
                let stage_span_name = format!("pipeline.stage {stage_name}");
                let stage_span = tracing::info_span!(
                    "pipeline_stage",
                    otel.name = %stage_span_name,
                    moa.session.id = %ctx.session_id,
                    moa.user.id = %ctx.user_id,
                    moa.workspace.id = %ctx.workspace_id,
                    moa.model = %ctx.model_capabilities.model_id,
                    langfuse.session.id = %ctx.session_id,
                    langfuse.user.id = %ctx.user_id,
                    langfuse.trace.metadata.workspace_id = %ctx.workspace_id,
                    langfuse.trace.metadata.model = %ctx.model_capabilities.model_id,
                    moa.pipeline.stage.number = stage.stage() as i64,
                    moa.pipeline.stage.name = %stage_name,
                    moa.pipeline.stage.tokens_added = tracing::field::Empty,
                    moa.pipeline.stage.tokens_removed = tracing::field::Empty,
                    moa.pipeline.stage.items_included = tracing::field::Empty,
                    moa.pipeline.stage.items_excluded = tracing::field::Empty,
                    moa.pipeline.stage.tokens_before = tracing::field::Empty,
                    moa.pipeline.stage.tokens_after = tracing::field::Empty,
                );

                let started_at = Instant::now();
                let tokens_before = ctx.token_count;
                stage_span.record("moa.pipeline.stage.tokens_before", tokens_before as i64);
                let instrument_stage_span = stage_span.clone();
                let mut output = async { stage.process(ctx).await }
                    .instrument(instrument_stage_span)
                    .await?;
                output.duration = started_at.elapsed();
                let tokens_after = ctx.token_count;

                stage_span.record(
                    "moa.pipeline.stage.tokens_added",
                    output.tokens_added as i64,
                );
                stage_span.record(
                    "moa.pipeline.stage.tokens_removed",
                    output.tokens_removed as i64,
                );
                stage_span.record(
                    "moa.pipeline.stage.items_included",
                    output.items_included.len() as i64,
                );
                stage_span.record(
                    "moa.pipeline.stage.items_excluded",
                    output.items_excluded.len() as i64,
                );
                stage_span.record("moa.pipeline.stage.tokens_after", tokens_after as i64);

                tracing::info!(
                    stage = stage.stage(),
                    name = stage.name(),
                    tokens_before,
                    tokens_after,
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

            let cache_ratio = cache_prefix_ratio(ctx);
            pipeline_span.record("moa.pipeline.total_tokens", ctx.token_count as i64);
            pipeline_span.record("moa.pipeline.cache_ratio", cache_ratio);
            pipeline_span.record(
                "moa.pipeline.cache_breakpoints",
                ctx.cache_breakpoints.len() as i64,
            );
            ctx.insert_metadata("_moa.context_tokens", serde_json::json!(ctx.token_count));
            ctx.insert_metadata("_moa.cache_ratio", serde_json::json!(cache_ratio));

            Ok(reports)
        }
        .instrument(instrument_pipeline_span)
        .await
    }
}

/// Builds the default seven-stage context pipeline.
pub fn build_default_pipeline(
    config: &MoaConfig,
    session_store: Arc<dyn SessionStore>,
    memory_store: Arc<dyn MemoryStore>,
) -> ContextPipeline {
    build_default_pipeline_with_tools(config, session_store, memory_store, Vec::new())
}

/// Builds the default seven-stage context pipeline with a fixed tool loadout.
pub fn build_default_pipeline_with_tools(
    config: &MoaConfig,
    session_store: Arc<dyn SessionStore>,
    memory_store: Arc<dyn MemoryStore>,
    tool_schemas: Vec<serde_json::Value>,
) -> ContextPipeline {
    build_default_pipeline_with_runtime(config, session_store, memory_store, None, tool_schemas)
}

/// Builds the default seven-stage context pipeline with an optional compaction-capable LLM.
pub fn build_default_pipeline_with_runtime(
    config: &MoaConfig,
    session_store: Arc<dyn SessionStore>,
    memory_store: Arc<dyn MemoryStore>,
    llm_provider: Option<Arc<dyn LLMProvider>>,
    tool_schemas: Vec<serde_json::Value>,
) -> ContextPipeline {
    build_default_pipeline_with_runtime_and_instructions(
        config,
        session_store,
        memory_store,
        llm_provider,
        None,
        tool_schemas,
    )
}

/// Builds the default seven-stage context pipeline with optional discovered workspace instructions.
pub fn build_default_pipeline_with_runtime_and_instructions(
    config: &MoaConfig,
    session_store: Arc<dyn SessionStore>,
    memory_store: Arc<dyn MemoryStore>,
    llm_provider: Option<Arc<dyn LLMProvider>>,
    discovered_workspace_instructions: Option<String>,
    tool_schemas: Vec<serde_json::Value>,
) -> ContextPipeline {
    let history: Box<dyn ContextProcessor> = if let Some(llm_provider) = llm_provider {
        Box::new(
            HistoryCompiler::with_compaction(
                session_store.clone(),
                llm_provider,
                config.compaction.clone(),
            )
            .with_tool_output_config(config.tool_output.clone()),
        )
    } else {
        Box::new(
            HistoryCompiler::new(session_store.clone())
                .with_tool_output_config(config.tool_output.clone()),
        )
    };
    ContextPipeline::with_daily_workspace_budget(
        vec![
            Box::new(IdentityProcessor::default()),
            Box::new(InstructionProcessor::new(
                config.general.workspace_instructions.clone(),
                config.general.user_instructions.clone(),
                discovered_workspace_instructions,
            )),
            Box::new(ToolDefinitionProcessor::with_memory(
                tool_schemas,
                memory_store.clone(),
            )),
            Box::new(SkillInjector::from_memory(memory_store.clone())),
            Box::new(MemoryRetriever::new(memory_store, session_store.clone())),
            history,
            Box::new(CacheOptimizer),
        ],
        config.budgets.daily_workspace_cents,
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

fn cache_prefix_ratio(ctx: &WorkingContext) -> f64 {
    if ctx.token_count == 0 {
        return 1.0;
    }

    let Some(cache_breakpoint) = ctx.cache_breakpoints.last().copied() else {
        return 0.0;
    };

    let prefix_tokens = ctx.messages[..cache_breakpoint.min(ctx.messages.len())]
        .iter()
        .map(|message| estimate_tokens(&message.content))
        .sum::<usize>();

    prefix_tokens as f64 / ctx.token_count as f64
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use moa_core::{
        ContextProcessor, MoaError, ModelCapabilities, Platform, ProcessorOutput, Result,
        SessionId, SessionMeta, TokenPricing, ToolCallFormat, UserId, WorkingContext, WorkspaceId,
    };
    use serde_json::json;

    use super::{ContextPipeline, PipelineStageReport, estimate_tokens};

    struct TestStage {
        stage: u8,
        name: &'static str,
    }

    impl TestStage {
        fn new(stage: u8, name: &'static str) -> Self {
            Self { stage, name }
        }
    }

    #[async_trait]
    impl ContextProcessor for TestStage {
        fn name(&self) -> &str {
            self.name
        }

        fn stage(&self) -> u8 {
            self.stage
        }

        async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
            let mut order = ctx
                .metadata()
                .get("stage_order")
                .cloned()
                .unwrap_or_else(|| json!([]));
            let Some(order_items) = order.as_array_mut() else {
                return Err(MoaError::ValidationError(
                    "stage order metadata must be an array".to_string(),
                ));
            };
            order_items.push(json!(self.name));
            ctx.insert_metadata("stage_order", order);

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
        let pipeline = ContextPipeline::new(vec![
            Box::new(TestStage::new(1, "identity")),
            Box::new(TestStage::new(2, "instructions")),
            Box::new(TestStage::new(3, "tools")),
        ]);
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
            native_tools: Vec::new(),
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
            ctx.metadata().get("stage_order"),
            Some(&json!(["identity", "instructions", "tools"]))
        );
    }
}
