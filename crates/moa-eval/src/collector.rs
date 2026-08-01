//! Event-log collection helpers for eval trajectories, responses, evidence, and
//! aggregate metrics.
//!
//! The collector produces two different things from one event stream, and the
//! difference matters. [`TrajectoryStep`]s are a lossy human-readable path
//! summary used for triage. The [`EvidenceEnvelope`] is the typed, ordered
//! record that assertions are actually settled against: invocations with their
//! real arguments and outcomes, approval requests and decisions in the order
//! they happened, conversation history, and lineage references.
//!
//! When content capture is disabled the envelope is marked truncated, because
//! evidence assembled from blanked-out payloads must fail closed rather than
//! quietly assert nothing.

use moa_core::{
    events::Event, types::action_policy::ActionReviewDecision, types::events_stream::EventRecord,
    types::identifiers::ToolCallId, types::model::TokenPricing,
};
use moa_eval_core::evidence::{
    ActionKind, ActionOutcome, EvidenceEnvelope, EvidenceSubject, HistoryRole,
};
use moa_eval_core::{EvalMetrics, TrajectoryStep};
use std::collections::HashMap;
use uuid::Uuid;

/// Collected response, trajectory, evidence, and metrics extracted from
/// persisted session events.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct CollectedExecution {
    /// Final aggregated assistant response text, when content capture is enabled.
    pub response: Option<String>,
    /// Observed tool-call trajectory.
    pub trajectory: Vec<TrajectoryStep>,
    /// Ordered evidence entries awaiting a run subject.
    pub evidence: Vec<EvidenceEntry>,
    /// Whether the capture was complete.
    pub capture_truncated: Option<String>,
    /// Aggregate usage and latency metrics.
    pub metrics: EvalMetrics,
}

impl CollectedExecution {
    /// Builds the versioned evidence envelope for one run subject.
    pub fn to_evidence(&self, subject: EvidenceSubject) -> EvidenceEnvelope {
        let mut builder = EvidenceEnvelope::builder(subject).source("session_event_log");
        if let Some(reason) = &self.capture_truncated {
            builder = builder.truncated(reason.clone());
        }
        for entry in &self.evidence {
            builder = match entry {
                EvidenceEntry::Action {
                    kind,
                    name,
                    arguments,
                    outcome,
                } => builder.action(*kind, name.clone(), arguments.clone(), *outcome),
                EvidenceEntry::History { role, text } => builder.history(*role, text.clone()),
                EvidenceEntry::Lineage { kind, reference } => {
                    builder.lineage(kind.clone(), reference.clone())
                }
            };
        }
        if let Some(response) = &self.response {
            builder = builder.response(response.clone());
        }
        builder.build()
    }
}

/// One ordered observation destined for the evidence envelope.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EvidenceEntry {
    /// An invocation or approval fact.
    Action {
        /// What the entry represents.
        kind: ActionKind,
        /// Action or approval subject name.
        name: String,
        /// Structured arguments.
        arguments: serde_json::Value,
        /// Terminal outcome.
        outcome: ActionOutcome,
    },
    /// A conversation record.
    History {
        /// Speaker role.
        role: HistoryRole,
        /// Recorded text.
        text: String,
    },
    /// A lineage reference.
    Lineage {
        /// Lineage category.
        kind: String,
        /// Stable reference.
        reference: String,
    },
}

/// Aggregates persisted session events into eval-friendly execution artifacts.
#[derive(Debug, Clone)]
pub struct TrajectoryCollector {
    steps: Vec<TrajectoryStep>,
    tool_indices: HashMap<ToolCallId, usize>,
    evidence: Vec<EvidenceEntry>,
    evidence_indices: HashMap<ToolCallId, usize>,
    review_tools: HashMap<Uuid, String>,
    final_response: Option<String>,
    metrics: EvalMetrics,
    pricing: Option<TokenPricing>,
    capture_content: bool,
    content_max_bytes: usize,
    content_truncated: bool,
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
            evidence: Vec::new(),
            evidence_indices: HashMap::new(),
            review_tools: HashMap::new(),
            final_response: None,
            metrics: EvalMetrics::default(),
            pricing,
            capture_content,
            content_max_bytes,
            content_truncated: false,
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
                let input_summary = self.render_json(input);
                self.steps.push(TrajectoryStep {
                    tool_name: tool_name.clone(),
                    input_summary,
                    output_summary: String::new(),
                    success: false,
                    duration_ms: 0,
                });
                // The evidence ledger keeps the *real* arguments, not a
                // truncated preview, because action assertions match on them.
                self.evidence_indices.insert(*tool_id, self.evidence.len());
                self.evidence.push(EvidenceEntry::Action {
                    kind: ActionKind::Invocation,
                    name: tool_name.clone(),
                    arguments: input.clone(),
                    outcome: ActionOutcome::Failed,
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
                self.set_action_outcome(
                    tool_id,
                    if *success {
                        ActionOutcome::Succeeded
                    } else {
                        ActionOutcome::Failed
                    },
                );
            }
            Event::ToolError { tool_id, error, .. } => {
                let step_index = self.ensure_step(tool_id);
                let output_summary = self.render_text(error);
                if let Some(step) = self.steps.get_mut(step_index) {
                    step.output_summary = output_summary;
                    step.success = false;
                }
                self.set_action_outcome(tool_id, ActionOutcome::Failed);
                self.metrics.tool_error_count += 1;
            }
            Event::ActionReviewRequested {
                review_id,
                envelope,
                ..
            } => {
                self.review_tools
                    .insert(*review_id, envelope.tool_name.clone());
                self.evidence.push(EvidenceEntry::Action {
                    kind: ActionKind::ApprovalRequested,
                    name: envelope.tool_name.clone(),
                    arguments: serde_json::json!({ "review_id": review_id }),
                    outcome: ActionOutcome::Recorded,
                });
            }
            Event::ActionReviewDecided {
                review_id,
                decision,
                ..
            } => {
                let name = self
                    .review_tools
                    .get(review_id)
                    .cloned()
                    .unwrap_or_else(|| review_id.to_string());
                self.evidence.push(EvidenceEntry::Action {
                    kind: match decision {
                        ActionReviewDecision::Cleared => ActionKind::ApprovalGranted,
                        ActionReviewDecision::Denied { .. } => ActionKind::ApprovalDenied,
                    },
                    name,
                    arguments: serde_json::json!({ "review_id": review_id }),
                    outcome: ActionOutcome::Recorded,
                });
            }
            Event::ActionReviewTimedOut { review_id, .. } => {
                let name = self
                    .review_tools
                    .get(review_id)
                    .cloned()
                    .unwrap_or_else(|| review_id.to_string());
                // A review that expired is not an approval. Recording it as a
                // denial keeps an approval-ordering assertion fail-closed.
                self.evidence.push(EvidenceEntry::Action {
                    kind: ActionKind::ApprovalDenied,
                    name,
                    arguments: serde_json::json!({ "review_id": review_id, "timed_out": true }),
                    outcome: ActionOutcome::Recorded,
                });
            }
            Event::UserMessage { text, .. } => {
                if self.capture_content {
                    let text = self.capture_text(text);
                    self.evidence.push(EvidenceEntry::History {
                        role: HistoryRole::User,
                        text,
                    });
                }
            }
            Event::MemoryRead { path, .. } => {
                self.evidence.push(EvidenceEntry::Lineage {
                    kind: "memory_read".to_string(),
                    reference: path.clone(),
                });
            }
            Event::MemoryWrite { path, .. } => {
                self.evidence.push(EvidenceEntry::Lineage {
                    kind: "memory_write".to_string(),
                    reference: path.clone(),
                });
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
                    let text = self.capture_text(text);
                    self.final_response = Some(text.clone());
                    self.evidence.push(EvidenceEntry::History {
                        role: HistoryRole::Assistant,
                        text,
                    });
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

    fn set_action_outcome(&mut self, tool_id: &ToolCallId, outcome: ActionOutcome) {
        let Some(index) = self.evidence_indices.get(tool_id).copied() else {
            return;
        };
        if let Some(EvidenceEntry::Action {
            outcome: recorded, ..
        }) = self.evidence.get_mut(index)
        {
            *recorded = outcome;
        }
    }

    /// Processes a complete ordered event stream.
    pub fn process_events(&mut self, events: &[EventRecord]) {
        for record in events {
            self.process_event(&record.event);
        }
    }

    /// Consumes the complete event-log capture into assertion evidence.
    ///
    /// Production Behavior Lab release trials use this narrow surface after the
    /// target stops and before scoring, so they reuse the same ledger semantics
    /// as the internal regression harness without exposing its report payload.
    #[must_use]
    pub fn into_evidence(self, subject: EvidenceSubject) -> EvidenceEnvelope {
        self.finish().to_evidence(subject)
    }

    /// Consumes the collector and returns the final collected execution payload.
    pub(crate) fn finish(self) -> CollectedExecution {
        // Content capture off means the response and history observations were
        // never recorded. That is a partial view, and the envelope says so.
        let capture_truncated = if !self.capture_content {
            Some(
                "content capture is disabled, so response and history observations were not recorded"
                    .to_string(),
            )
        } else if self.content_truncated {
            Some(
                "one or more captured content payloads exceeded content_max_bytes and were truncated"
                    .to_string(),
            )
        } else {
            None
        };
        CollectedExecution {
            response: self.final_response,
            trajectory: self.steps,
            evidence: self.evidence,
            capture_truncated,
            metrics: self.metrics,
        }
    }

    fn ensure_step(&mut self, tool_id: &ToolCallId) -> usize {
        if let Some(index) = self.tool_indices.get(tool_id) {
            return *index;
        }

        let index = self.steps.len();
        self.tool_indices.insert(*tool_id, index);
        self.steps.push(TrajectoryStep::default());
        index
    }

    fn render_json(&mut self, value: &serde_json::Value) -> String {
        if !self.capture_content {
            return String::new();
        }

        let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
        self.capture_text(&text)
    }

    fn render_text(&mut self, value: &str) -> String {
        if !self.capture_content {
            return String::new();
        }

        self.capture_text(value)
    }

    fn capture_text(&mut self, value: &str) -> String {
        if self.content_max_bytes != 0 && value.len() > self.content_max_bytes {
            self.content_truncated = true;
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
    use chrono::Utc;
    use std::time::Duration;

    use moa_core::{
        events::Event, types::identifiers::ToolCallId, types::model::TokenPricing,
        types::tools::ToolOutput,
    };
    use moa_eval_core::{
        TestCase,
        assertion::{
            AssertionCategory, AssertionSpec, EvaluatorRef, GateEffect, builtin_registry,
            evaluate_assertions,
        },
        evidence::EvidenceSubject,
    };
    use serde_json::json;

    use super::TrajectoryCollector;

    #[test]
    fn collector_tracks_tool_steps_and_metrics() {
        let tool_id = ToolCallId::new();
        let mut collector = TrajectoryCollector::new(
            Some(TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: None,
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
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
            original_output_tokens: None,
            success: true,
            duration_ms: 5,
            assessment: moa_core::types::security::ToolOutputAssessment::safe(),
            capability: moa_core::types::security::ToolCapabilityId::builtin("bash"),
        });
        collector.process_event(&Event::BrainResponse {
            text: "done".to_string(),
            thought_signature: None,
            model: moa_core::types::identifiers::ModelId::new("mock"),
            model_tier: moa_core::types::provider::ModelTier::Main,
            input_tokens_uncached: 100,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 50,
            cost_cents: 0,
            duration_ms: 12,
            llm_ttft_ms: None,
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

    #[test]
    fn collector_uses_the_latest_brain_response_as_the_final_response() {
        // Pins: multi-turn histories retain every assistant message, while
        // final-response assertions evaluate the same last response surfaced by
        // production trial terminal evidence.
        let mut collector = TrajectoryCollector::new(None, true, 1_024);
        for text in ["intermediate", "final"] {
            collector.process_event(&Event::BrainResponse {
                text: text.to_string(),
                thought_signature: None,
                model: moa_core::types::identifiers::ModelId::new("mock"),
                model_tier: moa_core::types::provider::ModelTier::Main,
                input_tokens_uncached: 0,
                input_tokens_cache_write: 0,
                input_tokens_cache_read: 0,
                output_tokens: 0,
                cost_cents: 0,
                duration_ms: 0,
                llm_ttft_ms: None,
            });
        }

        let collected = collector.finish();
        assert_eq!(collected.response.as_deref(), Some("final"));
        assert_eq!(
            collected
                .evidence
                .iter()
                .filter(|entry| matches!(
                    entry,
                    super::EvidenceEntry::History {
                        role: moa_eval_core::evidence::HistoryRole::Assistant,
                        ..
                    }
                ))
                .count(),
            2
        );
    }

    #[test]
    fn truncated_forbidden_tail_fails_assertions_closed() {
        // Pins: a forbidden phrase beyond the capture boundary cannot disappear
        // and turn a blocking not-contains assertion into a pass.
        let forbidden = "forbidden-tail";
        let response = format!("{}{forbidden}", "safe-prefix ".repeat(64));
        let mut collector = TrajectoryCollector::new(None, true, 64);
        collector.process_event(&Event::BrainResponse {
            text: response,
            thought_signature: None,
            model: moa_core::types::identifiers::ModelId::new("mock"),
            model_tier: moa_core::types::provider::ModelTier::Main,
            input_tokens_uncached: 0,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 0,
            cost_cents: 0,
            duration_ms: 0,
            llm_ttft_ms: None,
        });
        let collected = collector.finish();
        assert!(
            !collected
                .response
                .as_deref()
                .expect("response was captured")
                .contains(forbidden)
        );

        let evidence = collected.to_evidence(EvidenceSubject {
            case: "forbidden-tail".to_string(),
            case_schema_version: moa_eval_core::types::TEST_CASE_SCHEMA_VERSION,
            agent_config: "mock".to_string(),
            run_label: "1".to_string(),
        });
        let case = TestCase {
            name: "forbidden-tail".to_string(),
            assertions: vec![AssertionSpec {
                id: "no-forbidden-tail".to_string(),
                category: AssertionCategory::Communication,
                gate_effect: GateEffect::Blocking,
                evaluator: EvaluatorRef::deterministic("text_match", 1),
                config: json!({ "not_contains": [forbidden] }),
            }],
            ..TestCase::default()
        };

        let outcomes = evaluate_assertions(builtin_registry(), &case, Some(&evidence));
        assert_eq!(outcomes.len(), 1);
        assert!(!outcomes[0].passed);
        assert!(outcomes[0].diagnostic.contains("evidence is truncated"));
    }

    #[test]
    fn queued_message_is_not_duplicated_as_user_history() {
        // Pins: enqueueing and later delivering one message records one user turn.
        let text = "hello".to_string();
        let mut collector = TrajectoryCollector::new(None, true, 1_024);
        collector.process_event(&Event::QueuedMessage {
            text: text.clone(),
            attachments: Vec::new(),
            queued_at: Utc::now(),
        });
        collector.process_event(&Event::UserMessage {
            text,
            attachments: Vec::new(),
        });

        let collected = collector.finish();
        let user_entries = collected
            .evidence
            .iter()
            .filter(|entry| {
                matches!(
                    entry,
                    super::EvidenceEntry::History {
                        role: moa_eval_core::evidence::HistoryRole::User,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(user_entries, 1);
    }
}
