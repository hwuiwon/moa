//! Shared deterministic fixtures for context-pipeline integration tests.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use moa_core::{
    ContextMessage, ModelCapabilities, ModelId, Platform, SessionId, SessionMeta, SessionStatus,
    TokenPricing, ToolCallFormat, UserId, WorkingContext, WorkspaceId,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use uuid::Uuid;

const DEFAULT_CLOCK: &str = "2026-05-07T12:00:00Z";
const WORKSPACE_ROOT_METADATA_KEY: &str = "_moa.runtime.workspace_root";

/// Builds a deterministic [`WorkingContext`] fixture for one-stage pipeline tests.
///
/// Example:
/// ```ignore
/// let fixture = WorkingContextFixture::new()
///     .with_workspace_id("ws-001")
///     .with_tools(&["bash", "file_read", "file_write"])
///     .with_memory_hits(&[mem_hit("auth.rs", "uses jwt")])
///     .with_user_message("fix the auth bug")
///     .with_clock_at("2026-05-07T12:00:00Z")
///     .build();
/// ```
pub struct WorkingContextFixture {
    workspace_id: String,
    user_id: String,
    model_id: String,
    tool_names: Vec<String>,
    memory_hits: Vec<MemoryHitSpec>,
    messages: Vec<ContextMessage>,
    clock_at: DateTime<Utc>,
}

impl WorkingContextFixture {
    /// Creates a fixture with a stable workspace, user, model, clock, and one user message.
    #[must_use]
    pub fn new() -> Self {
        Self {
            workspace_id: "ws-fixture".to_string(),
            user_id: "user-fixture".to_string(),
            model_id: "claude-sonnet-4-6".to_string(),
            tool_names: Vec::new(),
            memory_hits: Vec::new(),
            messages: vec![ContextMessage::user("fix the auth bug")],
            clock_at: parse_utc(DEFAULT_CLOCK),
        }
    }

    /// Sets the workspace id used by the compiled context and workspace-root fixture.
    #[must_use]
    pub fn with_workspace_id(mut self, workspace_id: &str) -> Self {
        self.workspace_id = workspace_id.to_string();
        self
    }

    /// Sets the user id used by the compiled context.
    #[must_use]
    pub fn with_user_id(mut self, user_id: &str) -> Self {
        self.user_id = user_id.to_string();
        self
    }

    /// Sets the active model id used by the compiled context.
    #[must_use]
    pub fn with_model_id(mut self, model_id: &str) -> Self {
        self.model_id = model_id.to_string();
        self
    }

    /// Sets the available tool names and preloads matching deterministic tool schemas.
    #[must_use]
    pub fn with_tools(mut self, tools: &[&str]) -> Self {
        self.tool_names = tools.iter().map(|tool| (*tool).to_string()).collect();
        self
    }

    /// Sets memory-hit specs assigned stable UUIDs when the fixture is built.
    #[must_use]
    pub fn with_memory_hits(mut self, hits: &[MemoryHitSpec]) -> Self {
        self.memory_hits = hits.to_vec();
        self
    }

    /// Replaces the fixture conversation with one user message.
    #[must_use]
    pub fn with_user_message(mut self, message: &str) -> Self {
        self.messages = vec![ContextMessage::user(message)];
        self
    }

    /// Replaces the fixture conversation with explicit context messages.
    #[must_use]
    pub fn with_messages(mut self, messages: Vec<ContextMessage>) -> Self {
        self.messages = messages;
        self
    }

    /// Sets the deterministic clock timestamp used by runtime-stage tests.
    #[must_use]
    pub fn with_clock_at(mut self, timestamp: &str) -> Self {
        self.clock_at = parse_utc(timestamp);
        self
    }

    /// Builds the deterministic context fixture.
    #[must_use]
    pub fn build(self) -> BuiltWorkingContextFixture {
        let tempdir = tempfile::tempdir().expect("pipeline fixture tempdir should be created");
        let workspace_root = tempdir.path().join(&self.workspace_id);
        let session = SessionMeta {
            id: SessionId(Uuid::from_u128(0x100)),
            workspace_id: WorkspaceId::new(self.workspace_id.clone()),
            user_id: UserId::new(self.user_id.clone()),
            title: None,
            status: SessionStatus::Created,
            platform: Platform::Cli,
            platform_channel: None,
            model: ModelId::new(self.model_id.clone()),
            created_at: self.clock_at,
            updated_at: self.clock_at,
            completed_at: None,
            parent_session_id: None,
            total_input_tokens: 0,
            total_input_tokens_uncached: 0,
            total_input_tokens_cache_write: 0,
            total_input_tokens_cache_read: 0,
            total_output_tokens: 0,
            total_cost_cents: 0,
            event_count: 0,
            last_checkpoint_seq: None,
        };
        let mut ctx = WorkingContext::new(&session, capabilities(&self.model_id));
        ctx.insert_metadata(
            WORKSPACE_ROOT_METADATA_KEY,
            json!(workspace_root.display().to_string()),
        );
        ctx.set_tools(
            self.tool_names
                .iter()
                .map(|name| tool_schema(name))
                .collect(),
        );
        ctx.extend_messages(self.messages);
        let memory_hits = self
            .memory_hits
            .into_iter()
            .enumerate()
            .map(|(index, spec)| spec.into_hit(index))
            .collect();

        BuiltWorkingContextFixture {
            ctx,
            tool_schemas: self
                .tool_names
                .iter()
                .map(|name| tool_schema(name))
                .collect(),
            memory_hits,
            clock_at: self.clock_at,
            workspace_root,
            _tempdir: tempdir,
        }
    }
}

impl Default for WorkingContextFixture {
    fn default() -> Self {
        Self::new()
    }
}

/// Built deterministic context plus fixture-side inputs used by individual stages.
pub struct BuiltWorkingContextFixture {
    /// Mutable working context under test.
    pub ctx: WorkingContext,
    /// Deterministic tool schemas corresponding to the requested tool names.
    pub tool_schemas: Vec<Value>,
    /// Deterministic memory hits corresponding to the requested hit specs.
    pub memory_hits: Vec<MemoryHit>,
    /// Fixed clock instant used by runtime-stage tests.
    pub clock_at: DateTime<Utc>,
    /// Workspace root path recorded in context metadata.
    pub workspace_root: PathBuf,
    _tempdir: TempDir,
}

/// One deterministic memory hit specification before UUID assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryHitSpec {
    name: String,
    summary: String,
    valid: bool,
}

impl MemoryHitSpec {
    /// Marks the memory hit as invalidated before retrieval.
    #[must_use]
    pub fn invalidated(mut self) -> Self {
        self.valid = false;
        self
    }

    fn into_hit(self, index: usize) -> MemoryHit {
        MemoryHit {
            uid: Uuid::from_u128(0x1_000 + index as u128),
            name: self.name,
            summary: self.summary,
            valid: self.valid,
        }
    }
}

/// Creates a valid deterministic memory-hit fixture.
#[must_use]
pub fn mem_hit(name: &str, summary: &str) -> MemoryHitSpec {
    MemoryHitSpec {
        name: name.to_string(),
        summary: summary.to_string(),
        valid: true,
    }
}

/// One deterministic memory hit with an assigned UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryHit {
    /// Stable graph-memory UID.
    pub uid: Uuid,
    /// Human-readable node name.
    pub name: String,
    /// Summary inserted into `properties_summary`.
    pub summary: String,
    /// Whether the node is active for retrieval.
    pub valid: bool,
}

/// Creates a deterministic tool schema for the named tool.
#[must_use]
pub fn tool_schema(name: &str) -> Value {
    json!({
        "name": name,
        "description": format!("Deterministic fixture tool {name}"),
        "input_schema": {
            "type": "object",
            "properties": {
                "input": { "type": "string" }
            },
            "required": ["input"]
        }
    })
}

/// Returns deterministic model capabilities for context fixture tests.
#[must_use]
pub fn capabilities(model_id: &str) -> ModelCapabilities {
    ModelCapabilities {
        model_id: ModelId::new(model_id),
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
    }
}

fn parse_utc(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .expect("fixture timestamp should be RFC3339")
        .with_timezone(&Utc)
}
