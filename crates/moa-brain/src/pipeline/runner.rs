//! Context pipeline runner and per-stage reporting.

use std::time::Instant;

use moa_core::{
    ContextProcessor, ContextSnapshotConfig, MessageRole, ProcessorOutput, Result, WorkingContext,
};
use moa_observability::record_query_rewrite_decision;
use tracing::Instrument;

use moa_core::{estimate_text_tokens, sum_message_tokens};

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
    daily_tenant_budget_cents: u32,
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
        daily_tenant_budget_cents: u32,
        snapshot_config: ContextSnapshotConfig,
    ) -> Self {
        Self {
            stages,
            daily_tenant_budget_cents,
            snapshot_config,
        }
    }

    /// Returns the configured daily tenant budget limit in cents.
    pub fn daily_tenant_budget_cents(&self) -> u32 {
        self.daily_tenant_budget_cents
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
        let contact_id = ctx
            .contact
            .as_ref()
            .map(|contact| contact.contact_id.to_string())
            .unwrap_or_else(|| "none".to_string());
        let pipeline_span = tracing::info_span!(
            "context_compilation",
            moa.session.id = %ctx.session_id,
            moa.tenant.id = %ctx.tenant_id,
            moa.contact.id = %contact_id,
            moa.model = %ctx.model_capabilities.model_id,
            moa.pipeline.stage_count = self.stages.len() as i64,
            moa.pipeline.total_tokens = tracing::field::Empty,
            moa.pipeline.cache_ratio = tracing::field::Empty,
        );

        let instrument_pipeline_span = pipeline_span.clone();
        async {
            let mut reports = Vec::with_capacity(self.stages.len());

            // Stages run in a fixed order, but a maximal run of adjacent stages
            // that opt into `ContextProcessor::parallelizable` overlaps its
            // read-only `fetch` I/O concurrently, then `apply`s each result to
            // `&mut WorkingContext` in that same fixed order. Applying serially
            // preserves the deterministic message/tool layout the prompt cache
            // depends on, while the independent per-turn round trips (skill
            // registry reads, standing digest reads, and graph-memory
            // retrieval) run at once instead of one after another. Stages that
            // read post-group context (history budgeting off `token_count`,
            // runtime reminders relative to appended messages) keep
            // `parallelizable() == false` and run sequentially via `process`.
            let mut index = 0;
            while index < self.stages.len() {
                if self.stages[index].parallelizable() {
                    let start = index;
                    while index < self.stages.len() && self.stages[index].parallelizable() {
                        index += 1;
                    }
                    run_parallel_group(&self.stages[start..index], ctx, &contact_id, &mut reports)
                        .await?;
                } else {
                    run_sequential_stage(
                        self.stages[index].as_ref(),
                        ctx,
                        &contact_id,
                        &mut reports,
                    )
                    .await?;
                    index += 1;
                }
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

/// Builds the per-stage tracing span carrying the shared pipeline fields.
fn build_stage_span(
    ctx: &WorkingContext,
    contact_id: &str,
    stage: &dyn ContextProcessor,
) -> tracing::Span {
    let stage_name = stage.name();
    let stage_span_name = format!("pipeline.stage {stage_name}");
    tracing::info_span!(
        "pipeline_stage",
        otel.name = %stage_span_name,
        moa.session.id = %ctx.session_id,
        moa.tenant.id = %ctx.tenant_id,
        moa.contact.id = %contact_id,
        moa.model = %ctx.model_capabilities.model_id,
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
    )
}

/// Runs one stage sequentially through `process`, recording its span and report.
async fn run_sequential_stage(
    stage: &dyn ContextProcessor,
    ctx: &mut WorkingContext,
    contact_id: &str,
    reports: &mut Vec<PipelineStageReport>,
) -> Result<()> {
    let stage_name = stage.name().to_string();
    let stage_span = build_stage_span(ctx, contact_id, stage);
    let started_at = Instant::now();
    let tokens_before = ctx.token_count;
    stage_span.record("moa.pipeline.stage.tokens_before", tokens_before as i64);
    let mut output = async { stage.process(ctx).await }
        .instrument(stage_span.clone())
        .await
        .map_err(|error| stage_error(&stage_name, error))?;
    output.duration = started_at.elapsed();
    let tokens_after = ctx.token_count;
    finalize_stage_report(
        &stage_span,
        stage,
        output,
        tokens_before,
        tokens_after,
        reports,
    );
    Ok(())
}

/// Runs a contiguous run of parallelizable stages: the read-only `fetch` phase
/// runs concurrently, then each result is applied to the context in stage order.
///
/// Ordered application preserves the exact `WorkingContext` a fully sequential
/// run would produce, provided each stage's `fetch` reads only context that no
/// other stage in the group mutates during `apply` — the invariant every
/// `parallelizable` stage must uphold.
async fn run_parallel_group(
    group: &[Box<dyn ContextProcessor>],
    ctx: &mut WorkingContext,
    contact_id: &str,
    reports: &mut Vec<PipelineStageReport>,
) -> Result<()> {
    let spans = group
        .iter()
        .map(|stage| build_stage_span(ctx, contact_id, stage.as_ref()))
        .collect::<Vec<_>>();

    // Fetch phase: every stage's read-only I/O runs against the same immutable
    // context, overlapping the independent round trips.
    let fetched = {
        let ctx_ref: &WorkingContext = ctx;
        let fetches = group.iter().zip(&spans).map(|(stage, span)| {
            let stage_name = stage.name().to_string();
            let span = span.clone();
            async move {
                let started = Instant::now();
                let apply = stage
                    .fetch(ctx_ref)
                    .instrument(span)
                    .await
                    .map_err(|error| stage_error(&stage_name, error))?;
                Ok::<_, moa_core::MoaError>((started.elapsed(), apply))
            }
        });
        futures_util::future::try_join_all(fetches).await?
    };

    // Apply phase: strictly in stage order so message/tool layout is
    // deterministic regardless of which fetch finished first.
    for ((stage, span), (fetch_elapsed, apply)) in group.iter().zip(&spans).zip(fetched) {
        let stage_name = stage.name().to_string();
        let tokens_before = ctx.token_count;
        span.record("moa.pipeline.stage.tokens_before", tokens_before as i64);
        let apply_started = Instant::now();
        let mut output = match apply {
            Some(apply) => {
                let _enter = span.enter();
                apply(ctx).map_err(|error| stage_error(&stage_name, error))?
            }
            None => async { stage.process(ctx).await }
                .instrument(span.clone())
                .await
                .map_err(|error| stage_error(&stage_name, error))?,
        };
        output.duration = fetch_elapsed + apply_started.elapsed();
        let tokens_after = ctx.token_count;
        finalize_stage_report(
            span,
            stage.as_ref(),
            output,
            tokens_before,
            tokens_after,
            reports,
        );
    }
    Ok(())
}

/// Records the stage span fields, logs completion, and appends the stage report.
fn finalize_stage_report(
    stage_span: &tracing::Span,
    stage: &dyn ContextProcessor,
    output: ProcessorOutput,
    tokens_before: usize,
    tokens_after: usize,
    reports: &mut Vec<PipelineStageReport>,
) {
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
        record_query_rewrite_stage_metadata(stage_span, &output);
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

fn stage_error(stage_name: &str, error: moa_core::MoaError) -> moa_core::MoaError {
    let message = format!("context pipeline stage '{stage_name}' failed: {error}");
    match error {
        moa_core::MoaError::ProviderError(_) => moa_core::MoaError::ProviderError(message),
        moa_core::MoaError::MissingEnvironmentVariable(_) => {
            moa_core::MoaError::MissingEnvironmentVariable(message)
        }
        moa_core::MoaError::ConfigError(_) => moa_core::MoaError::ConfigError(message),
        moa_core::MoaError::StorageError(_) => moa_core::MoaError::StorageError(message),
        moa_core::MoaError::ToolError(_) => moa_core::MoaError::ToolError(message),
        moa_core::MoaError::ValidationError(_) => moa_core::MoaError::ValidationError(message),
        moa_core::MoaError::ProviderQuirk(_) => moa_core::MoaError::ProviderQuirk(message),
        moa_core::MoaError::SerializationError(_) => {
            moa_core::MoaError::SerializationError(message)
        }
        moa_core::MoaError::StreamError(_) => moa_core::MoaError::StreamError(message),
        moa_core::MoaError::PermissionDenied(_) => moa_core::MoaError::PermissionDenied(message),
        moa_core::MoaError::BudgetExhausted(_) => moa_core::MoaError::BudgetExhausted(message),
        moa_core::MoaError::Unsupported(_) => moa_core::MoaError::Unsupported(message),
        moa_core::MoaError::NotImplemented(_) => moa_core::MoaError::NotImplemented(message),
        moa_core::MoaError::HttpStatus {
            status,
            retry_after,
            ..
        } => moa_core::MoaError::HttpStatus {
            status,
            retry_after,
            message,
        },
        moa_core::MoaError::RateLimited { retries, .. } => {
            moa_core::MoaError::RateLimited { retries, message }
        }
        _other => moa_core::MoaError::ValidationError(message),
    }
}

fn cache_prefix_ratio(ctx: &WorkingContext) -> f64 {
    // Reuse the tool loadout token count the tools stage already computed rather
    // than re-serializing and re-tokenizing every schema at the end of the turn.
    let tool_tokens = ctx
        .metadata()
        .get(crate::pipeline::tools::TOOLS_TOKEN_COUNT_METADATA_KEY)
        .and_then(serde_json::Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or_else(|| {
            ctx.tools()
                .iter()
                .map(|tool| estimate_text_tokens(&tool.to_string()))
                .sum::<usize>()
        });
    let total_tokens = ctx.token_count + tool_tokens;

    if total_tokens == 0 {
        return 1.0;
    }

    let stable_message_count = ctx
        .messages
        .iter()
        .take_while(|message| message.role == MessageRole::System)
        .count();
    let prefix_tokens = sum_message_tokens(&ctx.messages[..stable_message_count]) + tool_tokens;

    prefix_tokens as f64 / total_tokens as f64
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use moa_core::{
        Channel, ContextMessage, ContextProcessor, MoaError, ModelCapabilities, ModelId,
        ProcessorOutput, Result, SessionId, SessionMeta, StageApply, TokenPricing, ToolCallFormat,
        WorkingContext,
    };
    use serde_json::json;

    use moa_core::estimate_text_tokens;

    use super::{ContextPipeline, PipelineStageReport, cache_prefix_ratio};

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
                tokens_added: estimate_text_tokens(self.name),
                ..ProcessorOutput::default()
            })
        }
    }

    #[tokio::test]
    async fn pipeline_runner_executes_stages_in_order() {
        let session = SessionMeta {
            id: SessionId::new(),
            channel: Channel::Chat,
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

    #[tokio::test]
    async fn pipeline_runner_error_names_the_failing_stage() {
        // Pins: callers can identify which context processor failed without
        // scraping tracing spans or test-only assertion wrappers.
        let session = SessionMeta {
            id: SessionId::new(),
            channel: Channel::Chat,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let pipeline = ContextPipeline::new(vec![Box::new(FailingStage)]);
        let mut ctx = WorkingContext::new(&session, capabilities());

        let error = pipeline
            .run(&mut ctx)
            .await
            .expect_err("failing stage should abort the pipeline");

        match error {
            MoaError::ValidationError(message) => {
                assert!(message.contains("context pipeline stage 'graph_memory' failed"));
                assert!(message.contains("fixture validation failed"));
            }
            other => panic!("expected validation error with stage name, got {other:?}"),
        }
    }

    struct FailingStage;

    #[async_trait]
    impl ContextProcessor for FailingStage {
        fn name(&self) -> &str {
            "graph_memory"
        }

        fn stage(&self) -> u8 {
            7
        }

        async fn process(&self, _ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
            Err(MoaError::ValidationError(
                "fixture validation failed".to_string(),
            ))
        }
    }

    #[test]
    fn cache_prefix_ratio_includes_tool_tokens() {
        // Pins: stable-prefix ratio counts deterministic tool schemas and leading system messages only.
        let session = SessionMeta {
            id: SessionId::new(),
            channel: Channel::Chat,
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

    /// Test stage that appends a system message and records its fetch and apply
    /// order, with an optional fetch delay to force out-of-order fetch completion.
    struct OrderedParallelStage {
        stage: u8,
        name: &'static str,
        parallel: bool,
        fetch_delay: Duration,
        fetch_order: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl ContextProcessor for OrderedParallelStage {
        fn name(&self) -> &str {
            self.name
        }

        fn stage(&self) -> u8 {
            self.stage
        }

        fn parallelizable(&self) -> bool {
            self.parallel
        }

        async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
            match self.fetch(ctx).await? {
                Some(apply) => apply(ctx),
                None => Ok(ProcessorOutput::default()),
            }
        }

        async fn fetch(&self, _ctx: &WorkingContext) -> Result<Option<StageApply>> {
            tokio::time::sleep(self.fetch_delay).await;
            self.fetch_order
                .lock()
                .expect("fetch order lock")
                .push(self.name);
            let name = self.name;
            let apply: StageApply = Box::new(move |ctx: &mut WorkingContext| {
                ctx.append_system(name);
                let mut order = ctx
                    .metadata()
                    .get("apply_order")
                    .cloned()
                    .unwrap_or_else(|| json!([]));
                order
                    .as_array_mut()
                    .expect("apply_order metadata is an array")
                    .push(json!(name));
                ctx.insert_metadata("apply_order", order);
                Ok(ProcessorOutput {
                    tokens_added: estimate_text_tokens(name),
                    ..ProcessorOutput::default()
                })
            });
            Ok(Some(apply))
        }
    }

    fn ordered_session() -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            channel: Channel::Chat,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        }
    }

    fn message_contents(ctx: &WorkingContext) -> Vec<String> {
        ctx.messages
            .iter()
            .map(|message| message.content.clone())
            .collect()
    }

    fn report_names(reports: &[PipelineStageReport]) -> Vec<String> {
        reports.iter().map(|report| report.name.clone()).collect()
    }

    #[tokio::test]
    async fn parallel_group_matches_sequential_final_context() {
        // Pins: running a run of parallelizable stages through the concurrent
        // fetch/apply runner produces the same final WorkingContext (messages in
        // order, token count, apply-order metadata) and the same ordered stage
        // reports as running the identical stages sequentially via `process`.
        let build = |parallel: bool| {
            ContextPipeline::new(vec![
                Box::new(OrderedParallelStage {
                    stage: 1,
                    name: "alpha",
                    parallel,
                    fetch_delay: Duration::from_millis(40),
                    fetch_order: Arc::new(Mutex::new(Vec::new())),
                }) as Box<dyn ContextProcessor>,
                Box::new(OrderedParallelStage {
                    stage: 2,
                    name: "bravo",
                    parallel,
                    fetch_delay: Duration::from_millis(0),
                    fetch_order: Arc::new(Mutex::new(Vec::new())),
                }),
                Box::new(OrderedParallelStage {
                    stage: 3,
                    name: "charlie",
                    parallel,
                    fetch_delay: Duration::from_millis(0),
                    fetch_order: Arc::new(Mutex::new(Vec::new())),
                }),
            ])
        };
        let session = ordered_session();

        let mut sequential_ctx = WorkingContext::new(&session, capabilities());
        let sequential_reports = build(false)
            .run(&mut sequential_ctx)
            .await
            .expect("sequential pipeline should run");

        let mut parallel_ctx = WorkingContext::new(&session, capabilities());
        let parallel_reports = build(true)
            .run(&mut parallel_ctx)
            .await
            .expect("parallel pipeline should run");

        assert_eq!(
            message_contents(&sequential_ctx),
            message_contents(&parallel_ctx),
        );
        assert_eq!(
            message_contents(&parallel_ctx),
            vec![
                "alpha".to_string(),
                "bravo".to_string(),
                "charlie".to_string()
            ],
        );
        assert_eq!(
            parallel_ctx.metadata().get("apply_order"),
            Some(&json!(["alpha", "bravo", "charlie"])),
        );
        assert_eq!(
            sequential_ctx.metadata().get("apply_order"),
            parallel_ctx.metadata().get("apply_order"),
        );
        assert_eq!(sequential_ctx.token_count, parallel_ctx.token_count);
        assert_eq!(
            report_names(&sequential_reports),
            report_names(&parallel_reports),
        );
        assert_eq!(
            report_names(&parallel_reports),
            vec![
                "alpha".to_string(),
                "bravo".to_string(),
                "charlie".to_string()
            ],
        );
    }

    #[tokio::test]
    async fn parallel_group_applies_in_stage_order_despite_fetch_completion_order() {
        // Pins: the concurrent group overlaps fetches (a zero-delay stage's fetch
        // finishes before a slow earlier stage's) yet applies strictly in stage
        // order, so message layout never depends on which fetch finished first.
        let fetch_order = Arc::new(Mutex::new(Vec::new()));
        let pipeline = ContextPipeline::new(vec![
            Box::new(OrderedParallelStage {
                stage: 1,
                name: "slow",
                parallel: true,
                fetch_delay: Duration::from_millis(50),
                fetch_order: fetch_order.clone(),
            }) as Box<dyn ContextProcessor>,
            Box::new(OrderedParallelStage {
                stage: 2,
                name: "fast",
                parallel: true,
                fetch_delay: Duration::from_millis(0),
                fetch_order: fetch_order.clone(),
            }),
        ]);
        let session = ordered_session();
        let mut ctx = WorkingContext::new(&session, capabilities());

        pipeline.run(&mut ctx).await.expect("pipeline should run");

        assert_eq!(
            *fetch_order.lock().expect("fetch order lock"),
            vec!["fast", "slow"],
            "the fast stage's fetch must finish first, proving the fetches overlapped",
        );
        assert_eq!(
            ctx.metadata().get("apply_order"),
            Some(&json!(["slow", "fast"])),
            "apply order must follow stage order, not fetch-completion order",
        );
        assert_eq!(
            message_contents(&ctx),
            vec!["slow".to_string(), "fast".to_string()]
        );
    }

    #[tokio::test]
    async fn mixed_sequential_and_parallel_stages_preserve_declared_order() {
        // Pins: a parallelizable run bounded by sequential stages still applies
        // every stage in full declared order across the group boundaries.
        let fetch_order = Arc::new(Mutex::new(Vec::new()));
        let stage = |stage: u8, name: &'static str, parallel: bool| {
            Box::new(OrderedParallelStage {
                stage,
                name,
                parallel,
                fetch_delay: Duration::from_millis(0),
                fetch_order: fetch_order.clone(),
            }) as Box<dyn ContextProcessor>
        };
        let pipeline = ContextPipeline::new(vec![
            stage(1, "lead", false),
            stage(2, "mid_a", true),
            stage(3, "mid_b", true),
            stage(4, "tail", false),
        ]);
        let session = ordered_session();
        let mut ctx = WorkingContext::new(&session, capabilities());

        pipeline.run(&mut ctx).await.expect("pipeline should run");

        assert_eq!(
            ctx.metadata().get("apply_order"),
            Some(&json!(["lead", "mid_a", "mid_b", "tail"])),
        );
        assert_eq!(
            message_contents(&ctx),
            vec![
                "lead".to_string(),
                "mid_a".to_string(),
                "mid_b".to_string(),
                "tail".to_string(),
            ],
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
