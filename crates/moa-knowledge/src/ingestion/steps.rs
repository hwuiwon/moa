//! Knowledge ingestion steps operations.

use super::*;

impl<R, P, E, G> KnowledgeIngestionPipeline<R, P, E, G>
where
    R: KnowledgeRepository,
    P: DocumentParser,
    E: EmbeddingProvider,
    G: KnowledgeGraphWriter,
{
    pub(super) async fn handle_deleted_record(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
        _record: ProviderRecord,
    ) -> Result<u64> {
        // F06: persist the provider-deleted object's latest metadata in a
        // non-terminal status before cleanup. `materialize_object` stamps
        // provider-deleted records with terminal `deleted`; upserting that directly
        // would strand active chunks the same way the old in-`delete_object`
        // ordering did (a failed invalidation would leave a `deleted` row no prune
        // pass revisits). `delete_object` writes the terminal status last, only
        // after invalidation and tombstoning succeed.
        let pre_cleanup_object = KnowledgeObject {
            status: crate::domain::ObjectStatus::Active,
            deleted_at: None,
            ..object.clone()
        };
        self.repository.upsert_object(pre_cleanup_object).await?;
        self.delete_object(
            sync_run_uid,
            object,
            json!({ "records_seen": 1, "records_deleted": 1 }),
            Some("provider record deleted".to_string()),
            KnowledgeSyncCounters {
                records_seen: 1,
                records_deleted: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await
    }

    pub(super) async fn handle_pruned_object(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
    ) -> Result<u64> {
        self.delete_object(
            sync_run_uid,
            object,
            json!({ "records_deleted": 1, "records_pruned": 1 }),
            Some("provider record absent from selected sources".to_string()),
            KnowledgeSyncCounters {
                records_deleted: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await
    }

    pub(super) async fn delete_object(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
        counters: Value,
        summary: Option<String>,
        counter_delta: KnowledgeSyncCounters,
    ) -> Result<u64> {
        self.record_counter_step(
            sync_run_uid,
            Some(object.object_uid),
            "object_change_checked",
            match summary {
                Some(summary) => {
                    StepOutcome::completed_with_counters_and_summary(counters, summary)
                }
                None => StepOutcome::completed_with_counters(counters),
            },
            counter_delta,
        )
        .await?;
        // F06: invalidate the graph and tombstone chunks FIRST (both idempotent),
        // then write the terminal `deleted` status LAST. If invalidation fails the
        // object stays non-terminal, so the next prune pass or deletion retry still
        // selects it and finishes the cleanup — instead of a `deleted` row stranding
        // active graph chunks that no later pass revisits.
        let latest = self
            .repository
            .latest_document_version(object.object_uid)
            .await?;
        let chunks = if let Some(version) = latest {
            self.repository
                .chunks_for_version(version.version_uid)
                .await?
        } else {
            Vec::new()
        };
        let graph_uids = chunks
            .iter()
            .map(|chunk| stable_uid(&format!("chunk:{}:{}", object.tenant_id, chunk.chunk_hash)))
            .collect::<Vec<_>>();
        let invalidation_span = tracing::info_span!(
            "knowledge_graph_write",
            tenant_id = %object.tenant_id,
            connection_id = %object.connection_uid,
            sync_run_id = %sync_run_uid,
            object_id = %object.object_uid,
            provider = %self.provider,
            parser = %self.parser_label,
            kind = "invalidate",
            status = tracing::field::Empty,
            error_code = tracing::field::Empty,
            graph_node_count = graph_uids.len(),
            graph_edge_count = 0
        );
        let invalidation_report = match self
            .graph
            .invalidate_chunks(&graph_uids)
            .instrument(invalidation_span.clone())
            .await
        {
            Ok(report) => {
                record_span_outcome(&invalidation_span, "completed", None);
                report
            }
            Err(error) => {
                let classification = self
                    .record_failure_step(
                        sync_run_uid,
                        Some(object.object_uid),
                        "vector_indexed",
                        &error,
                    )
                    .await?;
                record_span_outcome(
                    &invalidation_span,
                    "failed",
                    Some(classification.error_code),
                );
                return Err(error);
            }
        };
        let chunk_uids = chunks
            .iter()
            .map(|chunk| chunk.chunk_uid)
            .collect::<Vec<_>>();
        self.repository.tombstone_chunks(&chunk_uids).await?;
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "chunks_diffed",
            StepOutcome::completed_with_counters(json!({ "chunks_deleted": chunk_uids.len() })),
        )
        .await?;
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "vector_indexed",
            StepOutcome::completed_with_counters(json!({
                "vector_rows_deleted": invalidation_report.vector_rows_deleted
            })),
        )
        .await?;
        // Terminal state written last: only now that invalidation and tombstoning
        // have both succeeded is it safe to mark the object `deleted` and remove it
        // from the prune/retry-eligible set.
        self.repository
            .mark_object_deleted(object.object_uid, Utc::now())
            .await?;
        Ok(1)
    }

    pub(super) async fn record_step(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
        stage: &'static str,
        outcome: StepOutcome,
    ) -> Result<()> {
        self.record_step_with_counters(sync_run_uid, object_uid, stage, outcome, None)
            .await
            .map(|_| ())
    }

    pub(super) async fn record_counter_step(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
        stage: &'static str,
        outcome: StepOutcome,
        counter_delta: KnowledgeSyncCounters,
    ) -> Result<bool> {
        self.record_step_with_counters(
            sync_run_uid,
            object_uid,
            stage,
            outcome,
            Some(counter_delta),
        )
        .await
    }

    pub(super) async fn record_step_with_counters(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
        stage: &'static str,
        mut outcome: StepOutcome,
        counter_delta: Option<KnowledgeSyncCounters>,
    ) -> Result<bool> {
        let started = std::time::Instant::now();
        let labels = StepLabels {
            provider: &self.provider,
            parser: &self.parser_label,
            stage,
            retryable: outcome
                .summary
                .as_deref()
                .is_some_and(|summary| summary.starts_with("retryable")),
            error_code: outcome.error_code.as_deref().unwrap_or("none"),
        };
        let status = outcome.status.as_str();
        tracing::Span::current().record("status", status);
        tracing::Span::current().record(
            "error_code",
            outcome.error_code.as_deref().unwrap_or("none"),
        );
        outcome.duration_ms = outcome.duration_ms.or_else(|| {
            u64::try_from(started.elapsed().as_millis())
                .ok()
                .map(|duration| duration.max(1))
        });
        record_step_observability(labels, &outcome);
        let step = build_step_row(sync_run_uid, object_uid, stage, outcome);
        if let Some(counter_delta) = counter_delta {
            self.repository
                .record_ingestion_step_once(step, counter_delta)
                .await
        } else {
            self.repository
                .record_ingestion_step(step)
                .await
                .map(|()| true)
        }
    }

    pub(super) async fn record_failure_step(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
        stage: &'static str,
        error: &Error,
    ) -> Result<FailureClassification> {
        let classification = classify_failure(stage, error);
        self.record_counter_step(
            sync_run_uid,
            object_uid,
            stage,
            failed_outcome(classification),
            KnowledgeSyncCounters {
                records_failed: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;
        if let Some(mut run) = self.repository.get_sync_run(sync_run_uid).await? {
            run.status = if classification.retryable {
                SyncRunStatus::FailedRetryable
            } else {
                SyncRunStatus::FailedTerminal
            };
            run.error_code = Some(classification.error_code.to_string());
            run.finished_at = Some(Utc::now());
            self.repository.update_sync_run(run).await?;
        }
        let object_id = object_uid
            .map(|uid| uid.to_string())
            .unwrap_or_else(|| "none".to_string());
        tracing::warn!(
            sync_run_id = %sync_run_uid,
            object_id = %object_id,
            provider = %self.provider,
            parser = %self.parser_label,
            stage,
            retryable = classification.retryable,
            error_code = classification.error_code,
            "knowledge ingestion failure recorded"
        );
        Ok(classification)
    }
}

pub(super) fn record_span_outcome(span: &Span, status: &'static str, error_code: Option<&str>) {
    span.record("status", status);
    span.record("error_code", error_code.unwrap_or("none"));
}
