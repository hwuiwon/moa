//! Tool output budgeting, truncation, and claim-check artifactization.

use moa_core::{
    SessionMeta, ToolBudgetConfig, ToolContent, ToolDefinition, ToolOutput, ToolOutputArtifact,
    ToolOutputConfig, truncate_head_tail,
};
use moa_observability::record_tool_output_truncated_metric;
use serde_json::json;

use super::ToolRouter;

impl ToolRouter {
    /// Overrides the router's replay truncation settings used for head/tail shaping.
    #[must_use]
    pub fn with_tool_output_config(mut self, tool_output: ToolOutputConfig) -> Self {
        self.tool_output = tool_output;
        self
    }

    /// Overrides the router's per-tool output budgets.
    #[must_use]
    pub fn with_tool_budgets(mut self, tool_budgets: ToolBudgetConfig) -> Self {
        self.registry.apply_budgets(&tool_budgets);
        self.tool_budgets = tool_budgets;
        self
    }

    pub(super) async fn apply_output_budget(
        &self,
        session: &SessionMeta,
        tool_definition: &ToolDefinition,
        output: ToolOutput,
    ) -> ToolOutput {
        if output.is_error {
            if output.truncated {
                return output;
            }
            return output.with_original_output_tokens(None);
        }

        let existing_truncated = output.truncated;
        let existing_original_output_tokens = output.original_output_tokens;
        // Render the original output once and share it with the artifact path
        // instead of re-rendering the full output via `to_text()` twice.
        let rendered = output.to_text();
        let original_output_tokens = estimate_tokens(&rendered);
        if let Some(artifactized_output) = self
            .artifactize_output(
                session,
                tool_definition,
                &output,
                &rendered,
                original_output_tokens,
            )
            .await
        {
            record_tool_output_truncated_metric(&tool_definition.name);
            return artifactized_output.with_truncated(true);
        }

        let (stream_budgeted_output, stream_truncated) =
            self.apply_stream_budget(tool_definition, output);

        let (mut final_output, text_truncated) = self.apply_text_budget(
            tool_definition,
            original_output_tokens,
            stream_budgeted_output,
        );
        let router_truncated = stream_truncated || text_truncated;
        let truncated = existing_truncated || router_truncated;
        if router_truncated && !text_truncated {
            let footer =
                truncation_footer(original_output_tokens, tool_definition.max_output_tokens);
            let rendered = final_output.to_text();
            let with_footer = append_footer(&rendered, &footer);
            if estimate_tokens(&with_footer) > tool_definition.max_output_tokens {
                let available_chars = tool_definition
                    .max_output_tokens
                    .saturating_mul(4)
                    .saturating_sub(footer.chars().count() as u32)
                    as usize;
                let (truncated_text, _) = truncate_head_tail(
                    &rendered,
                    available_chars.max(1),
                    self.tool_output.head_ratio,
                );
                final_output.content = vec![ToolContent::Text {
                    text: append_footer(&truncated_text, &footer),
                }];
                final_output.structured = None;
            } else {
                final_output.content = vec![ToolContent::Text { text: with_footer }];
            }
        }
        final_output.truncated = truncated;
        final_output.original_output_tokens = if router_truncated {
            Some(original_output_tokens)
        } else {
            existing_original_output_tokens
        };

        if router_truncated {
            record_tool_output_truncated_metric(&tool_definition.name);
        }

        final_output
    }

    async fn artifactize_output(
        &self,
        session: &SessionMeta,
        tool_definition: &ToolDefinition,
        output: &ToolOutput,
        rendered: &str,
        original_output_tokens: u32,
    ) -> Option<ToolOutput> {
        if original_output_tokens <= tool_definition.max_output_tokens {
            return None;
        }

        let session_store = self.session_store.as_ref()?;

        let combined = match session_store
            .store_text_artifact(session.id, rendered)
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
        let stdout = match output.process_stdout() {
            Some(stdout) if !stdout.is_empty() => {
                match session_store.store_text_artifact(session.id, stdout).await {
                    Ok(claim_check) => Some(claim_check),
                    Err(error) => {
                        tracing::warn!(
                            session_id = %session.id,
                            tool_name = %tool_definition.name,
                            error = %error,
                            "failed to persist tool stdout artifact; continuing with combined artifact only"
                        );
                        None
                    }
                }
            }
            _ => None,
        };
        let stderr = match output.process_stderr() {
            Some(stderr) if !stderr.is_empty() => {
                match session_store.store_text_artifact(session.id, stderr).await {
                    Ok(claim_check) => Some(claim_check),
                    Err(error) => {
                        tracing::warn!(
                            session_id = %session.id,
                            tool_name = %tool_definition.name,
                            error = %error,
                            "failed to persist tool stderr artifact; continuing with combined artifact only"
                        );
                        None
                    }
                }
            }
            _ => None,
        };

        let artifact = ToolOutputArtifact {
            combined,
            estimated_tokens: original_output_tokens,
            line_count: count_lines(rendered),
            stdout,
            stderr,
        };
        let inline_preview_tokens =
            inline_artifact_preview_budget(tool_definition.max_output_tokens);
        let preview_footer = artifact_storage_footer(&artifact);
        let preview_budget_chars = inline_preview_tokens
            .saturating_mul(4)
            .saturating_sub(preview_footer.chars().count() as u32)
            as usize;
        let (preview, _) = truncate_head_tail(
            rendered,
            preview_budget_chars.max(1),
            self.tool_output.head_ratio,
        );
        let summary = format_artifact_summary(
            output.process_exit_code(),
            artifact.available_streams(),
            append_footer(&preview, &preview_footer),
        );

        Some(ToolOutput {
            content: vec![ToolContent::Text { text: summary }],
            is_error: false,
            structured: Some(json!({
                "artifact_available": true,
                "estimated_tokens": artifact.estimated_tokens,
                "line_count": artifact.line_count,
                "available_streams": artifact.available_streams(),
                "exit_code": output.process_exit_code(),
            })),
            duration: output.duration,
            truncated: true,
            original_output_tokens: Some(original_output_tokens),
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

        let stdout_budget = self.tool_budgets.bash_stdout;
        let stderr_budget = self.tool_budgets.bash_stderr;
        let (stdout, stdout_truncated) =
            truncate_text_for_budget(stdout, stdout_budget, self.tool_output.head_ratio);
        let (stderr, stderr_truncated) =
            truncate_text_for_budget(stderr, stderr_budget, self.tool_output.head_ratio);

        if !stdout_truncated && !stderr_truncated {
            return (output, false);
        }

        (
            ToolOutput::from_process(stdout, stderr, exit_code, output.duration),
            true,
        )
    }

    fn apply_text_budget(
        &self,
        tool_definition: &ToolDefinition,
        original_output_tokens: u32,
        output: ToolOutput,
    ) -> (ToolOutput, bool) {
        let rendered = output.to_text();
        let budget = tool_definition.max_output_tokens;
        if estimate_tokens(&rendered) <= budget {
            return (output, false);
        }

        let footer = truncation_footer(original_output_tokens, budget);
        let available_chars = budget
            .saturating_mul(4)
            .saturating_sub(footer.chars().count() as u32) as usize;
        let available_chars = available_chars.max(1);
        let (truncated_text, _) =
            truncate_head_tail(&rendered, available_chars, self.tool_output.head_ratio);

        (
            ToolOutput {
                content: vec![ToolContent::Text {
                    text: append_footer(&truncated_text, &footer),
                }],
                structured: None,
                ..output
            },
            true,
        )
    }
}

fn estimate_tokens(text: &str) -> u32 {
    let char_count = text.chars().count() as u32;
    if char_count == 0 {
        0
    } else {
        char_count.div_ceil(4)
    }
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
    exit_code: Option<i32>,
    available_streams: Vec<&'static str>,
    preview: String,
) -> String {
    let mut lines = Vec::new();
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
