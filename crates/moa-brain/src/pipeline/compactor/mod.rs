//! Stage 9: applies tiered context compaction to compiled history.

mod deterministic;
mod report;
mod snapshot;
mod summarize;
#[cfg(test)]
mod test_support;
mod triggers;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use moa_core::{
    CompactionConfig, ContextProcessor, LLMProvider, ProcessorOutput, Result, SessionStore,
    WorkingContext,
};
use serde_json::json;

use crate::pipeline::history::HISTORY_END_INDEX_METADATA_KEY;

use self::deterministic::{apply_tier1, apply_tier2};
use self::report::{CompactionReport, CompactionTier};
use self::snapshot::{collapse_snapshot_for_tier2, load_snapshot, store_snapshot};
use self::summarize::apply_tier3;
use self::triggers::{
    history_bounds, protected_snapshot_tool_use_ids, recent_turn_boundary_messages,
    should_apply_tier2, token_count,
};

/// Tiered message compaction stage.
pub struct Compactor {
    config: CompactionConfig,
    session_store: Arc<dyn SessionStore>,
    llm_provider: Option<Arc<dyn LLMProvider>>,
}

impl Compactor {
    /// Creates a compactor that operates on compiled history messages.
    pub fn new(
        config: CompactionConfig,
        session_store: Arc<dyn SessionStore>,
        llm_provider: Option<Arc<dyn LLMProvider>>,
    ) -> Self {
        Self {
            config,
            session_store,
            llm_provider,
        }
    }

    fn should_apply_tier3(&self, ctx: &WorkingContext) -> bool {
        let model_limit = ctx.model_capabilities.context_window.max(1);
        let configured_ceiling = self.config.max_input_tokens_per_turn.max(1);
        let fraction_ceiling = ((model_limit as f64)
            * self.config.tier3_trigger_fraction.clamp(0.0, 1.0))
        .round() as usize;
        let effective_ceiling = configured_ceiling.min(fraction_ceiling.max(1));
        ctx.token_count >= effective_ceiling
    }
}

#[async_trait]
impl ContextProcessor for Compactor {
    fn name(&self) -> &str {
        "compactor"
    }

    fn stage(&self) -> u8 {
        10
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        if !self.config.enabled {
            return Ok(ProcessorOutput::default());
        }

        let Some((history_start, history_end)) = history_bounds(ctx) else {
            return Ok(ProcessorOutput::default());
        };
        if history_start >= history_end || history_end > ctx.messages.len() {
            return Ok(ProcessorOutput::default());
        }

        let tokens_before = ctx.token_count;
        let mut report = CompactionReport {
            tokens_before,
            ..CompactionReport::default()
        };
        report
            .tiers_applied
            .push(CompactionTier::Tier1Deterministic);

        let mut history_messages = ctx.messages[history_start..history_end].to_vec();
        let mut snapshot = load_snapshot(ctx);

        let snapshot_protected = snapshot
            .as_ref()
            .map(protected_snapshot_tool_use_ids)
            .unwrap_or_default();
        let tier1_elided = apply_tier1(
            &mut history_messages,
            self.config.recent_turns_verbatim,
            &HashSet::new(),
        );
        report.messages_elided += tier1_elided;

        if let Some(snapshot) = snapshot.as_mut() {
            report.messages_elided += apply_tier1(&mut snapshot.messages, 0, &snapshot_protected);
            snapshot.token_count = token_count(&snapshot.messages);
        }

        let recent_start =
            recent_turn_boundary_messages(&history_messages, self.config.recent_turns_verbatim);
        if should_apply_tier2(&history_messages, recent_start, &self.config) {
            report.tiers_applied.push(CompactionTier::Tier2CacheAware);
            report.messages_elided += apply_tier2(&mut history_messages);
            if let Some(snapshot) = snapshot.as_mut() {
                collapse_snapshot_for_tier2(snapshot);
            }
        }

        if self.should_apply_tier3(ctx)
            && let Some(llm_provider) = &self.llm_provider
            && let Some(summary) = apply_tier3(
                ctx,
                &history_messages,
                &self.config,
                &*self.session_store,
                &**llm_provider,
            )
            .await?
        {
            report
                .tiers_applied
                .push(CompactionTier::Tier3Summarization);
            report.messages_elided += history_messages.len();
            history_messages = summary.messages;
            report.summary_text = Some(summary.summary);
            report.events_summarized = Some(summary.events_summarized);
            snapshot = None;
        }

        ctx.messages
            .splice(history_start..history_end, history_messages.clone());
        ctx.insert_metadata(
            HISTORY_END_INDEX_METADATA_KEY,
            json!(history_start + history_messages.len()),
        );
        store_snapshot(ctx, snapshot)?;

        ctx.token_count = token_count(&ctx.messages);
        report.tokens_after = ctx.token_count;

        Ok(ProcessorOutput {
            tokens_added: 0,
            tokens_removed: report.tokens_reclaimed(),
            metadata: report_metadata(&report)?,
            ..ProcessorOutput::default()
        })
    }
}

fn report_metadata(report: &CompactionReport) -> Result<HashMap<String, serde_json::Value>> {
    let mut metadata = HashMap::new();
    metadata.insert(
        "tiers_applied".to_string(),
        serde_json::to_value(&report.tiers_applied)?,
    );
    metadata.insert("tokens_before".to_string(), json!(report.tokens_before));
    metadata.insert("tokens_after".to_string(), json!(report.tokens_after));
    metadata.insert(
        "tokens_reclaimed".to_string(),
        json!(report.tokens_reclaimed()),
    );
    metadata.insert("messages_elided".to_string(), json!(report.messages_elided));
    metadata.insert(
        "tier1_applied".to_string(),
        json!(
            report
                .tiers_applied
                .contains(&CompactionTier::Tier1Deterministic)
        ),
    );
    metadata.insert(
        "tier2_applied".to_string(),
        json!(
            report
                .tiers_applied
                .contains(&CompactionTier::Tier2CacheAware)
        ),
    );
    metadata.insert(
        "tier3_applied".to_string(),
        json!(
            report
                .tiers_applied
                .contains(&CompactionTier::Tier3Summarization)
        ),
    );
    if let Some(summary_text) = report.summary_text.as_ref() {
        metadata.insert("summary_text".to_string(), json!(summary_text));
    }
    if let Some(events_summarized) = report.events_summarized {
        metadata.insert("events_summarized".to_string(), json!(events_summarized));
    }

    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use moa_core::{
        CompactionConfig, ContextMessage, ContextProcessor, Event, EventRange, ModelId,
        SessionStore, WorkingContext,
    };
    use serde_json::json;

    use super::Compactor;
    use super::test_support::{MockLlmProvider, MockSessionStore, capabilities, event_record};
    use crate::pipeline::history::{
        HISTORY_END_INDEX_METADATA_KEY, HISTORY_SNAPSHOT_METADATA_KEY,
        HISTORY_START_INDEX_METADATA_KEY,
    };

    #[tokio::test]
    async fn tier3_emits_checkpoint_and_replaces_history_with_summary() {
        let session = super::test_support::session();
        let history = vec![
            event_record(
                &session.id,
                1,
                Event::UserMessage {
                    text: "first request".to_string(),
                    attachments: Vec::new(),
                },
            ),
            event_record(
                &session.id,
                2,
                Event::BrainResponse {
                    text: "first response".to_string(),
                    model: ModelId::new("claude-sonnet-4-6"),
                    model_tier: moa_core::ModelTier::Main,
                    input_tokens_uncached: 10,
                    input_tokens_cache_write: 0,
                    input_tokens_cache_read: 0,
                    output_tokens: 5,
                    cost_cents: 1,
                    duration_ms: 10,
                    thought_signature: None,
                },
            ),
            event_record(
                &session.id,
                3,
                Event::UserMessage {
                    text: "second request".to_string(),
                    attachments: Vec::new(),
                },
            ),
        ];
        let store = Arc::new(MockSessionStore::new(session.clone(), history.clone()));
        let llm = Arc::new(MockLlmProvider);
        let compactor = Compactor::new(
            CompactionConfig {
                max_input_tokens_per_turn: 1,
                recent_turns_verbatim: 1,
                ..CompactionConfig::default()
            },
            store.clone(),
            Some(llm),
        );
        let mut ctx = WorkingContext::new(&session, capabilities());
        ctx.extend_messages(vec![
            ContextMessage::user("first request"),
            ContextMessage::assistant("first response"),
            ContextMessage::user("second request"),
        ]);
        ctx.insert_metadata(HISTORY_START_INDEX_METADATA_KEY, json!(0));
        ctx.insert_metadata(HISTORY_END_INDEX_METADATA_KEY, json!(3));
        ctx.insert_metadata(HISTORY_SNAPSHOT_METADATA_KEY, serde_json::Value::Null);
        ctx.token_count = 10;

        let output = compactor
            .process(&mut ctx)
            .await
            .expect("tier 3 compaction should process");
        let events = store
            .get_events(session.id, EventRange::all())
            .await
            .expect("events should be readable");

        assert!(
            output
                .metadata
                .get("tier3_applied")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        );
        assert!(
            events
                .iter()
                .any(|record| matches!(record.event, Event::Checkpoint { .. }))
        );
        assert!(ctx.messages.iter().any(|message| {
            message.content.contains("<session_checkpoint")
                && message.content.contains("summarized_events")
        }));
        assert_eq!(
            ctx.metadata().get(HISTORY_SNAPSHOT_METADATA_KEY),
            Some(&serde_json::Value::Null)
        );
    }
}
