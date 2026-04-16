//! Event-log collection helpers for eval trajectories, responses, and aggregate metrics.

use moa_core::{Event, EventRecord, TokenPricing};
use std::collections::HashMap;
use uuid::Uuid;

use crate::{EvalMetrics, TrajectoryStep};

/// Collected response, trajectory, and metrics extracted from persisted session events.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct CollectedExecution {
    /// Final aggregated assistant response text, when content capture is enabled.
    pub response: Option<String>,
    /// Observed tool-call trajectory.
    pub trajectory: Vec<TrajectoryStep>,
    /// Aggregate usage and latency metrics.
    pub metrics: EvalMetrics,
}

/// Aggregates persisted session events into eval-friendly execution artifacts.
#[derive(Debug, Clone)]
pub struct TrajectoryCollector {
    steps: Vec<TrajectoryStep>,
    tool_indices: HashMap<Uuid, usize>,
    response_chunks: Vec<String>,
    metrics: EvalMetrics,
    pricing: Option<TokenPricing>,
    capture_content: bool,
    content_max_bytes: usize,
}

impl TrajectoryCollector {
    /// Creates a new collector.
    pub fn new(
        pricing: Option<TokenPricing>,
        capture_content: bool,
        content_max_bytes: usize,
    ) -> Self {
        Self {
            steps: Vec::new(),
            tool_indices: HashMap::new(),
            response_chunks: Vec::new(),
            metrics: EvalMetrics::default(),
            pricing,
            capture_content,
            content_max_bytes,
        }
    }

    /// Processes one event emitted during the eval run.
    pub fn process_event(&mut self, event: &Event) {
        match event {
            Event::ToolCall {
                tool_id,
                tool_name,
                input,
                ..
            } => {
                let step_index = self.steps.len();
                self.tool_indices.insert(*tool_id, step_index);
                self.steps.push(TrajectoryStep {
                    tool_name: tool_name.clone(),
                    input_summary: self.render_json(input),
                    output_summary: String::new(),
                    success: false,
                    duration_ms: 0,
                });
                self.metrics.tool_call_count += 1;
            }
            Event::ToolResult {
                tool_id,
                output,
                success,
                duration_ms,
                ..
            } => {
                let step_index = self.ensure_step(tool_id);
                let output_summary = self.render_text(&output.to_text());
                if let Some(step) = self.steps.get_mut(step_index) {
                    step.output_summary = output_summary;
                    step.success = *success;
                    step.duration_ms = *duration_ms;
                }
            }
            Event::ToolError { tool_id, error, .. } => {
                let step_index = self.ensure_step(tool_id);
                let output_summary = self.render_text(error);
                if let Some(step) = self.steps.get_mut(step_index) {
                    step.output_summary = output_summary;
                    step.success = false;
                }
                self.metrics.tool_error_count += 1;
            }
            Event::BrainResponse {
                text,
                output_tokens,
                cost_cents,
                duration_ms,
                ..
            } => {
                let input_tokens = event.input_tokens();
                if self.capture_content && !text.trim().is_empty() {
                    self.response_chunks
                        .push(truncate(text, self.content_max_bytes));
                }
                self.metrics.input_tokens += input_tokens;
                self.metrics.output_tokens += *output_tokens;
                self.metrics.total_tokens += input_tokens + output_tokens;
                self.metrics.latency_ms += *duration_ms;
                self.metrics.turn_count += 1;
                self.metrics.cost_dollars += if *cost_cents > 0 {
                    *cost_cents as f64 / 100.0
                } else {
                    estimate_cost(self.pricing.as_ref(), input_tokens, *output_tokens)
                };
            }
            _ => {}
        }
    }

    /// Processes a complete ordered event stream.
    pub fn process_events(&mut self, events: &[EventRecord]) {
        for record in events {
            self.process_event(&record.event);
        }
    }

    /// Returns a snapshot of the collected steps.
    pub fn steps(&self) -> &[TrajectoryStep] {
        &self.steps
    }

    /// Returns the current aggregate metrics.
    pub fn metrics(&self) -> &EvalMetrics {
        &self.metrics
    }

    /// Returns the aggregated assistant response text, if any.
    pub fn response(&self) -> Option<String> {
        if self.response_chunks.is_empty() {
            None
        } else {
            Some(self.response_chunks.join("\n\n"))
        }
    }

    /// Consumes the collector and returns the final collected execution payload.
    pub(crate) fn finish(self) -> CollectedExecution {
        CollectedExecution {
            response: if self.response_chunks.is_empty() {
                None
            } else {
                Some(self.response_chunks.join("\n\n"))
            },
            trajectory: self.steps,
            metrics: self.metrics,
        }
    }

    fn ensure_step(&mut self, tool_id: &Uuid) -> usize {
        if let Some(index) = self.tool_indices.get(tool_id) {
            return *index;
        }

        let index = self.steps.len();
        self.tool_indices.insert(*tool_id, index);
        self.steps.push(TrajectoryStep::default());
        index
    }

    fn render_json(&self, value: &serde_json::Value) -> String {
        if !self.capture_content {
            return String::new();
        }

        let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
        truncate(&text, self.content_max_bytes)
    }

    fn render_text(&self, value: &str) -> String {
        if !self.capture_content {
            return String::new();
        }

        truncate(value, self.content_max_bytes)
    }
}

fn estimate_cost(pricing: Option<&TokenPricing>, input_tokens: usize, output_tokens: usize) -> f64 {
    let Some(pricing) = pricing else {
        return 0.0;
    };

    ((input_tokens as f64 * pricing.input_per_mtok)
        + (output_tokens as f64 * pricing.output_per_mtok))
        / 1_000_000.0
}

fn truncate(text: &str, max_bytes: usize) -> String {
    if max_bytes == 0 || text.len() <= max_bytes {
        return text.to_string();
    }

    let mut boundary = 0usize;
    for (index, _) in text.char_indices() {
        if index <= max_bytes.saturating_sub(3) {
            boundary = index;
        } else {
            break;
        }
    }

    if boundary == 0 {
        "...".to_string()
    } else {
        format!("{}...", &text[..boundary])
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moa_core::{Event, TokenPricing, ToolOutput};
    use serde_json::json;
    use uuid::Uuid;

    use super::TrajectoryCollector;

    #[test]
    fn collector_tracks_tool_steps_and_metrics() {
        let tool_id = Uuid::now_v7();
        let mut collector = TrajectoryCollector::new(
            Some(TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: None,
            }),
            true,
            1_024,
        );

        collector.process_event(&Event::ToolCall {
            tool_id,
            provider_tool_use_id: None,
            provider_thought_signature: None,
            tool_name: "bash".to_string(),
            input: json!({ "cmd": "ls" }),
            hand_id: None,
        });
        collector.process_event(&Event::ToolResult {
            tool_id,
            provider_tool_use_id: None,
            output: ToolOutput::text("file1\nfile2", Duration::from_millis(5)),
            success: true,
            duration_ms: 5,
        });
        collector.process_event(&Event::BrainResponse {
            text: "done".to_string(),
            thought_signature: None,
            model: "mock".to_string(),
            input_tokens_uncached: 100,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 50,
            cost_cents: 0,
            duration_ms: 12,
        });

        let collected = collector.finish();
        assert_eq!(collected.trajectory.len(), 1);
        assert_eq!(collected.trajectory[0].tool_name, "bash");
        assert!(collected.trajectory[0].success);
        assert_eq!(collected.metrics.tool_call_count, 1);
        assert_eq!(collected.metrics.total_tokens, 150);
        assert!(collected.metrics.cost_dollars > 0.0);
        assert_eq!(collected.response.as_deref(), Some("done"));
    }
}
