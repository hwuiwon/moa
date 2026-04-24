//! Stage 5: rewrites the current user query before memory retrieval.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use moa_core::{
    CompletionRequest, ContextMessage, ContextProcessor, Event, EventRange, EventRecord,
    JsonResponseFormat, LLMProvider, MessageRole, MoaError, ModelId, ProcessorOutput,
    QueryRewriteConfig, QueryRewriteResult, Result, RewriteSource, SessionStore, WorkingContext,
};
use serde::Deserialize;
use serde_json::{Value, json};

const METADATA_KEY: &str = "query_rewrite";
const RECENT_HISTORY_EVENT_LIMIT: usize = 32;
const MAX_HISTORY_MESSAGES: usize = 10;
const MAX_PROMPT_MESSAGE_CHARS: usize = 1_000;
const REWRITER_OUTPUT_TOKENS: usize = 768;

/// Query-rewriting context processor.
pub struct QueryRewriter {
    config: QueryRewriteConfig,
    llm: Arc<dyn LLMProvider>,
    session_store: Option<Arc<dyn SessionStore>>,
    circuit_breaker: CircuitBreaker,
}

impl QueryRewriter {
    /// Creates a query rewriter backed by the provided LLM.
    pub fn new(config: QueryRewriteConfig, llm: Arc<dyn LLMProvider>) -> Self {
        let circuit_breaker = CircuitBreaker::new(
            config.circuit_breaker_threshold,
            config.circuit_breaker_window_secs,
            config.circuit_breaker_cooldown_secs,
        );
        Self {
            config,
            llm,
            session_store: None,
            circuit_breaker,
        }
    }

    /// Configures the rewriter to load recent user history directly from the session log.
    pub fn with_session_store(mut self, session_store: Arc<dyn SessionStore>) -> Self {
        self.session_store = Some(session_store);
        self
    }

    fn should_skip(&self, input: &RewriteInput) -> bool {
        if !self.config.enabled || self.circuit_breaker.is_open() {
            return true;
        }

        if input.query.trim().is_empty() {
            return true;
        }

        if starts_with_tool_like_verb(&input.query) {
            return true;
        }

        self.config.skip_single_turn
            && input.user_message_count <= 1
            && approximate_query_tokens(&input.query) < self.config.min_query_tokens
    }

    async fn rewrite(
        &self,
        input: &RewriteInput,
        ctx: &WorkingContext,
    ) -> Result<QueryRewriteResult> {
        let prompt = build_rewriter_prompt(input, ctx);
        let mut request = CompletionRequest::new(prompt);
        request.model = self
            .config
            .model
            .as_ref()
            .map(|model| ModelId::new(model.clone()));
        request.max_output_tokens = Some(REWRITER_OUTPUT_TOKENS);
        request.temperature = Some(0.0);
        request.response_format = Some(query_rewrite_response_format());
        request.metadata =
            HashMap::from([("moa.pipeline.stage".to_string(), json!("query_rewrite"))]);

        let stream = self.llm.complete(request).await?;
        let response = stream.collect().await?;
        let parsed = parse_rewrite_response(&response.text)?;
        Ok(validate_rewrite_result(parsed, input, ctx))
    }

    async fn load_input(&self, ctx: &WorkingContext) -> Result<Option<RewriteInput>> {
        if let Some(input) = input_from_context_messages(&ctx.messages) {
            return Ok(Some(input));
        }

        let Some(session_store) = &self.session_store else {
            return Ok(None);
        };

        let records = session_store
            .get_events(
                ctx.session_id,
                EventRange::recent(RECENT_HISTORY_EVENT_LIMIT),
            )
            .await?;
        Ok(input_from_event_records(&records))
    }
}

#[async_trait]
impl ContextProcessor for QueryRewriter {
    fn name(&self) -> &str {
        "query_rewrite"
    }

    fn stage(&self) -> u8 {
        5
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let input = match self.load_input(ctx).await {
            Ok(Some(input)) => input,
            Ok(None) => RewriteInput::empty(),
            Err(error) => {
                self.circuit_breaker.record_failure();
                tracing::warn!(
                    error = %error,
                    "query rewriter failed to load history, falling back"
                );
                store_rewrite_result(ctx, QueryRewriteResult::passthrough(""))?;
                return Ok(ProcessorOutput::default());
            }
        };

        if self.should_skip(&input) {
            store_rewrite_result(ctx, QueryRewriteResult::passthrough(input.query))?;
            return Ok(ProcessorOutput::default());
        }

        let timeout = Duration::from_millis(self.config.timeout_ms);
        match tokio::time::timeout(timeout, self.rewrite(&input, ctx)).await {
            Ok(Ok(result)) => {
                self.circuit_breaker.record_success();
                let metadata = HashMap::from([
                    ("rewrite_source".to_string(), json!("rewritten")),
                    ("intent".to_string(), json!(result.intent.clone())),
                ]);
                store_rewrite_result(ctx, result)?;
                Ok(ProcessorOutput {
                    metadata,
                    ..ProcessorOutput::default()
                })
            }
            Ok(Err(error)) => {
                self.circuit_breaker.record_failure();
                tracing::warn!(
                    error = %error,
                    "query rewriter failed, falling back"
                );
                store_rewrite_result(ctx, QueryRewriteResult::passthrough(input.query))?;
                Ok(ProcessorOutput::default())
            }
            Err(_) => {
                self.circuit_breaker.record_failure();
                tracing::warn!(
                    timeout_ms = self.config.timeout_ms,
                    "query rewriter timed out, falling back"
                );
                store_rewrite_result(ctx, QueryRewriteResult::passthrough(input.query))?;
                Ok(ProcessorOutput::default())
            }
        }
    }
}

/// Sliding-window circuit breaker for fail-open query rewriting.
pub struct CircuitBreaker {
    failures: AtomicU32,
    successes: AtomicU32,
    last_reset: AtomicU64,
    tripped_until: AtomicU64,
    threshold: f64,
    window_secs: u64,
    cooldown_secs: u64,
}

impl CircuitBreaker {
    /// Creates a circuit breaker with an error-rate threshold, window, and cooldown.
    #[must_use]
    pub fn new(threshold: f64, window_secs: u64, cooldown_secs: u64) -> Self {
        Self {
            failures: AtomicU32::new(0),
            successes: AtomicU32::new(0),
            last_reset: AtomicU64::new(now_epoch_millis()),
            tripped_until: AtomicU64::new(0),
            threshold,
            window_secs,
            cooldown_secs,
        }
    }

    /// Returns whether the circuit is currently open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        let now = now_epoch_millis();
        let tripped_until = self.tripped_until.load(Ordering::Relaxed);
        if tripped_until > now {
            return true;
        }

        if tripped_until != 0 {
            self.tripped_until.store(0, Ordering::Relaxed);
            self.reset_window(now);
        } else {
            self.rotate_window_if_needed(now);
        }

        false
    }

    /// Records a successful rewriter call.
    pub fn record_success(&self) {
        self.rotate_window_if_needed(now_epoch_millis());
        self.successes.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a failed rewriter call and trips the circuit when the error rate is too high.
    pub fn record_failure(&self) {
        let now = now_epoch_millis();
        self.rotate_window_if_needed(now);
        let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        let successes = self.successes.load(Ordering::Relaxed);
        let total = failures + successes;
        if total == 0 {
            return;
        }

        let failure_rate = f64::from(failures) / f64::from(total);
        if failure_rate > self.threshold {
            let cooldown_ms = self.cooldown_secs.saturating_mul(1_000);
            self.tripped_until
                .store(now.saturating_add(cooldown_ms), Ordering::Relaxed);
        }
    }

    fn rotate_window_if_needed(&self, now: u64) {
        let window_ms = self.window_secs.saturating_mul(1_000);
        if window_ms == 0 {
            self.reset_window(now);
            return;
        }

        let last_reset = self.last_reset.load(Ordering::Relaxed);
        if now.saturating_sub(last_reset) < window_ms {
            return;
        }

        self.reset_window(now);
    }

    fn reset_window(&self, now: u64) {
        self.failures.store(0, Ordering::Relaxed);
        self.successes.store(0, Ordering::Relaxed);
        self.last_reset.store(now, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, PartialEq)]
struct RewriteInput {
    query: String,
    history: Vec<ContextMessage>,
    user_message_count: usize,
}

impl RewriteInput {
    fn empty() -> Self {
        Self {
            query: String::new(),
            history: Vec::new(),
            user_message_count: 0,
        }
    }
}

fn store_rewrite_result(ctx: &mut WorkingContext, result: QueryRewriteResult) -> Result<()> {
    ctx.insert_metadata(METADATA_KEY, serde_json::to_value(result)?);
    Ok(())
}

fn query_rewrite_response_format() -> JsonResponseFormat {
    JsonResponseFormat::strict_json_schema(
        "query_rewrite_result",
        "Self-contained query rewrite, intent classification, and retrieval hints.",
        QueryRewriteResult::response_schema(),
    )
}

fn input_from_context_messages(messages: &[ContextMessage]) -> Option<RewriteInput> {
    let conversation = messages
        .iter()
        .filter(|message| matches!(message.role, MessageRole::User | MessageRole::Assistant))
        .cloned()
        .collect::<Vec<_>>();
    input_from_conversation(conversation)
}

fn input_from_event_records(records: &[EventRecord]) -> Option<RewriteInput> {
    let conversation = records
        .iter()
        .filter_map(event_to_rewrite_message)
        .collect::<Vec<_>>();
    input_from_conversation(conversation)
}

fn event_to_rewrite_message(record: &EventRecord) -> Option<ContextMessage> {
    match &record.event {
        Event::UserMessage { text, .. } | Event::QueuedMessage { text, .. } => {
            Some(ContextMessage::user(text.clone()))
        }
        Event::BrainResponse { text, .. } => Some(ContextMessage::assistant(text.clone())),
        _ => None,
    }
}

fn input_from_conversation(conversation: Vec<ContextMessage>) -> Option<RewriteInput> {
    let last_user_index = conversation
        .iter()
        .rposition(|message| message.role == MessageRole::User)?;
    let query = conversation.get(last_user_index)?.content.clone();
    let user_message_count = conversation
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .count();
    let start = last_user_index.saturating_sub(MAX_HISTORY_MESSAGES);
    let history = conversation[start..last_user_index].to_vec();

    Some(RewriteInput {
        query,
        history,
        user_message_count,
    })
}

fn build_rewriter_prompt(input: &RewriteInput, ctx: &WorkingContext) -> String {
    let history = format_history(&input.history);
    let tools = available_tool_names(ctx).join(", ");
    let skills = available_skill_lines(ctx).join("\n");

    format!(
        "You are a query rewriter for an AI agent system. Rewrite the user's query\n\
         into a self-contained, unambiguous request. Resolve pronouns and references\n\
         using the conversation history.\n\n\
         Rules:\n\
         - Do NOT invent information not present in the conversation history\n\
         - Do NOT add entities, file paths, or technical details not mentioned\n\
         - DO resolve \"that\", \"it\", \"the bug\", etc. to their concrete referents\n\
         - DO decompose compound requests into sub_queries\n\
         - Determine if this message starts a NEW task or continues the current one\n\
         - A new task means the user is asking about something unrelated to the current work\n\
         - Set is_new_task=true only when the topic genuinely shifts, not for follow-up questions\n\
         - If is_new_task=true, provide a short task_summary in one sentence\n\
         - Treat coreferences like \"that file\", \"the error above\", and \"try again\" as continuations\n\
         - Respond ONLY with valid JSON matching the schema below. No preamble.\n\n\
         Schema: {{\"rewritten_query\": string, \"intent\": string, \"sub_queries\": [string],\n\
         \"suggested_tools\": [string], \"needs_clarification\": bool,\n\
         \"clarification_question\": string|null, \"is_new_task\": bool,\n\
         \"task_summary\": string|null}}\n\
         intent must be one of: coding, research, file_operation, system_admin,\n\
         creative, question, conversation, unknown.\n\n\
         Available tools: {tools}\n\n\
         Available skills:\n{skills}\n\n\
         Conversation history (last 5 turns):\n{history}\n\n\
         Current query:\n{}",
        input.query
    )
}

fn format_history(history: &[ContextMessage]) -> String {
    if history.is_empty() {
        return "(none)".to_string();
    }

    history
        .iter()
        .map(|message| {
            let role = match message.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
                MessageRole::Tool => "tool",
            };
            format!(
                "{role}: {}",
                truncate_for_prompt(message.content.trim(), MAX_PROMPT_MESSAGE_CHARS)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_for_prompt(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let mut truncated = text.chars().take(max_chars).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn available_tool_names(ctx: &WorkingContext) -> Vec<String> {
    ctx.tools()
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

fn available_skill_lines(ctx: &WorkingContext) -> Vec<String> {
    ctx.messages
        .iter()
        .filter(|message| message.role == MessageRole::System)
        .flat_map(|message| message.content.lines())
        .map(str::trim)
        .filter(|line| line.starts_with("- "))
        .take(50)
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_rewrite_response(text: &str) -> Result<QueryRewriteResult> {
    let result = serde_json::from_str::<RawQueryRewriteResult>(text.trim())?.into_result();
    if result.rewritten_query.trim().is_empty() {
        return Err(MoaError::ValidationError(
            "query rewriter returned an empty rewritten_query".to_string(),
        ));
    }
    Ok(result)
}

#[derive(Debug, Deserialize)]
struct RawQueryRewriteResult {
    rewritten_query: String,
    intent: moa_core::QueryIntent,
    sub_queries: Vec<String>,
    suggested_tools: Vec<String>,
    needs_clarification: bool,
    clarification_question: Option<String>,
    #[serde(default)]
    is_new_task: bool,
    #[serde(default)]
    task_summary: Option<String>,
}

impl RawQueryRewriteResult {
    fn into_result(self) -> QueryRewriteResult {
        QueryRewriteResult {
            rewritten_query: self.rewritten_query,
            intent: self.intent,
            sub_queries: self.sub_queries,
            suggested_tools: self.suggested_tools,
            needs_clarification: self.needs_clarification,
            clarification_question: self.clarification_question,
            is_new_task: self.is_new_task,
            task_summary: self.task_summary,
            source: RewriteSource::Rewritten,
        }
    }
}

fn validate_rewrite_result(
    mut result: QueryRewriteResult,
    input: &RewriteInput,
    ctx: &WorkingContext,
) -> QueryRewriteResult {
    let allowed_terms = allowed_terms(input, ctx);
    result.rewritten_query =
        strip_unsupported_entity_tokens(&result.rewritten_query, &allowed_terms);
    result.sub_queries = result
        .sub_queries
        .into_iter()
        .map(|query| strip_unsupported_entity_tokens(&query, &allowed_terms))
        .filter(|query| !query.trim().is_empty())
        .collect();
    result.clarification_question = result
        .clarification_question
        .map(|question| strip_unsupported_entity_tokens(&question, &allowed_terms))
        .filter(|question| !question.trim().is_empty());
    result.task_summary = result
        .task_summary
        .map(|summary| strip_unsupported_entity_tokens(&summary, &allowed_terms))
        .filter(|summary| !summary.trim().is_empty());
    result.suggested_tools = filter_suggested_tools(result.suggested_tools, ctx);
    result.source = RewriteSource::Rewritten;

    if result.rewritten_query.trim().is_empty() {
        QueryRewriteResult::passthrough(input.query.clone())
    } else {
        result
    }
}

fn allowed_terms(input: &RewriteInput, ctx: &WorkingContext) -> HashSet<String> {
    let mut terms = HashSet::new();
    for message in &input.history {
        terms.extend(entity_terms(&message.content));
    }
    terms.extend(entity_terms(&input.query));
    for tool in available_tool_names(ctx) {
        terms.extend(entity_terms(&tool));
    }
    for line in available_skill_lines(ctx) {
        terms.extend(entity_terms(&line));
    }
    terms
}

fn filter_suggested_tools(suggested_tools: Vec<String>, ctx: &WorkingContext) -> Vec<String> {
    let available = available_tool_names(ctx)
        .into_iter()
        .collect::<HashSet<_>>();
    if available.is_empty() {
        return suggested_tools;
    }

    suggested_tools
        .into_iter()
        .filter(|tool| available.contains(tool))
        .collect()
}

fn strip_unsupported_entity_tokens(text: &str, allowed_terms: &HashSet<String>) -> String {
    let mut sanitized = Vec::new();
    for raw in text.split_whitespace() {
        let term = normalize_entity_token(raw);
        if term.is_empty()
            || !is_entity_like(&term)
            || is_rewrite_function_word(&term)
            || allowed_terms.contains(&term)
        {
            sanitized.push(raw);
        }
    }

    cleanup_stripped_text(&sanitized.join(" "))
}

fn cleanup_stripped_text(text: &str) -> String {
    let mut words = text
        .split_whitespace()
        .map(|word| word.trim_matches([',', ';', ':']))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    while words.last().is_some_and(|word| {
        matches!(
            word.to_ascii_lowercase().as_str(),
            "and" | "or" | "in" | "for" | "with" | "to"
        )
    }) {
        words.pop();
    }
    words.join(" ")
}

fn entity_terms(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(normalize_entity_token)
        .filter(|token| is_entity_like(token))
        .collect()
}

fn normalize_entity_token(token: &str) -> String {
    token
        .trim_matches(|character: char| {
            !character.is_alphanumeric()
                && !matches!(character, '_' | '-' | '/' | '.' | ':' | '@' | '#')
        })
        .trim_end_matches(['.', ',', ';', ':', '!', '?'])
        .to_ascii_lowercase()
}

fn is_entity_like(token: &str) -> bool {
    token.len() >= 3 && !is_rewrite_function_word(token)
}

fn is_rewrite_function_word(token: &str) -> bool {
    const FUNCTION_WORDS: &[&str] = &[
        "about",
        "add",
        "after",
        "again",
        "all",
        "also",
        "and",
        "answer",
        "around",
        "before",
        "build",
        "can",
        "check",
        "clarify",
        "code",
        "coverage",
        "covering",
        "create",
        "debug",
        "delete",
        "describe",
        "diagnose",
        "edit",
        "explain",
        "file",
        "find",
        "fix",
        "for",
        "from",
        "help",
        "how",
        "implement",
        "into",
        "investigate",
        "issue",
        "make",
        "move",
        "need",
        "please",
        "question",
        "read",
        "remove",
        "request",
        "resolve",
        "review",
        "run",
        "search",
        "show",
        "summarize",
        "task",
        "tell",
        "that",
        "the",
        "then",
        "this",
        "to",
        "update",
        "use",
        "using",
        "what",
        "when",
        "where",
        "which",
        "with",
        "write",
    ];

    FUNCTION_WORDS.contains(&token)
}

fn starts_with_tool_like_verb(query: &str) -> bool {
    let Some(first_token) = query
        .split_whitespace()
        .next()
        .map(normalize_entity_token)
        .filter(|token| !token.is_empty())
    else {
        return false;
    };

    matches!(
        first_token.as_str(),
        "read" | "write" | "search" | "run" | "deploy"
    )
}

fn approximate_query_tokens(query: &str) -> usize {
    query.split_whitespace().count()
}

fn now_epoch_millis() -> u64 {
    let Ok(duration) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return 0;
    };
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use moa_core::{
        CompletionResponse, CompletionStream, ModelCapabilities, Platform, SessionId, SessionMeta,
        StopReason, TokenPricing, TokenUsage, ToolCallFormat, UserId, WorkspaceId,
    };

    use super::*;

    #[derive(Clone)]
    struct MockProvider {
        response: Arc<std::sync::Mutex<String>>,
        delay: Duration,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl LLMProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn capabilities(&self) -> ModelCapabilities {
            capabilities()
        }

        async fn complete(&self, _request: CompletionRequest) -> Result<CompletionStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            let text = self
                .response
                .lock()
                .expect("mock response lock should not be poisoned")
                .clone();
            Ok(CompletionStream::from_response(CompletionResponse {
                text: text.clone(),
                content: vec![moa_core::CompletionContent::Text(text)],
                stop_reason: StopReason::EndTurn,
                model: ModelId::new("mock"),
                usage: TokenUsage::default(),
                duration_ms: 1,
                thought_signature: None,
            }))
        }
    }

    fn capabilities() -> ModelCapabilities {
        ModelCapabilities {
            model_id: ModelId::new("mock"),
            context_window: 32_000,
            max_output: 1_024,
            supports_tools: true,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: TokenPricing {
                input_per_mtok: 1.0,
                output_per_mtok: 1.0,
                cached_input_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }

    fn context_with_messages(messages: Vec<ContextMessage>) -> WorkingContext {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Desktop,
            model: ModelId::new("mock"),
            ..SessionMeta::default()
        };
        let mut ctx = WorkingContext::new(&session, capabilities());
        for message in messages {
            ctx.append_message(message);
        }
        ctx
    }

    fn response_json(rewritten_query: &str, sub_queries: Vec<&str>) -> String {
        json!({
            "rewritten_query": rewritten_query,
            "intent": "coding",
            "sub_queries": sub_queries,
            "suggested_tools": [],
            "needs_clarification": false,
            "clarification_question": null,
            "is_new_task": false,
            "task_summary": null,
        })
        .to_string()
    }

    fn rewriter_with_response(response: String) -> (QueryRewriter, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = MockProvider {
            response: Arc::new(std::sync::Mutex::new(response)),
            delay: Duration::ZERO,
            calls: calls.clone(),
        };
        (
            QueryRewriter::new(QueryRewriteConfig::default(), Arc::new(provider)),
            calls,
        )
    }

    fn metadata_result(ctx: &WorkingContext) -> QueryRewriteResult {
        serde_json::from_value(
            ctx.metadata()
                .get(METADATA_KEY)
                .expect("rewrite metadata should exist")
                .clone(),
        )
        .expect("rewrite metadata should deserialize")
    }

    #[tokio::test]
    async fn skips_single_turn_short_query() {
        let (rewriter, calls) = rewriter_with_response(response_json("hello there", Vec::new()));
        let mut ctx = context_with_messages(vec![ContextMessage::user("hello")]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("query rewrite should process");

        let result = metadata_result(&ctx);
        assert_eq!(result.source, RewriteSource::Passthrough);
        assert_eq!(result.rewritten_query, "hello");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rewrites_multiturn_coreference() {
        let (rewriter, calls) = rewriter_with_response(response_json(
            "Fix the OAuth refresh token race condition in auth/refresh.rs",
            Vec::new(),
        ));
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("The OAuth refresh token race condition is in auth/refresh.rs"),
            ContextMessage::assistant("I found it."),
            ContextMessage::user("fix that"),
        ]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("query rewrite should process");

        let result = metadata_result(&ctx);
        assert_eq!(result.source, RewriteSource::Rewritten);
        assert_eq!(
            result.rewritten_query,
            "Fix the OAuth refresh token race condition in auth/refresh.rs"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn default_timeout_allows_segment_transition_rewrite_latency() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = MockProvider {
            response: Arc::new(std::sync::Mutex::new(
                json!({
                    "rewritten_query": "Write a five-word project status headline about database migrations.",
                    "intent": "creative",
                    "sub_queries": [],
                    "suggested_tools": [],
                    "needs_clarification": false,
                    "clarification_question": null,
                    "is_new_task": true,
                    "task_summary": "Write a short project status headline about database migrations.",
                })
                .to_string(),
            )),
            delay: Duration::from_millis(600),
            calls: calls.clone(),
        };
        let rewriter = QueryRewriter::new(QueryRewriteConfig::default(), Arc::new(provider));
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("What is 2 + 2? Answer with only the number."),
            ContextMessage::assistant("4"),
            ContextMessage::user(
                "Now switch tasks: write a five-word project status headline about database migrations.",
            ),
        ]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("default timeout should allow live-like rewrite latency");

        let result = metadata_result(&ctx);
        assert_eq!(result.source, RewriteSource::Rewritten);
        assert!(result.is_new_task);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn preserves_compound_sub_queries() {
        let (rewriter, _) = rewriter_with_response(response_json(
            "Review auth/refresh.rs and add tests",
            vec!["Review auth/refresh.rs", "add tests"],
        ));
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("auth/refresh.rs handles OAuth refresh tokens"),
            ContextMessage::assistant("Noted."),
            ContextMessage::user("review that and add tests"),
        ]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("query rewrite should process");

        let result = metadata_result(&ctx);
        assert_eq!(
            result.sub_queries,
            vec![
                "Review auth/refresh.rs".to_string(),
                "add tests".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn timeout_falls_back_to_passthrough() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = MockProvider {
            response: Arc::new(std::sync::Mutex::new(response_json("unused", Vec::new()))),
            delay: Duration::from_millis(50),
            calls: calls.clone(),
        };
        let config = QueryRewriteConfig {
            timeout_ms: 1,
            ..QueryRewriteConfig::default()
        };
        let rewriter = QueryRewriter::new(config, Arc::new(provider));
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("OAuth refresh token race condition"),
            ContextMessage::assistant("I found it."),
            ContextMessage::user("fix that"),
        ]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("timeout should fail open");

        let result = metadata_result(&ctx);
        assert_eq!(result.source, RewriteSource::Passthrough);
        assert_eq!(result.rewritten_query, "fix that");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn circuit_breaker_trips_after_failures() {
        let (rewriter, calls) = rewriter_with_response("not json".to_string());
        let mut first = context_with_messages(vec![
            ContextMessage::user("OAuth refresh token race condition"),
            ContextMessage::assistant("I found it."),
            ContextMessage::user("fix that"),
        ]);
        let mut second = first.clone();

        rewriter
            .process(&mut first)
            .await
            .expect("first failure should fail open");
        rewriter
            .process(&mut second)
            .await
            .expect("open circuit should skip");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(metadata_result(&second).source, RewriteSource::Passthrough);
    }

    #[tokio::test]
    async fn invalid_intent_falls_back_to_passthrough() {
        let invalid_response = json!({
            "rewritten_query": "Fix the OAuth refresh token race condition in auth/refresh.rs",
            "intent": "software engineering task",
            "sub_queries": [],
            "suggested_tools": [],
            "needs_clarification": false,
            "clarification_question": null,
            "is_new_task": false,
            "task_summary": null,
        })
        .to_string();
        let (rewriter, calls) = rewriter_with_response(invalid_response);
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("The OAuth refresh token race condition is in auth/refresh.rs"),
            ContextMessage::assistant("I found it."),
            ContextMessage::user("fix that"),
        ]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("invalid intent should fail open");

        let result = metadata_result(&ctx);
        assert_eq!(result.source, RewriteSource::Passthrough);
        assert_eq!(result.rewritten_query, "fix that");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn circuit_breaker_resets_after_cooldown() {
        let breaker = CircuitBreaker::new(0.05, 60, 1);
        breaker.record_failure();
        assert!(breaker.is_open());

        std::thread::sleep(Duration::from_millis(1_100));

        assert!(!breaker.is_open());
    }

    #[tokio::test]
    async fn validation_strips_entities_not_present_in_history() {
        let (rewriter, _) = rewriter_with_response(response_json(
            "Fix the OAuth refresh token race condition in auth/refresh.rs and Kubernetes deployment",
            Vec::new(),
        ));
        let mut ctx = context_with_messages(vec![
            ContextMessage::user("The OAuth refresh token race condition is in auth/refresh.rs"),
            ContextMessage::assistant("I found it."),
            ContextMessage::user("fix that"),
        ]);

        rewriter
            .process(&mut ctx)
            .await
            .expect("query rewrite should process");

        let result = metadata_result(&ctx);
        assert!(!result.rewritten_query.contains("Kubernetes"));
        assert!(!result.rewritten_query.contains("deployment"));
        assert_eq!(
            result.rewritten_query,
            "Fix the OAuth refresh token race condition in auth/refresh.rs"
        );
    }
}
