//! Stage 7: injects per-turn runtime context outside the cached prompt prefix.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{
    ContextMessage, ContextProcessor, MessageRole, ProcessorOutput, Result, WorkingContext,
};
use tokio::process::Command;

use super::estimate_tokens;

pub(crate) const WORKSPACE_ROOT_METADATA_KEY: &str = "_moa.runtime.workspace_root";

/// Clock abstraction used to freeze runtime context in tests.
pub trait Clock: Send + Sync {
    /// Returns the current UTC timestamp for the active turn.
    fn now(&self) -> DateTime<Utc>;
}

/// Production clock backed by the system wall clock.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Fixed clock for deterministic runtime-context tests.
#[cfg(test)]
#[derive(Debug, Clone)]
struct FixedClock {
    now: DateTime<Utc>,
}

#[cfg(test)]
impl FixedClock {
    /// Creates a fixed clock that always returns the provided timestamp.
    fn new(now: DateTime<Utc>) -> Self {
        Self { now }
    }
}

#[cfg(test)]
impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.now
    }
}

/// Emits a trailing runtime reminder message that can vary between turns.
pub struct RuntimeContextProcessor {
    clock: Arc<dyn Clock>,
}

impl RuntimeContextProcessor {
    /// Creates a runtime-context processor using the provided clock.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock }
    }

    /// Creates a runtime-context processor with a deterministic fixed clock.
    #[cfg(test)]
    fn with_fixed_clock(now: DateTime<Utc>) -> Self {
        Self::new(Arc::new(FixedClock::new(now)))
    }
}

impl Default for RuntimeContextProcessor {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock))
    }
}

#[async_trait]
impl ContextProcessor for RuntimeContextProcessor {
    fn name(&self) -> &str {
        "runtime_context"
    }

    fn stage(&self) -> u8 {
        7
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        let reminder = build_runtime_reminder(self.clock.now(), ctx).await;
        let insertion_index = runtime_context_insertion_index(&ctx.messages);
        let tokens_added = estimate_tokens(&reminder);
        ctx.insert_message(insertion_index, ContextMessage::user(reminder));

        Ok(ProcessorOutput {
            tokens_added,
            items_included: vec!["runtime_context".to_string()],
            ..ProcessorOutput::default()
        })
    }
}

async fn build_runtime_reminder(now: DateTime<Utc>, ctx: &WorkingContext) -> String {
    let workspace_root = workspace_root_from_context(ctx);
    let workspace_name = workspace_root
        .as_ref()
        .and_then(|path| path.file_name())
        .and_then(|segment| segment.to_str())
        .map(ToOwned::to_owned);
    let git_branch = match workspace_root.as_ref() {
        Some(path) => detect_git_branch(path).await,
        None => None,
    };

    let mut lines = vec![
        "<system-reminder>".to_string(),
        format!("Current date: {}", now.format("%Y-%m-%d")),
    ];

    if let Some(name) = workspace_name {
        lines.push(format!("Current workspace: {name}"));
    }

    if let Some(path) = workspace_root {
        lines.push(format!("Current working directory: {}", path.display()));
    }

    if let Some(branch) = git_branch {
        lines.push(format!("Current git branch: {branch}"));
    }

    lines.push(format!("Current user: {}", ctx.user_id));
    lines.push("</system-reminder>".to_string());
    lines.join("\n")
}

fn workspace_root_from_context(ctx: &WorkingContext) -> Option<PathBuf> {
    ctx.metadata()
        .get(WORKSPACE_ROOT_METADATA_KEY)
        .and_then(|value| value.as_str())
        .map(PathBuf::from)
}

fn runtime_context_insertion_index(messages: &[ContextMessage]) -> usize {
    let mut insertion_index = messages.len();
    while insertion_index > 0 && messages[insertion_index - 1].role == MessageRole::User {
        insertion_index -= 1;
    }
    insertion_index
}

async fn detect_git_branch(workspace_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .arg("rev-parse")
        .arg("--abbrev-ref")
        .arg("HEAD")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let branch = String::from_utf8(output.stdout).ok()?;
    let branch = branch.trim();
    if branch.is_empty() || branch == "HEAD" {
        return None;
    }

    Some(branch.to_string())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use moa_core::{
        ContextMessage, ModelCapabilities, Platform, SessionId, SessionMeta, TokenPricing,
        ToolCallFormat, UserId, WorkspaceId,
    };

    use super::*;

    fn capabilities() -> ModelCapabilities {
        ModelCapabilities {
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
        }
    }

    fn session() -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Desktop,
            model: "claude-sonnet-4-6".to_string(),
            ..SessionMeta::default()
        }
    }

    #[tokio::test]
    async fn runtime_context_inserts_before_trailing_user_turn() {
        let mut ctx = WorkingContext::new(&session(), capabilities());
        ctx.append_system("identity");
        ctx.append_message(ContextMessage::assistant("Earlier answer"));
        ctx.append_message(ContextMessage::user("Current turn prompt"));
        ctx.insert_metadata(
            WORKSPACE_ROOT_METADATA_KEY,
            serde_json::json!("/tmp/runtime-context"),
        );

        RuntimeContextProcessor::with_fixed_clock(
            Utc.with_ymd_and_hms(2026, 4, 16, 12, 0, 0).unwrap(),
        )
        .process(&mut ctx)
        .await
        .expect("runtime context should compile");

        assert_eq!(ctx.messages.len(), 4);
        assert_eq!(ctx.messages[2].role, MessageRole::User);
        assert!(ctx.messages[2].content.contains("<system-reminder>"));
        assert_eq!(ctx.messages[3].content, "Current turn prompt");
    }

    #[tokio::test]
    async fn runtime_context_changes_when_clock_advances() {
        let mut first = WorkingContext::new(&session(), capabilities());
        first.insert_metadata(
            WORKSPACE_ROOT_METADATA_KEY,
            serde_json::json!("/tmp/runtime-context"),
        );

        let mut second = WorkingContext::new(&session(), capabilities());
        second.insert_metadata(
            WORKSPACE_ROOT_METADATA_KEY,
            serde_json::json!("/tmp/runtime-context"),
        );

        RuntimeContextProcessor::with_fixed_clock(
            Utc.with_ymd_and_hms(2026, 4, 16, 12, 0, 0).unwrap(),
        )
        .process(&mut first)
        .await
        .expect("first runtime context should compile");
        RuntimeContextProcessor::with_fixed_clock(
            Utc.with_ymd_and_hms(2026, 4, 17, 12, 0, 0).unwrap(),
        )
        .process(&mut second)
        .await
        .expect("second runtime context should compile");

        assert_ne!(first.messages[0].content, second.messages[0].content);
        assert!(
            first.messages[0]
                .content
                .contains("Current date: 2026-04-16")
        );
        assert!(
            second.messages[0]
                .content
                .contains("Current date: 2026-04-17")
        );
    }
}
