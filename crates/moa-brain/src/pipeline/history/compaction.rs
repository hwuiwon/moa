//! History-stage checkpoint generation trigger.

use moa_core::{EventRange, EventRecord, ModelTask, Result, WorkingContext};

use crate::compaction::{maybe_compact_events, watermark_may_compact};

use super::{CompiledHistory, HistoryCompiler};

impl HistoryCompiler {
    /// Returns whether compaction might fire this turn, using only the cheap
    /// session-row watermark (event count and last checkpoint sequence).
    ///
    /// When this returns `false` the incremental-snapshot fast path can run
    /// without reading the full event log; only an open gate justifies the
    /// bounded full read in [`Self::compile_full_messages_compacting`].
    pub(super) async fn compaction_gate_open(&self, ctx: &WorkingContext) -> Result<bool> {
        if self.llm_provider.is_none() || !self.compaction.enabled {
            return Ok(false);
        }

        let meta = self.session_store.get_session(ctx.session_id).await?;
        Ok(watermark_may_compact(
            &self.compaction,
            meta.event_count,
            meta.last_checkpoint_seq,
            ctx.token_budget,
        ))
    }

    /// Runs compaction against an already-loaded event list, returning the
    /// emitted checkpoint record when one fired so the caller can fold it into
    /// the list without re-reading the log.
    async fn maybe_emit_checkpoint_for_events(
        &self,
        ctx: &WorkingContext,
        events: &[EventRecord],
    ) -> Result<Option<EventRecord>> {
        let Some(llm_provider) = &self.llm_provider else {
            return Ok(None);
        };

        // The summarization LLM call runs inline here. Item 1's watermark gate
        // makes this path rare (only when the tail actually crosses the
        // threshold), and the history stage has no durable self-call to defer
        // the summary to, so keeping it inline avoids a cross-service hop.
        maybe_compact_events(
            &self.compaction,
            &*self.session_store,
            &**llm_provider,
            ModelTask::Summarization.tier(),
            ctx.session_id,
            ctx.token_budget,
            events,
        )
        .await
    }

    /// Reads the full log once, applies compaction, and compiles the result,
    /// folding any freshly emitted checkpoint into the in-memory list instead of
    /// re-reading the log.
    pub(super) async fn compile_full_messages_compacting(
        &self,
        ctx: &WorkingContext,
        remaining_budget: usize,
    ) -> Result<CompiledHistory> {
        let mut events = self
            .session_store
            .get_events(ctx.session_id, EventRange::all())
            .await?;

        if let Some(checkpoint) = self.maybe_emit_checkpoint_for_events(ctx, &events).await? {
            events.push(checkpoint);
        }

        self.compile_messages_with_stats(&events, remaining_budget)
    }

    /// Reads the full log once and compiles it without attempting compaction.
    ///
    /// Used on the full-replay fallback when the watermark gate is closed, so a
    /// missing or stale snapshot does not pay for a redundant compaction pass.
    pub(super) async fn compile_full_messages(
        &self,
        ctx: &WorkingContext,
        remaining_budget: usize,
    ) -> Result<CompiledHistory> {
        let events = self
            .session_store
            .get_events(ctx.session_id, EventRange::all())
            .await?;

        self.compile_messages_with_stats(&events, remaining_budget)
    }
}

#[cfg(test)]
mod tests {
    use crate::pipeline::history::test_support::prelude::*;

    fn stage_inputs_hash(ctx: &WorkingContext) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        serde_json::to_string(&ctx.messages)
            .expect("working context messages serialize")
            .hash(&mut hasher);
        hasher.finish()
    }

    #[tokio::test]
    async fn compaction_triggers_at_threshold_and_keeps_full_log() {
        let session = session();
        let mut events = Vec::new();
        for index in 0..7 {
            events.push(event_record(
                &session.id,
                index,
                Event::UserMessage {
                    text: format!("event {index}"),
                    attachments: Vec::new(),
                },
            ));
        }
        let store = Arc::new(MockSessionStore::new(session.clone(), events));
        let compiler = HistoryCompiler::with_compaction(
            store.clone(),
            Arc::new(MockLlmProvider),
            CompactionConfig {
                event_threshold: 4,
                recent_turns_verbatim: 2,
                ..CompactionConfig::default()
            },
        );
        let mut ctx = WorkingContext::new(&session, capabilities());

        compiler.process(&mut ctx).await.unwrap();
        let stored_events = store
            .get_events(session.id, EventRange::all())
            .await
            .unwrap();

        assert_eq!(stored_events.len(), 8);
        assert!(matches!(
            stored_events.last().map(|record| &record.event),
            Some(Event::Checkpoint { events_summarized, .. }) if *events_summarized == 5
        ));
    }

    #[tokio::test]
    async fn compacted_view_preserves_old_errors_and_respects_budget() {
        let session = session();
        let mut events = vec![event_record(
            &session.id,
            0,
            Event::Error {
                message: "deploy failed on port binding".to_string(),
                recoverable: true,
            },
        )];
        for index in 1..12 {
            events.push(event_record(
                &session.id,
                index,
                Event::UserMessage {
                    text: format!("turn {index}"),
                    attachments: Vec::new(),
                },
            ));
        }
        events.push(event_record(
            &session.id,
            12,
            Event::Checkpoint {
                summary: "## Key Facts\n- earlier turns were compacted".to_string(),
                events_summarized: 8,
                token_count: 12,
                model: ModelId::new("claude-sonnet-4-6"),
                model_tier: moa_core::ModelTier::Auxiliary,
                input_tokens: 60,
                output_tokens: 20,
                cost_cents: 1,
            },
        ));
        let compiler = HistoryCompiler::new(Arc::new(MockSessionStore::new(
            session.clone(),
            events.clone(),
        )));

        let (messages, tokens_used) = compiler.compile_messages(&events, 80).unwrap();
        let rendered = messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("deploy failed on port binding"));
        assert!(rendered.contains("<session_checkpoint"));
        assert!(tokens_used <= 120);
    }

    #[tokio::test]
    async fn no_compaction_below_threshold() {
        let session = session();
        let events = vec![
            event_record(
                &session.id,
                0,
                Event::UserMessage {
                    text: "one".to_string(),
                    attachments: Vec::new(),
                },
            ),
            event_record(
                &session.id,
                1,
                Event::UserMessage {
                    text: "two".to_string(),
                    attachments: Vec::new(),
                },
            ),
        ];
        let store = Arc::new(MockSessionStore::new(session.clone(), events));
        let compiler = HistoryCompiler::with_compaction(
            store.clone(),
            Arc::new(MockLlmProvider),
            CompactionConfig {
                event_threshold: 10,
                ..CompactionConfig::default()
            },
        );
        let mut ctx = WorkingContext::new(&session, capabilities());

        compiler.process(&mut ctx).await.unwrap();
        let stored_events = store
            .get_events(session.id, EventRange::all())
            .await
            .unwrap();

        assert_eq!(stored_events.len(), 2);
        assert!(
            !stored_events
                .iter()
                .any(|record| matches!(record.event, Event::Checkpoint { .. }))
        );
    }

    #[tokio::test]
    async fn compaction_triggers_even_when_incremental_snapshot_is_current() {
        let session = session();
        let mut events = Vec::new();
        for index in 0..7 {
            events.push(event_record(
                &session.id,
                index,
                Event::UserMessage {
                    text: format!("event {index}"),
                    attachments: Vec::new(),
                },
            ));
        }
        let store = Arc::new(MockSessionStore::new(session.clone(), events.clone()));
        let compiler = HistoryCompiler::with_compaction(
            store.clone(),
            Arc::new(MockLlmProvider),
            CompactionConfig {
                event_threshold: 4,
                recent_turns_verbatim: 1,
                ..CompactionConfig::default()
            },
        );
        let mut ctx = WorkingContext::new(&session, capabilities());
        let prefix = compiler
            .compile_messages_with_stats(&events[..2], 100_000)
            .expect("compile snapshot prefix");
        let mut snapshot = compiled_snapshot(&session, &prefix).expect("prefix yields snapshot");
        snapshot.stage_inputs_hash = stage_inputs_hash(&ctx);
        store
            .put_snapshot(session.id, snapshot)
            .await
            .expect("store current snapshot");

        compiler.process(&mut ctx).await.expect("compile history");
        let stored_events = store
            .get_events(session.id, EventRange::all())
            .await
            .expect("load stored events");

        assert!(matches!(
            stored_events.last().map(|record| &record.event),
            Some(Event::Checkpoint { events_summarized, .. }) if *events_summarized == 6
        ));
    }
}
