//! Context compilation messages and working state.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{
    AgentContext, AgentPolicySnapshot, CompletionRequest, ContactRef, EventRecord,
    ModelCapabilities, SandboxFile, SessionActorRef, SessionId, SessionMeta, TenantId, ToolCallId,
    ToolContent, ToolInvocation,
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

/// Origin category for a compiled context message or chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSourceKind {
    /// Message came from the append-only session event log.
    SessionEvent,
    /// Message came from graph memory retrieval.
    GraphMemory,
    /// Message came from a tool-call event.
    ToolCall,
    /// Message came from a tool-result event.
    ToolResult,
    /// Message came from a tool-error event.
    ToolError,
    /// Message was synthesized by a context processor.
    Synthetic,
}

/// Structured provenance for a compiled context message or chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSourceRef {
    /// Source category.
    pub kind: ContextSourceKind,
    /// Source object identifier, such as a graph node or session event id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uid: Option<Uuid>,
    /// Persisted session event id, when the source is an event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<Uuid>,
    /// Persisted session event sequence number, when the source is an event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_sequence_num: Option<u64>,
    /// Tool call id, when the source is a tool event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_id: Option<ToolCallId>,
    /// Human-readable source label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl ContextSourceRef {
    /// Creates a source ref for a persisted session event.
    #[must_use]
    pub fn session_event(record: &EventRecord) -> Self {
        Self {
            kind: ContextSourceKind::SessionEvent,
            source_uid: Some(record.id),
            event_id: Some(record.id),
            event_sequence_num: Some(record.sequence_num),
            tool_id: None,
            label: Some(format!("{:?}", record.event_type).to_ascii_lowercase()),
        }
    }

    /// Creates a source ref for a persisted tool call event.
    #[must_use]
    pub fn tool_call_event(record: &EventRecord, tool_id: ToolCallId) -> Self {
        Self {
            kind: ContextSourceKind::ToolCall,
            tool_id: Some(tool_id),
            ..Self::session_event(record)
        }
    }

    /// Creates a source ref for a persisted tool result event.
    #[must_use]
    pub fn tool_result_event(record: &EventRecord, tool_id: ToolCallId) -> Self {
        Self {
            kind: ContextSourceKind::ToolResult,
            tool_id: Some(tool_id),
            ..Self::session_event(record)
        }
    }

    /// Creates a source ref for a persisted tool error event.
    #[must_use]
    pub fn tool_error_event(record: &EventRecord, tool_id: ToolCallId) -> Self {
        Self {
            kind: ContextSourceKind::ToolError,
            tool_id: Some(tool_id),
            ..Self::session_event(record)
        }
    }

    /// Creates a source ref for a graph-memory node.
    #[must_use]
    pub fn graph_memory(uid: Uuid, label: impl Into<String>) -> Self {
        Self {
            kind: ContextSourceKind::GraphMemory,
            source_uid: Some(uid),
            event_id: None,
            event_sequence_num: None,
            tool_id: None,
            label: Some(label.into()),
        }
    }

    /// Creates a source ref for a synthesized context section.
    #[must_use]
    pub fn synthetic(label: impl Into<String>) -> Self {
        Self {
            kind: ContextSourceKind::Synthetic,
            source_uid: None,
            event_id: None,
            event_sequence_num: None,
            tool_id: None,
            label: Some(label.into()),
        }
    }
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
    /// Structured source references used by lineage and audit paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_refs: Vec<ContextSourceRef>,
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
            source_refs: Vec::new(),
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
            source_refs: Vec::new(),
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
            source_refs: Vec::new(),
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
            source_refs: Vec::new(),
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
            source_refs: Vec::new(),
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
            source_refs: Vec::new(),
        }
    }

    /// Adds one structured source reference.
    #[must_use]
    pub fn with_source_ref(mut self, source: ContextSourceRef) -> Self {
        self.source_refs.push(source);
        self
    }

    /// Replaces structured source references.
    #[must_use]
    pub fn with_source_refs(mut self, sources: impl IntoIterator<Item = ContextSourceRef>) -> Self {
        self.source_refs = sources.into_iter().collect();
        self
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
    /// Tenant runtime boundary that owns the session.
    pub tenant_id: TenantId,
    /// Agent-facing contact snapshot attached to this session, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contact: Option<ContactRef>,
    /// Actor that created the session, when recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<SessionActorRef>,
    /// Configured agent policy pinned to this session, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_context: Option<AgentContext>,
    /// Active tool schemas compiled for the request.
    tool_schemas: Vec<Value>,
    /// Arbitrary processor metadata.
    metadata: HashMap<String, Value>,
    /// Runtime-only trusted files to install into a hand before tool execution.
    #[serde(skip)]
    trusted_sandbox_files: Vec<SandboxFile>,
    /// Runtime-only recent session events preloaded by the orchestrator bridge.
    #[serde(skip)]
    recent_events: Vec<EventRecord>,
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
            tenant_id: session.tenant_id,
            contact: session.contact.clone(),
            created_by: session.created_by.clone(),
            agent_context: session.agent_context.clone(),
            tool_schemas: Vec::new(),
            metadata: HashMap::new(),
            trusted_sandbox_files: Vec::new(),
            recent_events: Vec::new(),
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

    /// Parses the configured-agent policy snapshot pinned to this context, when one exists.
    pub fn agent_policy_snapshot(&self) -> crate::Result<Option<AgentPolicySnapshot>> {
        self.agent_context
            .as_ref()
            .map(AgentContext::parsed_policy_snapshot)
            .transpose()
    }

    /// Returns mutable access to the active tool schemas for the request.
    pub fn tools_mut(&mut self) -> &mut Vec<Value> {
        &mut self.tool_schemas
    }

    /// Returns the auxiliary metadata map shared across stages.
    pub fn metadata(&self) -> &HashMap<String, Value> {
        &self.metadata
    }

    /// Inserts one auxiliary metadata value for cross-stage coordination.
    pub fn insert_metadata(&mut self, key: impl Into<String>, value: Value) {
        self.metadata.insert(key.into(), value);
    }

    /// Adds trusted files that the runtime may materialize into a sandbox.
    pub fn extend_trusted_sandbox_files<I>(&mut self, files: I)
    where
        I: IntoIterator<Item = SandboxFile>,
    {
        self.trusted_sandbox_files.extend(files);
    }

    /// Takes trusted sandbox files out of the context without serializing them into model metadata.
    pub fn take_trusted_sandbox_files(&mut self) -> Vec<SandboxFile> {
        std::mem::take(&mut self.trusted_sandbox_files)
    }

    /// Stores recent session events for processors that need the current turn tail.
    pub fn set_recent_events(&mut self, events: Vec<EventRecord>) {
        self.recent_events = events;
    }

    /// Returns recent session events preloaded for this compilation run.
    pub fn recent_events(&self) -> &[EventRecord] {
        &self.recent_events
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
    use super::{ContextMessage, ContextSourceKind, ContextSourceRef, MessageRole};
    use crate::types::{ToolContent, ToolInvocation};
    use uuid::Uuid;

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
    fn context_message_source_refs_are_preserved() {
        // Pins: compiled context messages can carry structured lineage provenance.
        let uid = Uuid::now_v7();
        let source = ContextSourceRef::graph_memory(uid, "Fact:oauth");
        let message =
            ContextMessage::user("OAuth uses access tokens.").with_source_ref(source.clone());

        assert_eq!(message.source_refs, vec![source]);
        assert_eq!(message.source_refs[0].kind, ContextSourceKind::GraphMemory);
        assert_eq!(message.source_refs[0].source_uid, Some(uid));
    }
}
