//! Restate virtual-object adapter for durable slow-path graph-memory ingestion.

use std::{sync::Arc, time::Duration};

use moa_memory_ingest::{IngestApplyReport, IngestRuntime, SessionTurn, SlowPathIngestor};
use restate_sdk::prelude::*;

const DONE_KEY_PREFIX: &str = "done";

/// Restate virtual object surface for slow-path turn ingestion.
#[restate_sdk::object]
pub trait IngestionVO {
    /// Ingests one finalized session turn into graph memory.
    async fn ingest_turn(turn: Json<SessionTurn>) -> Result<Json<IngestApplyReport>, HandlerError>;
}

/// Durable adapter over the host-owned ingestion runtime.
#[derive(Clone)]
pub struct IngestionVOImpl {
    ingestor: SlowPathIngestor,
}

impl IngestionVOImpl {
    /// Creates the virtual object with an explicitly injected ingestion runtime.
    #[must_use]
    pub fn new(runtime: Arc<IngestRuntime>) -> Self {
        Self {
            ingestor: SlowPathIngestor::new(runtime),
        }
    }
}

impl IngestionVO for IngestionVOImpl {
    #[tracing::instrument(skip(self, ctx, turn))]
    // SAFETY: Internal-only ingestion object; scope and ownership come from the finalized session turn.
    async fn ingest_turn(
        &self,
        ctx: ObjectContext<'_>,
        turn: Json<SessionTurn>,
    ) -> Result<Json<IngestApplyReport>, HandlerError> {
        moa_observability::adopt_remote_parent(&tracing::Span::current(), |name| {
            ctx.headers().get(name).cloned()
        });
        let turn = turn.into_inner();
        let done_key = done_key(turn.turn_seq);
        if ctx
            .get::<Json<bool>>(&done_key)
            .await?
            .map(Json::into_inner)
            .unwrap_or(false)
        {
            return Ok(Json::from(IngestApplyReport::default()));
        }

        if self.ingestor.should_skip_degraded(&turn).await? {
            ctx.set(&done_key, Json::from(true));
            return Ok(Json::from(IngestApplyReport {
                skipped: 1,
                ..IngestApplyReport::default()
            }));
        }

        let chunk_ingestor = self.ingestor.clone();
        let turn_for_chunking = turn.clone();
        let chunks = ctx
            .run(|| async move { chunk_ingestor.chunk(&turn_for_chunking).map(Json::from) })
            .name("chunk")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let extract_ingestor = self.ingestor.clone();
        let extract_chunks = chunks.clone();
        let extracted = ctx
            .run(|| async move {
                extract_ingestor
                    .extract(&extract_chunks)
                    .await
                    .map(Json::from)
            })
            .name("extract")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let classify_ingestor = self.ingestor.clone();
        let classify_facts_input = extracted.clone();
        let classified = ctx
            .run(|| async move {
                classify_ingestor
                    .classify_pii(&classify_facts_input)
                    .await
                    .map(Json::from)
            })
            .name("classify_pii")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let embed_ingestor = self.ingestor.clone();
        let embed_input = classified.clone();
        let embedded = ctx
            .run(|| async move { embed_ingestor.embed(&embed_input).await.map(Json::from) })
            .name("embed")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let contradict_ingestor = self.ingestor.clone();
        let contradiction_turn = turn.clone();
        let contradiction_input = embedded.clone();
        let decisions = ctx
            .run(|| async move {
                contradict_ingestor
                    .contradict(&contradiction_turn, &contradiction_input)
                    .await
                    .map(Json::from)
            })
            .name("contradict")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        let apply_ingestor = self.ingestor.clone();
        let upsert_turn = turn.clone();
        let report = ctx
            .run(|| async move {
                apply_ingestor
                    .apply(&upsert_turn, &decisions)
                    .await
                    .map(Json::from)
            })
            .name("upsert")
            .retry_policy(ingest_step_retry_policy())
            .await?
            .into_inner();

        ctx.set(&done_key, Json::from(true));
        Ok(Json::from(report))
    }
}

fn done_key(turn_seq: u64) -> String {
    format!("{DONE_KEY_PREFIX}:{turn_seq}")
}

fn ingest_step_retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_millis(250))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(5))
        .max_attempts(5)
}
