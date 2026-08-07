//! Tool output budgeting, truncation, and claim-check artifactization.

use moa_config::ToolBudgetConfig;
use moa_config::ToolOutputConfig;
use moa_core::{
    truncation::truncate_head_tail, types::session::SessionMeta,
    types::tools::ToolArtifactByteRange, types::tools::ToolContent, types::tools::ToolDefinition,
    types::tools::ToolOutput, types::tools::ToolOutputArtifact,
};
use serde_json::json;

use super::ToolRouter;

impl ToolRouter {
    /// Overrides the router's replay truncation settings used for head/tail shaping.
    #[must_use]
    pub fn with_tool_output_config(mut self, tool_output: ToolOutputConfig) -> Self {
        self.bindings.set_tool_output(tool_output);
        self
    }

    /// Overrides the router's per-tool output budgets.
    #[must_use]
    pub fn with_tool_budgets(mut self, tool_budgets: ToolBudgetConfig) -> Self {
        let mut registry = (*self.registry()).clone();
        registry.apply_budgets(&tool_budgets);
        self.publish_registry(registry);
        self.bindings.set_tool_budgets(tool_budgets);
        self
    }

    pub(super) async fn apply_output_budget(
        &self,
        session: &SessionMeta,
        tool_definition: &ToolDefinition,
        output: ToolOutput,
    ) -> ToolOutput {
        let existing_truncated = output.truncated;
        let existing_original_output_tokens = output.original_output_tokens;
        let original_payload = ArtifactPayload::from_output(&output);
        let original_output_tokens = original_payload.estimated_tokens;
        if let Some(artifactized_output) = self
            .artifactize_output(session, tool_definition, &output, &original_payload)
            .await
        {
            return artifactized_output.with_truncated(true);
        }

        let (stream_budgeted_output, stream_truncated) =
            self.apply_stream_budget(tool_definition, output);
        let text_budget_payload = if stream_truncated {
            ArtifactPayload::from_output(&stream_budgeted_output)
        } else {
            original_payload
        };

        let (mut final_output, text_truncated) = self.apply_text_budget(
            tool_definition,
            original_output_tokens,
            stream_budgeted_output,
            text_budget_payload,
            stream_truncated,
        );
        let router_truncated = stream_truncated || text_truncated;
        let truncated = existing_truncated || router_truncated;
        final_output.truncated = truncated;
        final_output.original_output_tokens = if router_truncated {
            Some(original_output_tokens)
        } else {
            existing_original_output_tokens
        };

        final_output
    }

    async fn artifactize_output(
        &self,
        session: &SessionMeta,
        tool_definition: &ToolDefinition,
        output: &ToolOutput,
        payload: &ArtifactPayload,
    ) -> Option<ToolOutput> {
        if payload.estimated_tokens <= tool_definition.max_output_tokens {
            return None;
        }

        let session_store = self.bindings.session_store()?;

        let combined = match session_store
            .store_text_artifact(session.id, &payload.text)
            .await
        {
            Ok(claim_check) => claim_check,
            Err(error) => {
                tracing::warn!(
                    session_id = %session.id,
                    tool_name = %tool_definition.name,
                    error = %error,
                    "failed to persist oversized tool output artifact; falling back to inline truncation"
                );
                return None;
            }
        };
        let artifact = ToolOutputArtifact {
            combined,
            estimated_tokens: payload.estimated_tokens,
            line_count: count_lines(&payload.text),
            stdout_range: payload.stdout_range,
            stderr_range: payload.stderr_range,
            stdout: None,
            stderr: None,
        };
        let inline_preview_tokens =
            inline_artifact_preview_budget(tool_definition.max_output_tokens);
        let preview_footer = artifact_storage_footer(&artifact);
        let preview_budget_chars = inline_preview_tokens
            .saturating_mul(4)
            .saturating_sub(preview_footer.chars().count() as u32)
            as usize;
        // JSON-shaped outputs get a structure-aware preview (empty fields
        // dropped, long arrays elided) instead of a blind head/tail cut; the
        // stored artifact keeps the full payload either way.
        let preview =
            json_compact_preview(output, preview_budget_chars.max(1)).unwrap_or_else(|| {
                truncate_head_tail(
                    &payload.text,
                    preview_budget_chars.max(1),
                    self.bindings.tool_output().head_ratio,
                )
                .0
            });
        let summary = format_artifact_summary(
            output.is_error,
            output.process_exit_code(),
            artifact.available_streams(),
            append_footer(&preview, &preview_footer),
        );

        Some(ToolOutput {
            content: vec![ToolContent::Text { text: summary }],
            is_error: output.is_error,
            structured: Some(json!({
                "artifact_available": true,
                "estimated_tokens": artifact.estimated_tokens,
                "line_count": artifact.line_count,
                "available_streams": artifact.available_streams(),
                "exit_code": output.process_exit_code(),
            })),
            duration: output.duration,
            truncated: true,
            original_output_tokens: Some(payload.estimated_tokens),
            artifact: Some(artifact),
        })
    }

    fn apply_stream_budget(
        &self,
        tool_definition: &ToolDefinition,
        output: ToolOutput,
    ) -> (ToolOutput, bool) {
        if tool_definition.name != "bash" {
            return (output, false);
        }

        let Some(exit_code) = output.process_exit_code() else {
            return (output, false);
        };
        let stdout = output.process_stdout().unwrap_or_default();
        let stderr = output.process_stderr().unwrap_or_default();

        let stdout_budget = self.bindings.tool_budgets().bash_stdout;
        let stderr_budget = self.bindings.tool_budgets().bash_stderr;
        let (stdout, stdout_truncated) = truncate_text_for_budget(
            stdout,
            stdout_budget,
            self.bindings.tool_output().head_ratio,
        );
        let (stderr, stderr_truncated) = truncate_text_for_budget(
            stderr,
            stderr_budget,
            self.bindings.tool_output().head_ratio,
        );

        if !stdout_truncated && !stderr_truncated {
            return (output, false);
        }

        (
            ToolOutput::from_process_with_source_truncation(
                stdout,
                stderr,
                exit_code,
                output.duration,
                output.process_stdout_truncated() || stdout_truncated,
                output.process_stderr_truncated() || stderr_truncated,
                output.original_output_tokens,
            ),
            true,
        )
    }

    fn apply_text_budget(
        &self,
        tool_definition: &ToolDefinition,
        original_output_tokens: u32,
        mut output: ToolOutput,
        payload: ArtifactPayload,
        stream_truncated: bool,
    ) -> (ToolOutput, bool) {
        let budget = tool_definition.max_output_tokens;
        if payload.estimated_tokens <= budget && !stream_truncated {
            return (output, false);
        }

        let footer = truncation_footer(original_output_tokens, budget);
        let available_chars = budget
            .saturating_mul(4)
            .saturating_sub(footer.chars().count() as u32) as usize;

        if payload.estimated_tokens > budget {
            let (truncated_text, _) = truncate_head_tail(
                &payload.text,
                available_chars.max(1),
                self.bindings.tool_output().head_ratio,
            );
            replace_with_inline_text(&mut output, append_footer(&truncated_text, &footer));
            return (output, true);
        }

        let inline_payload = payload.into_inline_text();
        if inline_payload.estimated_tokens_with_footer(&footer) > budget {
            let (truncated_text, _) = truncate_head_tail(
                &inline_payload.text,
                available_chars.max(1),
                self.bindings.tool_output().head_ratio,
            );
            replace_with_inline_text(&mut output, append_footer(&truncated_text, &footer));
        } else {
            replace_with_inline_text(&mut output, inline_payload.into_text_with_footer(&footer));
        }
        (output, false)
    }
}

struct ArtifactPayload {
    text: String,
    character_count: u32,
    estimated_tokens: u32,
    stdout_range: Option<ToolArtifactByteRange>,
    stderr_range: Option<ToolArtifactByteRange>,
}

impl ArtifactPayload {
    fn from_output(output: &ToolOutput) -> Self {
        let Some(exit_code) = output.process_exit_code() else {
            return Self::without_streams(output.to_text());
        };
        let stdout = output.process_stdout();
        let stderr = output.process_stderr();
        if stdout.is_none() && stderr.is_none() {
            return Self::without_streams(output.to_text());
        }

        let mut text = String::new();
        let stdout_range = append_exact_stream(&mut text, stdout.unwrap_or_default());
        let stderr_range = stderr.filter(|stream| !stream.is_empty()).map(|stream| {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str("stderr:\n");
            let start = text.len();
            text.push_str(stream);
            ToolArtifactByteRange {
                start,
                end: text.len(),
            }
        });
        if text.is_empty() || exit_code != 0 {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&format!("exit_code: {exit_code}"));
        }

        Self::new(text, stdout_range, stderr_range)
    }

    fn without_streams(text: String) -> Self {
        Self::new(text, None, None)
    }

    fn new(
        text: String,
        stdout_range: Option<ToolArtifactByteRange>,
        stderr_range: Option<ToolArtifactByteRange>,
    ) -> Self {
        let character_count = measure_characters(&text);
        Self {
            estimated_tokens: estimate_tokens_from_character_count(character_count),
            character_count,
            text,
            stdout_range,
            stderr_range,
        }
    }

    fn into_inline_text(mut self) -> Self {
        self.trim_stream_end(self.stderr_range, true);
        self.trim_stream_end(self.stdout_range, false);
        self.stdout_range = None;
        self.stderr_range = None;
        self.estimated_tokens = estimate_tokens_from_character_count(self.character_count);
        self
    }

    fn trim_stream_end(
        &mut self,
        range: Option<ToolArtifactByteRange>,
        trim_empty_marker_newline: bool,
    ) {
        let Some(range) = range else {
            return;
        };
        let Some(stream) = self.text.get(range.start..range.end) else {
            return;
        };
        let trimmed_stream_bytes = stream.trim_end().len();
        let mut removal_start = range.start.saturating_add(trimmed_stream_bytes);
        if trim_empty_marker_newline && trimmed_stream_bytes == 0 {
            removal_start = range
                .start
                .checked_sub(1)
                .filter(|index| self.text.as_bytes().get(*index) == Some(&b'\n'))
                .unwrap_or(range.start);
        }
        let Some(removed_text) = self.text.get(removal_start..range.end) else {
            return;
        };
        let removed_characters = removed_text.chars().count() as u32;
        self.text.replace_range(removal_start..range.end, "");
        self.character_count = self.character_count.saturating_sub(removed_characters);
    }

    fn estimated_tokens_with_footer(&self, footer: &str) -> u32 {
        let character_count = if self.text.trim().is_empty() {
            footer.chars().count() as u32
        } else {
            self.character_count
                .saturating_add(1)
                .saturating_add(footer.chars().count() as u32)
        };
        estimate_tokens_from_character_count(character_count)
    }

    fn into_text_with_footer(mut self, footer: &str) -> String {
        if self.text.trim().is_empty() {
            return footer.to_string();
        }
        self.text.push('\n');
        self.text.push_str(footer);
        self.text
    }
}

fn append_exact_stream(text: &mut String, stream: &str) -> Option<ToolArtifactByteRange> {
    if stream.is_empty() {
        return None;
    }
    let start = text.len();
    text.push_str(stream);
    Some(ToolArtifactByteRange {
        start,
        end: text.len(),
    })
}

fn replace_with_inline_text(output: &mut ToolOutput, text: String) {
    let process_metadata = output.process_exit_code().map(|exit_code| {
        json!({
            "exit_code": exit_code,
            "stdout_truncated": output.process_stdout_truncated(),
            "stderr_truncated": output.process_stderr_truncated(),
        })
    });
    output.content = vec![ToolContent::Text { text }];
    output.structured = process_metadata;
}

#[cfg(test)]
thread_local! {
    static TEXT_MEASUREMENT_COUNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn measure_characters(text: &str) -> u32 {
    #[cfg(test)]
    TEXT_MEASUREMENT_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    text.chars().count() as u32
}

fn estimate_tokens_from_character_count(character_count: u32) -> u32 {
    if character_count == 0 {
        0
    } else {
        character_count.div_ceil(4)
    }
}

fn estimate_tokens(text: &str) -> u32 {
    estimate_tokens_from_character_count(measure_characters(text))
}

#[cfg(test)]
fn reset_text_measurement_count() {
    TEXT_MEASUREMENT_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
fn text_measurement_count() -> u32 {
    TEXT_MEASUREMENT_COUNT.with(std::cell::Cell::get)
}

fn count_lines(text: &str) -> usize {
    text.lines().count()
}

fn inline_artifact_preview_budget(tool_budget_tokens: u32) -> u32 {
    tool_budget_tokens.div_ceil(4).clamp(256, 1_024)
}

fn artifact_storage_footer(artifact: &ToolOutputArtifact) -> String {
    format!(
        "[full output stored separately: ~{} tokens, {} lines, {} bytes; use tool_result_search first to locate exact matches, then tool_result_read to inspect a narrow span or stream]",
        artifact.estimated_tokens, artifact.line_count, artifact.combined.size
    )
}

fn format_artifact_summary(
    is_error: bool,
    exit_code: Option<i32>,
    available_streams: Vec<&'static str>,
    preview: String,
) -> String {
    let mut lines = Vec::new();
    if is_error {
        lines.push(match exit_code {
            Some(exit_code) => format!(
                "tool failed with exit code {exit_code}; full failure output is stored separately"
            ),
            None => "tool failed; full failure output is stored separately".to_string(),
        });
    }
    if let Some(exit_code) = exit_code {
        lines.push(format!("exit_code: {exit_code}"));
    }
    lines.push(format!(
        "available_streams: {}",
        available_streams.join(", ")
    ));
    lines.push(
        "recovery_hint: use the tool_result id from this message; call tool_result_search for exact patterns, then tool_result_read for a narrow range or a specific stream".to_string(),
    );
    lines.push(preview);
    lines.join("\n")
}

fn truncate_text_for_budget(text: &str, budget_tokens: u32, head_ratio: f64) -> (String, bool) {
    if estimate_tokens(text) <= budget_tokens {
        return (text.to_string(), false);
    }

    let max_chars = budget_tokens.saturating_mul(4) as usize;
    truncate_head_tail(text, max_chars.max(1), head_ratio)
}

fn truncation_footer(original_output_tokens: u32, budget_tokens: u32) -> String {
    format!("[output truncated from ~{original_output_tokens} to ~{budget_tokens} tokens]")
}

fn append_footer(text: &str, footer: &str) -> String {
    if text.trim().is_empty() {
        footer.to_string()
    } else {
        format!("{text}\n{footer}")
    }
}

/// Marker prefix on structure-aware JSON artifact previews.
const JSON_PREVIEW_HEADER: &str = "[json preview: null/empty fields dropped, long arrays \
     elided; the stored artifact holds the full payload]";

/// Builds a structure-aware preview for JSON-shaped outputs, trying
/// progressively harder array elision until the preview fits the budget.
/// Returns `None` for non-JSON outputs or when even the tersest form does not
/// fit, in which case the caller falls back to head/tail truncation.
fn json_compact_preview(output: &ToolOutput, budget_chars: usize) -> Option<String> {
    let value = output.structured_payload()?;

    for max_array_items in [16usize, 4, 1] {
        let compacted = compact_json_value(value, max_array_items);
        let rendered = serde_json::to_string_pretty(&compacted).ok()?;
        let annotated = format!("{JSON_PREVIEW_HEADER}\n{rendered}");
        if annotated.chars().count() <= budget_chars {
            return Some(annotated);
        }
    }
    None
}

/// Recursively drops null/empty fields and elides array tails past
/// `max_array_items`, appending an explicit `[+N more items]` marker so the
/// model knows data was withheld. Deterministic: `serde_json` maps are
/// key-sorted.
fn compact_json_value(value: &serde_json::Value, max_array_items: usize) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, entry)| !is_empty_json_value(entry))
                .map(|(key, entry)| (key.clone(), compact_json_value(entry, max_array_items)))
                .collect(),
        ),
        Value::Array(items) => {
            let mut kept = items
                .iter()
                .take(max_array_items)
                .map(|item| compact_json_value(item, max_array_items))
                .collect::<Vec<_>>();
            if items.len() > max_array_items {
                kept.push(Value::String(format!(
                    "[+{} more items]",
                    items.len() - max_array_items
                )));
            }
            Value::Array(kept)
        }
        other => other.clone(),
    }
}

fn is_empty_json_value(value: &serde_json::Value) -> bool {
    use serde_json::Value;
    match value {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use moa_config::ToolBudgetConfig;
    use moa_core::{
        error::{MoaError, Result},
        events::Event,
        traits::SessionStore,
        types::events_stream::{ClaimCheck, EventFilter, EventRange, EventRecord},
        types::identifiers::{SessionId, TenantId},
        types::session::{SessionFilter, SessionMeta, SessionStatus, SessionSummary},
        types::tools::{ToolDefinition, ToolOutput},
    };
    use serde_json::json;

    use super::super::{ToolRegistry, ToolRouter, local_development_sandbox_policy};
    use super::{
        JSON_PREVIEW_HEADER, json_compact_preview, reset_text_measurement_count,
        text_measurement_count,
    };

    #[derive(Default)]
    struct RecordingSessionStore {
        artifacts: Mutex<Vec<String>>,
    }

    impl RecordingSessionStore {
        fn artifacts(&self) -> Vec<String> {
            self.artifacts
                .lock()
                .expect("recording artifact lock should remain healthy")
                .clone()
        }
    }

    #[async_trait]
    impl SessionStore for RecordingSessionStore {
        async fn create_session(&self, _meta: SessionMeta) -> Result<SessionId> {
            Ok(SessionId::new())
        }

        async fn emit_event(&self, _session_id: SessionId, _event: Event) -> Result<u64> {
            Ok(0)
        }

        async fn store_text_artifact(
            &self,
            _session_id: SessionId,
            text: &str,
        ) -> Result<ClaimCheck> {
            let mut artifacts = self
                .artifacts
                .lock()
                .map_err(|_| MoaError::StorageError("artifact recorder lock poisoned".into()))?;
            let blob_id = format!("artifact-{}", artifacts.len());
            artifacts.push(text.to_string());
            Ok(ClaimCheck {
                blob_id,
                size: text.len(),
                preview: text.chars().take(32).collect(),
            })
        }

        async fn load_text_artifact(
            &self,
            _session_id: SessionId,
            claim_check: &ClaimCheck,
        ) -> Result<String> {
            let index = claim_check
                .blob_id
                .strip_prefix("artifact-")
                .and_then(|raw| raw.parse::<usize>().ok())
                .ok_or_else(|| MoaError::BlobNotFound(claim_check.blob_id.clone()))?;
            self.artifacts
                .lock()
                .map_err(|_| MoaError::StorageError("artifact recorder lock poisoned".into()))?
                .get(index)
                .cloned()
                .ok_or_else(|| MoaError::BlobNotFound(claim_check.blob_id.clone()))
        }

        async fn get_events(
            &self,
            _session_id: SessionId,
            _range: EventRange,
        ) -> Result<Vec<EventRecord>> {
            Ok(Vec::new())
        }

        async fn get_session(&self, _session_id: SessionId) -> Result<SessionMeta> {
            Ok(SessionMeta {
                tenant_id: TenantId::new(),
                status: SessionStatus::Running,
                ..SessionMeta::default()
            })
        }

        async fn update_status(
            &self,
            _session_id: SessionId,
            _status: SessionStatus,
        ) -> Result<()> {
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

        async fn tenant_cost_since(
            &self,
            _tenant_id: &TenantId,
            _since: DateTime<Utc>,
        ) -> Result<u32> {
            Ok(0)
        }

        async fn delete_empty_session(&self, _session_id: SessionId) -> Result<()> {
            Ok(())
        }
    }

    fn bash_definition(max_output_tokens: u32) -> ToolDefinition {
        let registry = ToolRegistry::default_local();
        let mut definition = registry
            .get("bash")
            .expect("default local registry should include bash")
            .clone();
        definition.max_output_tokens = max_output_tokens;
        definition
    }

    #[test]
    fn json_preview_drops_empty_fields_and_elides_long_arrays_within_budget() {
        // Pins: an oversized JSON output's artifact preview keeps structure —
        // empty fields vanish, long arrays elide with an explicit marker —
        // instead of a mid-object head/tail cut.
        let rows = (0..200)
            .map(|index| {
                json!({
                    "id": index,
                    "name": format!("record-{index}"),
                    "notes": null,
                    "labels": [],
                    "description": "",
                })
            })
            .collect::<Vec<_>>();
        let output = ToolOutput::json(
            "200 records",
            json!({ "records": rows, "next_cursor": null }),
            Duration::default(),
        );

        let preview = json_compact_preview(&output, 4_000).expect("json output yields a preview");

        assert!(preview.starts_with(JSON_PREVIEW_HEADER));
        assert!(preview.contains("record-0"), "head items survive");
        assert!(
            preview.contains("more items]"),
            "elision is explicit: {preview}"
        );
        assert!(
            !preview.contains("notes") && !preview.contains("next_cursor"),
            "null/empty fields drop"
        );
        assert!(preview.chars().count() <= 4_000);
    }

    #[test]
    fn json_preview_declines_text_outputs() {
        // Pins: plain-text outputs keep the head/tail fallback.
        let output = ToolOutput::text("x".repeat(50_000), Duration::default());
        assert!(json_compact_preview(&output, 4_000).is_none());
    }

    #[test]
    fn artifact_payload_ranges_preserve_exact_utf8_stream_bytes() {
        // Pins: stdout and stderr remain independently readable from one
        // persisted payload without trimming trailing spaces, tabs, or newlines.
        let output = ToolOutput::from_process(
            "α\nβ \n\n".to_string(),
            "警告\t \n".to_string(),
            0,
            Duration::default(),
        );
        let payload = super::ArtifactPayload::from_output(&output);

        assert_eq!(
            payload
                .stdout_range
                .expect("stdout range")
                .slice(&payload.text)
                .expect("stdout slice"),
            "α\nβ \n\n"
        );
        assert_eq!(
            payload
                .stderr_range
                .expect("stderr range")
                .slice(&payload.text)
                .expect("stderr slice"),
            "警告\t \n"
        );
    }

    #[test]
    fn inline_payload_reuse_matches_stable_process_rendering() {
        // Pins: reusing an exact process payload for inline footer shaping
        // retains the existing per-stream trailing-whitespace semantics.
        let output = ToolOutput::from_process(
            "α\nβ \n\n".to_string(),
            "警告\t \n".to_string(),
            7,
            Duration::default(),
        );

        let inline_text = super::ArtifactPayload::from_output(&output)
            .into_inline_text()
            .text;

        assert_eq!(inline_text, output.to_text());
    }

    #[tokio::test]
    async fn text_budget_preserves_exact_process_payload_when_streams_fit() {
        // Pins: when stream budgets leave a process result unchanged, text
        // budgeting preserves its exact head/tail and original token semantics.
        let router = ToolRouter::new(
            ToolRegistry::default_local(),
            HashMap::new(),
            local_development_sandbox_policy(),
        );
        let stdout = format!("{}{}", "H".repeat(200), "T".repeat(200));

        let output = router
            .apply_output_budget(
                &SessionMeta::default(),
                &bash_definition(32),
                ToolOutput::from_process(stdout, String::new(), 0, Duration::default()),
            )
            .await;

        let expected = format!(
            "{}\n[... ~354 chars omitted ...]\n{}\n\
             [output truncated from ~100 to ~32 tokens]",
            "H".repeat(18),
            "T".repeat(28),
        );
        assert_eq!(output.to_text(), expected);
        assert!(!output.is_error);
        assert!(output.truncated);
        assert_eq!(output.original_output_tokens, Some(100));
        assert_eq!(output.process_exit_code(), Some(0));
        assert_eq!(output.process_stdout(), None);
        assert_eq!(output.process_stderr(), None);
        assert!(!output.process_stdout_truncated());
        assert!(!output.process_stderr_truncated());
        assert_eq!(
            output.structured.as_ref(),
            Some(&json!({
                "exit_code": 0,
                "stdout_truncated": false,
                "stderr_truncated": false,
            }))
        );
        assert_eq!(output.artifact, None);
    }

    #[tokio::test]
    async fn stream_truncated_payload_and_measurement_are_reused_for_footer() {
        // Pins: stream truncation is not undone when the original process
        // payload exceeded the text budget but the stream-budgeted payload fits;
        // footer shaping does not render or measure that selected payload again.
        let router = ToolRouter::new(
            ToolRegistry::default_local(),
            HashMap::new(),
            local_development_sandbox_policy(),
        )
        .with_tool_budgets(ToolBudgetConfig {
            bash_stdout: 16,
            bash_stderr: 16,
            ..ToolBudgetConfig::default()
        });
        let stdout = format!("{}{}", "H".repeat(200), "T".repeat(200));
        reset_text_measurement_count();

        let output = router
            .apply_output_budget(
                &SessionMeta::default(),
                &bash_definition(64),
                ToolOutput::from_process(stdout, String::new(), 0, Duration::default()),
            )
            .await;

        assert_eq!(
            text_measurement_count(),
            4,
            "original payload, stdout, stderr, and changed payload are each measured once"
        );
        let expected = format!(
            "{}\n[... ~376 chars omitted ...]\n{}\n\
             [output truncated from ~100 to ~64 tokens]",
            "H".repeat(10),
            "T".repeat(14),
        );
        assert_eq!(output.to_text(), expected);
        assert!(!output.is_error);
        assert!(output.truncated);
        assert_eq!(output.original_output_tokens, Some(100));
        assert_eq!(output.process_exit_code(), Some(0));
        assert_eq!(output.process_stdout(), None);
        assert_eq!(output.process_stderr(), None);
        assert!(output.process_stdout_truncated());
        assert!(!output.process_stderr_truncated());
        assert_eq!(
            output.structured.as_ref(),
            Some(&json!({
                "exit_code": 0,
                "stdout_truncated": true,
                "stderr_truncated": false,
            }))
        );
        assert_eq!(output.artifact, None);
    }

    #[tokio::test]
    async fn oversized_process_error_persists_one_blob_and_keeps_failure_metadata() {
        // Pins: errors pass through the same artifact budget as successes. One
        // exact process payload is stored, while the inline result remains an
        // error with its exit code, duration, and bounded recovery summary.
        let store = Arc::new(RecordingSessionStore::default());
        let router = ToolRouter::new(
            ToolRegistry::default_local(),
            HashMap::new(),
            local_development_sandbox_policy(),
        )
        .with_session_store(store.clone());
        let session = SessionMeta::default();
        let stdout = format!("{} \n\n", "out".repeat(400));
        let stderr = format!("{}\t \n", "failure".repeat(300));
        let duration = Duration::from_millis(37);

        let output = router
            .apply_output_budget(
                &session,
                &bash_definition(64),
                ToolOutput::from_process(stdout.clone(), stderr.clone(), 23, duration),
            )
            .await;

        let stored = store.artifacts();
        assert_eq!(stored.len(), 1, "one oversized result must store one blob");
        let artifact = output.artifact.as_ref().expect("artifact metadata");
        assert_eq!(artifact.combined.blob_id, "artifact-0");
        assert_eq!(artifact.stdout, None, "new artifacts have no stdout blob");
        assert_eq!(artifact.stderr, None, "new artifacts have no stderr blob");
        assert_eq!(
            artifact
                .slice_stream(
                    moa_core::types::tools::ToolArtifactStream::Stdout,
                    &stored[0],
                )
                .expect("valid stdout range"),
            Some(stdout.as_str())
        );
        assert_eq!(
            artifact
                .slice_stream(
                    moa_core::types::tools::ToolArtifactStream::Stderr,
                    &stored[0],
                )
                .expect("valid stderr range"),
            Some(stderr.as_str())
        );
        assert!(output.is_error);
        assert_eq!(output.process_exit_code(), Some(23));
        assert_eq!(output.duration, duration);
        assert!(output.truncated);
        assert!(output.original_output_tokens.is_some());
        assert!(output.to_text().starts_with(
            "tool failed with exit code 23; full failure output is stored separately"
        ));
    }

    #[tokio::test]
    async fn oversized_process_error_without_store_is_bounded_inline() {
        // Pins: deployments without claim-check storage still bound failure
        // output and retain the status metadata needed by replay and callers.
        let router = ToolRouter::new(
            ToolRegistry::default_local(),
            HashMap::new(),
            local_development_sandbox_policy(),
        );
        let duration = Duration::from_millis(41);

        let output = router
            .apply_output_budget(
                &SessionMeta::default(),
                &bash_definition(128),
                ToolOutput::from_process(
                    "stdout".repeat(2_000),
                    "stderr".repeat(2_000),
                    29,
                    duration,
                ),
            )
            .await;

        assert!(output.is_error);
        assert_eq!(output.process_exit_code(), Some(29));
        assert_eq!(output.duration, duration);
        assert!(output.truncated);
        assert!(output.artifact.is_none());
        assert!(output.original_output_tokens.is_some());
        assert!(
            output.process_stdout().is_none(),
            "raw stdout must be dropped"
        );
        assert!(
            output.process_stderr().is_none(),
            "raw stderr must be dropped"
        );
        assert!(
            super::estimate_tokens(&output.to_text()) <= 128,
            "inline failure must obey the declared output budget"
        );
    }
}
