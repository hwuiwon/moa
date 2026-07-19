//! Knowledge ingestion page operations.

use super::*;

impl<R, P, E, G> KnowledgeIngestionPipeline<R, P, E, G>
where
    R: KnowledgeRepository,
    P: DocumentParser,
    E: EmbeddingProvider,
    G: KnowledgeGraphWriter,
{
    /// Creates a knowledge ingestion pipeline from injected dependencies.
    #[must_use]
    pub fn new(
        repository: Arc<R>,
        parser: Arc<P>,
        embedder: Arc<E>,
        graph: Arc<G>,
        config: KnowledgeIngestionPipelineConfig,
    ) -> Self {
        Self {
            repository,
            parser,
            embedder,
            graph,
            chunking: config.chunking,
            provider: config.provider,
            parser_label: config.parser_label,
            semantic_generic_entities: true,
            semantic_model_extractor: None,
            content_fetcher: None,
        }
    }

    /// Sets whether semantic extraction emits the deterministic generic
    /// proper-noun entity fallback for chunks with no domain-rule match.
    ///
    /// Defaults to enabled; wire this from `knowledge.semantic.generic_entities`
    /// to disable the fallback for a deployment.
    #[must_use]
    pub fn with_semantic_generic_entities(mut self, enabled: bool) -> Self {
        self.semantic_generic_entities = enabled;
        self
    }

    /// Attaches a model-backed semantic graph extractor as the production
    /// extractor.
    ///
    /// When present it replaces the deterministic keyword ruleset for new or
    /// changed chunks; the keyword ruleset remains the per-chunk fallback used
    /// whenever a model call, timeout, or parse fails. Passing `None` keeps the
    /// deterministic keyword extractor as the sole extractor.
    #[must_use]
    pub fn with_semantic_model_extractor(
        mut self,
        extractor: Option<Arc<ModelSemanticGraphExtractor>>,
    ) -> Self {
        self.semantic_model_extractor = extractor;
        self
    }

    /// Attaches a per-run content fetcher used to download byte content for
    /// records that carry neither inline text nor a directly fetchable URL.
    ///
    /// Passing `None` leaves the pipeline with its title-only fallback for such
    /// records.
    #[must_use]
    pub fn with_content_fetcher(
        mut self,
        content_fetcher: Option<Arc<dyn RecordContentFetcher>>,
    ) -> Self {
        self.content_fetcher = content_fetcher;
        self
    }

    /// Ingests one provider record page, including change-token checks and deletions.
    #[tracing::instrument(
        name = "knowledge_sync_run",
        skip(self, page),
        fields(
            tenant_id = %tenant_id,
            connection_id = %connection_uid,
            sync_run_id = %sync_run_uid,
            provider = %self.provider,
            parser = %self.parser_label,
            status = tracing::field::Empty,
            error_code = tracing::field::Empty,
            records_listed = page.records.len()
        )
    )]
    pub async fn ingest_record_page(
        &self,
        sync_run_uid: Uuid,
        connection_uid: Uuid,
        tenant_id: moa_core::types::identifiers::TenantId,
        page: RecordPage,
    ) -> Result<PageIngestionReport> {
        let records_listed = page.records.len() as u64;
        self.record_step(
            sync_run_uid,
            None,
            "provider_records_listed",
            StepOutcome::completed_with_counters(json!({ "records_listed": records_listed })),
        )
        .await?;

        // Process the page's records with bounded concurrency. Each record has a
        // distinct object and is guarded by its own document-version ingestion
        // claim, so parallel records do not conflict; the shared sqlx pool and the
        // atomic-increment sync-run counters are concurrency-safe. Results are
        // folded deterministically after completion (per-record step ordering may
        // interleave, but nothing depends on global step order). A record error
        // aborts the page, preserving the serial `?` contract.
        let contributions = stream::iter(page.records.into_iter().map(|record| {
            let object = self.materialize_object(connection_uid, tenant_id, &record);
            self.process_page_record(sync_run_uid, object, record)
        }))
        .buffer_unordered(MAX_CONCURRENT_PAGE_RECORDS)
        .try_collect::<Vec<_>>()
        .await?;

        let mut report = PageIngestionReport {
            records_listed,
            ..PageIngestionReport::default()
        };
        for contribution in contributions {
            match contribution {
                PageRecordContribution::Deleted(deleted) => {
                    report.records_deleted = report.records_deleted.saturating_add(deleted);
                }
                PageRecordContribution::Ingested { embeddings_created } => {
                    report.records_ingested = report.records_ingested.saturating_add(1);
                    report.embeddings_created =
                        report.embeddings_created.saturating_add(embeddings_created);
                }
                PageRecordContribution::Skipped => {
                    report.records_skipped = report.records_skipped.saturating_add(1);
                }
            }
        }
        Ok(report)
    }

    /// Processes one provider record (delete, ingest, or skip) and returns its
    /// contribution to the page report. Extracted so a page's records can run
    /// concurrently while the report is aggregated deterministically afterward.
    pub(super) async fn process_page_record(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
        record: ProviderRecord,
    ) -> Result<PageRecordContribution> {
        if record.deleted {
            let deleted = self
                .handle_deleted_record(sync_run_uid, object, record)
                .await?;
            return Ok(PageRecordContribution::Deleted(deleted));
        }
        match self.ingest_record(sync_run_uid, object, record).await? {
            RecordIngestionOutcome::Ingested { embeddings_created } => {
                Ok(PageRecordContribution::Ingested { embeddings_created })
            }
            RecordIngestionOutcome::Skipped => Ok(PageRecordContribution::Skipped),
        }
    }

    /// Tombstones active local objects that were absent from an exhaustive selected-source sync.
    #[tracing::instrument(
        name = "knowledge_source_selection_prune",
        skip(self, seen_source_ids),
        fields(
            tenant_id = %tenant_id,
            connection_id = %connection_uid,
            sync_run_id = %sync_run_uid,
            provider = %self.provider,
            parser = %self.parser_label,
            status = tracing::field::Empty,
            error_code = tracing::field::Empty,
            seen_sources = seen_source_ids.len()
        )
    )]
    pub async fn prune_unseen_objects(
        &self,
        sync_run_uid: Uuid,
        connection_uid: Uuid,
        tenant_id: moa_core::types::identifiers::TenantId,
        seen_source_ids: &HashSet<String>,
    ) -> Result<PageIngestionReport> {
        // Compute the prune set in SQL (active objects not in the seen set) and
        // page through it with a keyset cursor so a large already-seen set is
        // never materialized on the client. Each pruned object flips to
        // `deleted`, but the cursor advances independently, guaranteeing
        // termination.
        let seen = seen_source_ids.iter().cloned().collect::<Vec<_>>();
        let mut report = PageIngestionReport::default();
        let mut cursor: Option<(String, Uuid)> = None;
        loop {
            let batch = self
                .repository
                .unseen_active_objects_for_connection(
                    connection_uid,
                    tenant_id,
                    &seen,
                    cursor.take(),
                    PRUNE_BATCH_SIZE,
                )
                .await?;
            let Some(last) = batch.last() else {
                break;
            };
            cursor = Some((last.source_id.clone(), last.object_uid));
            let batch_len = batch.len();
            // Prune each batch's objects with bounded concurrency (distinct
            // objects, atomic counters — safe), keeping the keyset cursor
            // sequential. An error aborts the prune, preserving the serial
            // contract; per-batch deletes are summed after completion.
            let deleted_counts = stream::iter(
                batch
                    .into_iter()
                    .map(|object| self.handle_pruned_object(sync_run_uid, object)),
            )
            .buffer_unordered(MAX_CONCURRENT_PAGE_RECORDS)
            .try_collect::<Vec<u64>>()
            .await?;
            report.records_deleted = report
                .records_deleted
                .saturating_add(deleted_counts.iter().sum());
            if batch_len < PRUNE_BATCH_SIZE as usize {
                break;
            }
        }
        self.record_counter_step(
            sync_run_uid,
            None,
            "source_selection_pruned",
            StepOutcome::completed_with_counters_and_summary(
                json!({ "records_pruned": report.records_deleted }),
                "removed objects absent from selected provider sources",
            ),
            KnowledgeSyncCounters::default(),
        )
        .await?;
        Ok(report)
    }
}
