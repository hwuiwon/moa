//! Context compilation messages and working state.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    CacheBreakpoint, CacheTtl, CompletionRequest, ModelCapabilities, SessionId, SessionMeta,
    ToolContent, ToolInvocation, UserId, WorkspaceId,
};

/// Role of a context message passed to the LLM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// System prompt content.
    System,
    /// User-authored content.
    User,
    /// Assistant-authored content.
    Assistant,
    /// Tool result content.
    Tool,
}

/// Single compiled context message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextMessage {
    /// Message role.
    pub role: MessageRole,
    /// Text content.
    pub content: String,
    /// Provider-specific thought signature that must be replayed with this message when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    /// Optional attached tool schema payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Value>,
    /// Structured content blocks for providers that support them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_blocks: Option<Vec<ToolContent>>,
    /// Structured assistant tool call for providers that support native tool-use history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_invocation: Option<ToolInvocation>,
    /// Provider-specific tool use identifier for tool result messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
}

impl ContextMessage {
    /// Creates a system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            thought_signature: None,
            tools: None,
            content_blocks: None,
            tool_invocation: None,
            tool_use_id: None,
        }
    }

    /// Creates a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            thought_signature: None,
            tools: None,
            content_blocks: None,
            tool_invocation: None,
            tool_use_id: None,
        }
    }

    /// Creates an assistant message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::assistant_with_thought_signature(content, None::<String>)
    }

    /// Creates an assistant message with an optional provider-specific thought signature.
    pub fn assistant_with_thought_signature(
        content: impl Into<String>,
        thought_signature: Option<impl Into<String>>,
    ) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            thought_signature: thought_signature.map(Into::into),
            tools: None,
            content_blocks: None,
            tool_invocation: None,
            tool_use_id: None,
        }
    }

    /// Creates a tool message.
    pub fn tool(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            thought_signature: None,
            tools: None,
            content_blocks: None,
            tool_invocation: None,
            tool_use_id: None,
        }
    }

    /// Creates an assistant tool-call message with both text fallback and structured invocation.
    pub fn assistant_tool_call(invocation: ToolInvocation, content: impl Into<String>) -> Self {
        Self::assistant_tool_call_with_thought_signature(invocation, content, None::<String>)
    }

    /// Creates an assistant tool-call message with optional provider-specific replay metadata.
    pub fn assistant_tool_call_with_thought_signature(
        invocation: ToolInvocation,
        content: impl Into<String>,
        thought_signature: Option<impl Into<String>>,
    ) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            thought_signature: thought_signature.map(Into::into),
            tools: None,
            content_blocks: None,
            tool_invocation: Some(invocation),
            tool_use_id: None,
        }
    }

    /// Creates a tool result message with both text fallback and structured blocks.
    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        content_blocks: Option<Vec<ToolContent>>,
    ) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            thought_signature: None,
            tools: None,
            content_blocks,
            tool_invocation: None,
            tool_use_id: Some(tool_use_id.into()),
        }
    }
}

/// Mutable context under compilation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkingContext {
    /// Ordered context messages.
    pub messages: Vec<ContextMessage>,
    /// Current token count.
    pub token_count: usize,
    /// Maximum token budget.
    pub token_budget: usize,
    /// Active model capabilities.
    pub model_capabilities: ModelCapabilities,
    /// Session identifier.
    pub session_id: SessionId,
    /// User identifier.
    pub user_id: UserId,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// Cache breakpoint indexes within `messages`.
    pub cache_breakpoints: Vec<usize>,
    /// Detailed cache controls emitted to providers that support TTL-aware prompt caching.
    pub cache_controls: Vec<CacheBreakpoint>,
    /// Active tool schemas compiled for the request.
    tool_schemas: Vec<Value>,
    /// Arbitrary processor metadata.
    metadata: HashMap<String, Value>,
}

impl WorkingContext {
    /// Creates an empty working context for a session.
    pub fn new(session: &SessionMeta, model_capabilities: ModelCapabilities) -> Self {
        Self {
            messages: Vec::new(),
            token_count: 0,
            token_budget: model_capabilities
                .context_window
                .saturating_sub(model_capabilities.max_output),
            model_capabilities,
            session_id: session.id,
            user_id: session.user_id.clone(),
            workspace_id: session.workspace_id.clone(),
            cache_breakpoints: Vec::new(),
            cache_controls: Vec::new(),
            tool_schemas: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Appends a system message and updates the approximate token count.
    pub fn append_system(&mut self, content: impl Into<String>) {
        self.append_message(ContextMessage::system(content));
    }

    /// Appends a message and updates the approximate token count.
    pub fn append_message(&mut self, message: ContextMessage) {
        self.token_count += estimate_text_tokens(&message.content);
        self.messages.push(message);
    }

    /// Inserts a message at the requested index and updates token counts.
    pub fn insert_message(&mut self, index: usize, message: ContextMessage) {
        let bounded_index = index.min(self.messages.len());
        self.token_count += estimate_text_tokens(&message.content);
        self.messages.insert(bounded_index, message);
        for breakpoint in &mut self.cache_breakpoints {
            if *breakpoint > bounded_index {
                *breakpoint += 1;
            }
        }
        for breakpoint in &mut self.cache_controls {
            if let Some(index) = breakpoint.message_index()
                && index > bounded_index
            {
                *breakpoint = CacheBreakpoint::message(index + 1, breakpoint.ttl);
            }
        }
    }

    /// Extends the context with multiple messages and updates token counts.
    pub fn extend_messages<I>(&mut self, messages: I)
    where
        I: IntoIterator<Item = ContextMessage>,
    {
        for message in messages {
            self.append_message(message);
        }
    }

    /// Stores the active tool schemas for the request.
    pub fn set_tools(&mut self, tools: Vec<Value>) {
        self.tool_schemas = tools;
    }

    /// Returns the active tool schemas for the request.
    pub fn tools(&self) -> &[Value] {
        &self.tool_schemas
    }

    /// Returns mutable access to the active tool schemas for the request.
    pub fn tools_mut(&mut self) -> &mut Vec<Value> {
        &mut self.tool_schemas
    }

    /// Returns the auxiliary metadata map shared across stages.
    pub fn metadata(&self) -> &HashMap<String, Value> {
        &self.metadata
    }

    /// Returns mutable auxiliary metadata shared across stages.
    pub fn metadata_mut(&mut self) -> &mut HashMap<String, Value> {
        &mut self.metadata
    }

    /// Inserts one auxiliary metadata value for cross-stage coordination.
    pub fn insert_metadata(&mut self, key: impl Into<String>, value: Value) {
        self.metadata.insert(key.into(), value);
    }

    /// Marks the current message index as a cache breakpoint.
    pub fn mark_cache_breakpoint(&mut self) {
        self.mark_cache_breakpoint_with_ttl(CacheTtl::OneHour);
    }

    /// Marks the current message index as a cache breakpoint with an explicit TTL.
    pub fn mark_cache_breakpoint_with_ttl(&mut self, ttl: CacheTtl) {
        let index = self.messages.len();
        self.cache_breakpoints.push(index);
        self.cache_controls
            .push(CacheBreakpoint::message(index, ttl));
    }

    /// Marks the tool definitions block as a cache breakpoint with an explicit TTL.
    pub fn mark_tool_cache_breakpoint(&mut self, ttl: CacheTtl) {
        self.cache_controls.push(CacheBreakpoint::tools(ttl));
    }

    /// Returns the approximate token count of the last message.
    pub fn count_last(&self) -> usize {
        self.messages
            .last()
            .map(|message| estimate_text_tokens(&message.content))
            .unwrap_or(0)
    }

    /// Returns the most recent user-authored message text, if one exists.
    pub fn last_user_message(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.role == MessageRole::User)
            .map(|message| message.content.as_str())
    }

    /// Converts the compiled context into an LLM completion request.
    pub fn into_request(self) -> CompletionRequest {
        let model_id = self.model_capabilities.model_id;
        let max_output = self.model_capabilities.max_output;
        CompletionRequest {
            model: Some(model_id),
            messages: self.messages,
            tools: self.tool_schemas,
            max_output_tokens: Some(max_output),
            temperature: None,
            response_format: None,
            cache_breakpoints: self.cache_breakpoints,
            cache_controls: self.cache_controls,
            metadata: self.metadata,
        }
    }
}

/// Structured reason for excluding one item from a processor stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedItem {
    /// Stable identifier for the excluded item.
    pub item: String,
    /// Human-readable explanation for why the item was excluded.
    pub reason: String,
}

/// Output emitted by a context processor stage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProcessorOutput {
    /// Tokens added by the stage.
    pub tokens_added: usize,
    /// Tokens removed by the stage.
    pub tokens_removed: usize,
    /// Included item identifiers.
    pub items_included: Vec<String>,
    /// Excluded item identifiers.
    pub items_excluded: Vec<String>,
    /// Structured explanations for excluded items when the stage can provide them.
    pub excluded_items: Vec<ExcludedItem>,
    /// Auxiliary structured metadata emitted by the stage.
    pub metadata: HashMap<String, Value>,
    /// Stage execution duration.
    pub duration: Duration,
}

/// Estimates token usage using a rough four-characters-per-token heuristic.
pub fn estimate_text_tokens(text: &str) -> usize {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        0
    } else {
        trimmed.chars().count().div_ceil(4)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ContextMessage, MessageRole};
    use crate::types::{
        CacheBreakpoint, CacheTtl, ModelCapabilities, ModelId, Platform, SessionId, SessionMeta,
        TokenPricing, ToolCallFormat, ToolContent, ToolInvocation, UserId, WorkingContext,
        WorkspaceId,
    };

    #[test]
    fn context_message_tool_result_preserves_text_and_blocks() {
        let message = ContextMessage::tool_result(
            "toolu_123",
            "<untrusted_tool_output>\nhello\n</untrusted_tool_output>",
            Some(vec![ToolContent::Text {
                text: "hello".to_string(),
            }]),
        );

        assert_eq!(message.role, MessageRole::Tool);
        assert_eq!(message.tool_use_id.as_deref(), Some("toolu_123"));
        assert_eq!(
            message.content_blocks,
            Some(vec![ToolContent::Text {
                text: "hello".to_string()
            }])
        );
        assert!(message.content.contains("<untrusted_tool_output>"));
    }

    #[test]
    fn context_message_tool_still_defaults_to_text_only() {
        let message = ContextMessage::tool("plain text");

        assert_eq!(message.role, MessageRole::Tool);
        assert_eq!(message.content, "plain text");
        assert!(message.content_blocks.is_none());
        assert!(message.tool_invocation.is_none());
        assert!(message.tool_use_id.is_none());
    }

    #[test]
    fn context_message_assistant_tool_call_preserves_invocation() {
        let invocation = ToolInvocation {
            id: Some("toolu_123".to_string()),
            name: "bash".to_string(),
            input: serde_json::json!({ "cmd": "pwd" }),
        };
        let message = ContextMessage::assistant_tool_call(
            invocation.clone(),
            "<tool_call name=\"bash\">{\"cmd\":\"pwd\"}</tool_call>",
        );

        assert_eq!(message.role, MessageRole::Assistant);
        assert_eq!(message.tool_invocation, Some(invocation));
        assert!(message.content_blocks.is_none());
        assert!(message.tool_use_id.is_none());
    }

    #[test]
    fn working_context_into_request_preserves_cache_breakpoints() {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Desktop,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let capabilities = ModelCapabilities {
            model_id: ModelId::new("claude-sonnet-4-6"),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: Some(Duration::from_secs(300)),
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
            },
            native_tools: Vec::new(),
        };
        let mut ctx = WorkingContext::new(&session, capabilities);
        ctx.append_system("identity");
        ctx.mark_cache_breakpoint();
        ctx.append_message(ContextMessage::user("hello"));

        let request = ctx.into_request();

        assert_eq!(request.cache_breakpoints, vec![1]);
        assert_eq!(
            request.cache_controls,
            vec![CacheBreakpoint::message(1, CacheTtl::OneHour)]
        );
    }
}
