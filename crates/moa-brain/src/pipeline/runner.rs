//! Context pipeline runner and per-stage reporting.

use std::time::Instant;

use moa_core::{
    ContextProcessor, ContextSnapshotConfig, MessageRole, ProcessorOutput, Result, WorkingContext,
    record_query_rewrite_decision,
};
use tracing::Instrument;

use super::util::estimate_tokens;

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
    snapshot_config: ContextSnapshotConfig,
}

impl ContextPipeline {
    /// Creates a pipeline from an ordered list of processors.
    pub fn new(stages: Vec<Box<dyn ContextProcessor>>) -> Self {
        Self::with_runtime_limits(stages, 0, ContextSnapshotConfig::default())
    }

    /// Creates a pipeline from an ordered list of processors and runtime limits.
    pub fn with_runtime_limits(
        stages: Vec<Box<dyn ContextProcessor>>,
        daily_workspace_budget_cents: u32,
        snapshot_config: ContextSnapshotConfig,
    ) -> Self {
        Self {
            stages,
            daily_workspace_budget_cents,
            snapshot_config,
        }
    }

    /// Returns the configured daily workspace budget limit in cents.
    pub fn daily_workspace_budget_cents(&self) -> u32 {
        self.daily_workspace_budget_cents
    }

    /// Returns the number of configured pipeline stages.
    pub fn stage_count(&self) -> usize {
        self.stages.len()
    }

    /// Returns the configured processor names in execution order.
    pub fn stage_names(&self) -> Vec<&str> {
        self.stages.iter().map(|stage| stage.name()).collect()
    }

    /// Returns the snapshot configuration used by history compilation.
    pub fn snapshot_config(&self) -> &ContextSnapshotConfig {
        &self.snapshot_config
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
                    moa.query_rewrite.decision = tracing::field::Empty,
                    moa.query_rewrite.reason = tracing::field::Empty,
                    moa.query_rewrite.llm_called = tracing::field::Empty,
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
                if stage.name() == "query_rewrite" {
                    record_query_rewrite_stage_metadata(&stage_span, &output);
                }

                tracing::info!(
                    stage = stage.stage(),
                    name = stage.name(),
                    tokens_before,
                    tokens_after,
                    tokens_added = output.tokens_added,
                    tokens_removed = output.tokens_removed,
                    items_included = ?output.items_included,
                    items_excluded = ?output.items_excluded,
                    excluded_items = ?output.excluded_items,
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
            ctx.insert_metadata("_moa.context_tokens", serde_json::json!(ctx.token_count));
            ctx.insert_metadata("_moa.cache_ratio", serde_json::json!(cache_ratio));

            Ok(reports)
        }
        .instrument(instrument_pipeline_span)
        .await
    }
}

fn record_query_rewrite_stage_metadata(span: &tracing::Span, output: &ProcessorOutput) {
    let Some(decision) = metadata_str(output, "moa.query_rewrite.decision") else {
        return;
    };
    let reason = metadata_str(output, "moa.query_rewrite.reason").unwrap_or(decision);
    let llm_called = output
        .metadata
        .get("moa.query_rewrite.llm_called")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    span.record(
        "moa.query_rewrite.decision",
        tracing::field::display(decision),
    );
    span.record("moa.query_rewrite.reason", tracing::field::display(reason));
    span.record("moa.query_rewrite.llm_called", llm_called);
    record_query_rewrite_decision(decision, reason, llm_called, output.duration);
}

fn metadata_str<'a>(output: &'a ProcessorOutput, key: &str) -> Option<&'a str> {
    output.metadata.get(key).and_then(serde_json::Value::as_str)
}

fn cache_prefix_ratio(ctx: &WorkingContext) -> f64 {
    let tool_tokens = ctx
        .tools()
        .iter()
        .map(|tool| estimate_tokens(&tool.to_string()))
        .sum::<usize>();
    let total_tokens = ctx.token_count + tool_tokens;

    if total_tokens == 0 {
        return 1.0;
    }

    let stable_message_count = ctx
        .messages
        .iter()
        .take_while(|message| message.role == MessageRole::System)
        .count();
    let prefix_tokens = ctx.messages[..stable_message_count]
        .iter()
        .map(|message| estimate_tokens(&message.content))
        .sum::<usize>()
        + tool_tokens;

    prefix_tokens as f64 / total_tokens as f64
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use moa_core::{
        ContextMessage, ContextProcessor, MoaError, ModelCapabilities, ModelId, Platform,
        ProcessorOutput, Result, SessionId, SessionMeta, TokenPricing, ToolCallFormat, UserId,
        WorkingContext, WorkspaceId,
    };
    use serde_json::json;

    use super::{ContextPipeline, PipelineStageReport, cache_prefix_ratio};
    use crate::pipeline::estimate_tokens;

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
            platform: Platform::Api,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let pipeline = ContextPipeline::new(vec![
            Box::new(TestStage::new(1, "identity")),
            Box::new(TestStage::new(2, "instructions")),
            Box::new(TestStage::new(3, "tools")),
        ]);
        let capabilities = capabilities();
        let mut ctx = WorkingContext::new(&session, capabilities);

        let reports = pipeline
            .run(&mut ctx)
            .await
            .expect("pipeline should run stages");

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

    #[test]
    fn cache_prefix_ratio_includes_tool_tokens() {
        // Pins: stable-prefix ratio counts deterministic tool schemas and leading system messages only.
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Api,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let mut ctx = WorkingContext::new(&session, capabilities());
        ctx.set_tools(vec![json!({
            "name": "bash",
            "description": "Run shell commands",
            "input_schema": {
                "type": "object",
                "properties": {
                    "cmd": {"type": "string"}
                }
            }
        })]);
        ctx.append_system("identity");
        ctx.append_message(ContextMessage::user("hello"));

        let ratio = cache_prefix_ratio(&ctx);

        assert!(
            ratio > 0.5,
            "tool tokens should contribute to the cached prefix"
        );
    }

    fn capabilities() -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("claude-sonnet-4-6"),
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
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }
}
