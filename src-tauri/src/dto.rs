//! Serializable DTOs exposed on the Tauri IPC boundary.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use moa_core::{
    Event, EventRecord, MemoryScope, MemorySearchResult, MoaConfig, PageSummary, SessionMeta,
    SessionSummary, WikiPage,
};
use moa_runtime::{ChatRuntime, SessionPreview};
use serde::Serialize;
use serde_json::Value;

/// Summary of the currently selected runtime.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeInfoDto {
    /// Active session identifier.
    pub session_id: String,
    /// Active workspace identifier.
    pub workspace_id: String,
    /// Selected model identifier.
    pub model: String,
    /// Local sandbox root path.
    pub sandbox_root: String,
    /// Runtime transport kind.
    pub runtime_kind: String,
}

impl RuntimeInfoDto {
    /// Builds a DTO from the active runtime.
    pub fn from_runtime(runtime: &ChatRuntime) -> Self {
        let runtime_kind = match runtime {
            ChatRuntime::Local(_) => "local",
            ChatRuntime::Daemon(_) => "daemon",
        };

        Self {
            session_id: runtime.session_id().to_string(),
            workspace_id: runtime.workspace_id().to_string(),
            model: runtime.model().to_string(),
            sandbox_root: runtime.sandbox_root().display().to_string(),
            runtime_kind: runtime_kind.to_string(),
        }
    }
}

/// Compact session listing row for the frontend session list.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummaryDto {
    /// Session identifier.
    pub session_id: String,
    /// Workspace identifier.
    pub workspace_id: String,
    /// User identifier.
    pub user_id: String,
    /// Optional title.
    pub title: Option<String>,
    /// Session lifecycle status.
    pub status: String,
    /// Origin platform.
    pub platform: String,
    /// Model identifier.
    pub model: String,
    /// Last update timestamp.
    pub updated_at: String,
    /// Whether this is the active session in the desktop runtime.
    pub active: bool,
}

impl SessionSummaryDto {
    /// Builds a DTO from a session summary row.
    pub fn from_summary(summary: SessionSummary, active_session_id: &str) -> Self {
        let session_id = summary.session_id.to_string();
        Self {
            workspace_id: summary.workspace_id.to_string(),
            user_id: summary.user_id.to_string(),
            title: summary.title,
            status: enum_label(&summary.status),
            platform: enum_label(&summary.platform),
            model: summary.model,
            updated_at: iso(summary.updated_at),
            active: session_id == active_session_id,
            session_id,
        }
    }
}

/// Session preview row used by the sidebar.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPreviewDto {
    /// Persisted session summary.
    pub summary: SessionSummaryDto,
    /// Most recent conversational text when present.
    pub last_message: Option<String>,
}

impl SessionPreviewDto {
    /// Builds a DTO from a runtime session preview.
    pub fn from_preview(preview: SessionPreview, active_session_id: &str) -> Self {
        Self {
            summary: SessionSummaryDto::from_summary(preview.summary, active_session_id),
            last_message: preview.last_message,
        }
    }
}

/// Full session metadata snapshot.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetaDto {
    /// Session identifier.
    pub id: String,
    /// Workspace identifier.
    pub workspace_id: String,
    /// User identifier.
    pub user_id: String,
    /// Optional session title.
    pub title: Option<String>,
    /// Session lifecycle status.
    pub status: String,
    /// Origin platform.
    pub platform: String,
    /// Optional platform channel identifier.
    pub platform_channel: Option<String>,
    /// Model identifier.
    pub model: String,
    /// Session creation timestamp.
    pub created_at: String,
    /// Last update timestamp.
    pub updated_at: String,
    /// Completion timestamp when available.
    pub completed_at: Option<String>,
    /// Parent session identifier when this is a child session.
    pub parent_session_id: Option<String>,
    /// Aggregate input tokens.
    pub total_input_tokens: usize,
    /// Aggregate output tokens.
    pub total_output_tokens: usize,
    /// Aggregate cost in cents.
    pub total_cost_cents: u32,
    /// Persisted event count.
    pub event_count: usize,
    /// Last checkpoint sequence when present.
    pub last_checkpoint_seq: Option<u64>,
}

impl From<SessionMeta> for SessionMetaDto {
    fn from(session: SessionMeta) -> Self {
        Self {
            id: session.id.to_string(),
            workspace_id: session.workspace_id.to_string(),
            user_id: session.user_id.to_string(),
            title: session.title,
            status: enum_label(&session.status),
            platform: enum_label(&session.platform),
            platform_channel: session.platform_channel,
            model: session.model,
            created_at: iso(session.created_at),
            updated_at: iso(session.updated_at),
            completed_at: session.completed_at.map(iso),
            parent_session_id: session.parent_session_id.map(|value| value.to_string()),
            total_input_tokens: session.total_input_tokens,
            total_output_tokens: session.total_output_tokens,
            total_cost_cents: session.total_cost_cents,
            event_count: session.event_count,
            last_checkpoint_seq: session.last_checkpoint_seq,
        }
    }
}

/// Serialized event-log row for session inspection.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRecordDto {
    /// Event identifier.
    pub id: String,
    /// Session identifier.
    pub session_id: String,
    /// Sequence number within the session.
    pub sequence_num: u64,
    /// Event type label.
    pub event_type: String,
    /// Event timestamp.
    pub timestamp: String,
    /// Optional token count for the event.
    pub token_count: Option<usize>,
    /// Serialized event payload.
    pub payload: Value,
}

impl From<EventRecord> for EventRecordDto {
    fn from(record: EventRecord) -> Self {
        Self {
            id: record.id.to_string(),
            session_id: record.session_id.to_string(),
            sequence_num: record.sequence_num,
            event_type: enum_label(&record.event_type),
            timestamp: iso(record.timestamp),
            token_count: record.token_count,
            payload: event_payload(record.event),
        }
    }
}

/// Compact memory search result row.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySearchResultDto {
    /// Human-readable memory scope label.
    pub scope: String,
    /// Logical page path.
    pub path: String,
    /// Page title.
    pub title: String,
    /// Page type label.
    pub page_type: String,
    /// Search snippet.
    pub snippet: String,
    /// Confidence label.
    pub confidence: String,
    /// Last update timestamp.
    pub updated: String,
    /// Reference count.
    pub reference_count: u64,
}

impl From<MemorySearchResult> for MemorySearchResultDto {
    fn from(result: MemorySearchResult) -> Self {
        Self {
            scope: memory_scope_label(&result.scope),
            path: result.path.to_string(),
            title: result.title,
            page_type: enum_label(&result.page_type),
            snippet: result.snippet,
            confidence: enum_label(&result.confidence),
            updated: iso(result.updated),
            reference_count: result.reference_count,
        }
    }
}

/// Compact memory page listing entry.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageSummaryDto {
    /// Logical page path.
    pub path: String,
    /// Page title.
    pub title: String,
    /// Page type label.
    pub page_type: String,
    /// Confidence label.
    pub confidence: String,
    /// Last update timestamp.
    pub updated: String,
}

impl From<PageSummary> for PageSummaryDto {
    fn from(page: PageSummary) -> Self {
        Self {
            path: page.path.to_string(),
            title: page.title,
            page_type: enum_label(&page.page_type),
            confidence: enum_label(&page.confidence),
            updated: iso(page.updated),
        }
    }
}

/// Full wiki page payload returned to the frontend editor.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WikiPageDto {
    /// Logical page path when one exists.
    pub path: Option<String>,
    /// Page title.
    pub title: String,
    /// Page type label.
    pub page_type: String,
    /// Raw markdown content.
    pub content: String,
    /// Creation timestamp.
    pub created: String,
    /// Last update timestamp.
    pub updated: String,
    /// Confidence label.
    pub confidence: String,
    /// Explicit related links.
    pub related: Vec<String>,
    /// Provenance sources.
    pub sources: Vec<String>,
    /// User-defined tags.
    pub tags: Vec<String>,
    /// Whether the page was auto-generated.
    pub auto_generated: bool,
    /// Last reference timestamp.
    pub last_referenced: String,
    /// Reference count.
    pub reference_count: u64,
    /// Additional preserved metadata.
    pub metadata: HashMap<String, Value>,
}

impl From<WikiPage> for WikiPageDto {
    fn from(page: WikiPage) -> Self {
        Self {
            path: page.path.map(|value| value.to_string()),
            title: page.title,
            page_type: enum_label(&page.page_type),
            content: page.content,
            created: iso(page.created),
            updated: iso(page.updated),
            confidence: enum_label(&page.confidence),
            related: page.related,
            sources: page.sources,
            tags: page.tags,
            auto_generated: page.auto_generated,
            last_referenced: iso(page.last_referenced),
            reference_count: page.reference_count,
            metadata: page.metadata,
        }
    }
}

/// Runtime configuration values the frontend needs to render settings.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoaConfigDto {
    /// Default provider name.
    pub default_provider: String,
    /// Default model identifier.
    pub default_model: String,
    /// Requested reasoning effort.
    pub reasoning_effort: String,
    /// Whether native provider web search is enabled.
    pub web_search_enabled: bool,
    /// Optional workspace instructions.
    pub workspace_instructions: Option<String>,
    /// Optional user instructions.
    pub user_instructions: Option<String>,
    /// Sandbox directory path.
    pub sandbox_dir: String,
    /// Memory directory path.
    pub memory_dir: String,
    /// Whether daemon auto-connect is enabled.
    pub daemon_auto_connect: bool,
    /// Whether observability export is enabled.
    pub observability_enabled: bool,
    /// Deployment environment label when configured.
    pub environment: Option<String>,
}

impl From<&MoaConfig> for MoaConfigDto {
    fn from(config: &MoaConfig) -> Self {
        Self {
            default_provider: config.general.default_provider.clone(),
            default_model: config.general.default_model.clone(),
            reasoning_effort: config.general.reasoning_effort.clone(),
            web_search_enabled: config.general.web_search_enabled,
            workspace_instructions: config.general.workspace_instructions.clone(),
            user_instructions: config.general.user_instructions.clone(),
            sandbox_dir: config.local.sandbox_dir.clone(),
            memory_dir: config.local.memory_dir.clone(),
            daemon_auto_connect: config.daemon.auto_connect,
            observability_enabled: config.observability.enabled,
            environment: config.observability.environment.clone(),
        }
    }
}

fn iso(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339()
}

fn enum_label<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "unknown".to_string())
}

fn memory_scope_label(scope: &MemoryScope) -> String {
    match scope {
        MemoryScope::User(user_id) => format!("user:{user_id}"),
        MemoryScope::Workspace(workspace_id) => format!("workspace:{workspace_id}"),
    }
}

fn event_payload(event: Event) -> Value {
    serde_json::to_value(event).unwrap_or_else(|error| Value::String(error.to_string()))
}
