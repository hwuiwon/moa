//! Knowledge ingestion materialization operations.

use super::steps::record_span_outcome;
use super::*;

impl<R, P, E, G> KnowledgeIngestionPipeline<R, P, E, G>
where
    R: KnowledgeRepository,
    P: DocumentParser,
    E: EmbeddingProvider,
    G: KnowledgeGraphWriter,
{
    pub(super) async fn persist_parsed(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
        parsed: ParsedDocument,
    ) -> Result<PersistedIngestion> {
        let content_hash = content_hash(&normalize_text(&parsed.text));
        let latest_version = self
            .repository
            .latest_document_version(object.object_uid)
            .await?;
        let latest_chunks = if let Some(latest) = &latest_version {
            self.repository
                .chunks_for_version(latest.version_uid)
                .await?
        } else {
            Vec::new()
        };
        let latest_version_completed = if let Some(latest) = &latest_version {
            latest.content_hash == content_hash
                && self
                    .repository
                    .object_ingestion_completed_since(object.object_uid, latest.created_at)
                    .await?
        } else {
            false
        };
        if let Some(latest) = &latest_version
            && latest.content_hash == content_hash
            && !latest_chunks.is_empty()
            && latest_version_completed
        {
            self.record_step(
                sync_run_uid,
                Some(object.object_uid),
                "normalized",
                StepOutcome {
                    status: IngestionStepStatus::Skipped,
                    counters: json!({ "blocks_total": 0, "chunks_total": latest_chunks.len() }),
                    summary: Some("content hash unchanged".to_string()),
                    retry_count: 0,
                    error_code: None,
                    duration_ms: None,
                },
            )
            .await?;
            return Ok(PersistedIngestion {
                delta: KnowledgeGraphDelta::default(),
                embeddings_created: 0,
                ingested: false,
            });
        }

        // Reconcile against every currently active chunk for the object, not just
        // the latest version's chunks. A failed or retried same-hash version
        // transition leaves the newest version incomplete; keying `previous_chunks`
        // off that version (previously an empty list) forgot the real predecessor
        // and left both versions' chunks active. The new version's chunk
        // occurrences are the desired state, and every active prior chunk whose
        // occurrence uid is absent from them is invalidated in
        // `persist_claimed_version` — so this version's own chunks re-persisted on
        // retry keep their identity while superseded occurrences (including
        // byte-identical text carried over from an older version) are reliably
        // orphaned.
        let previous_chunks = self
            .repository
            .active_chunks_for_object(object.object_uid)
            .await?;
        let version = if let Some(latest) = latest_version
            && latest.content_hash == content_hash
        {
            latest
        } else {
            DocumentVersion {
                version_uid: stable_uid(&format!("version:{}:{content_hash}", object.object_uid)),
                object_uid: object.object_uid,
                parser: parsed.parser.clone(),
                parser_job_id: parsed.parser_job_id.clone(),
                content_hash,
                metadata: parsed.metadata.clone(),
                created_at: Utc::now(),
            }
        };
        let (version, claim_token) = match self
            .repository
            .claim_document_version_ingestion(sync_run_uid, version)
            .await?
        {
            DocumentVersionIngestionClaim::Claimed {
                version,
                claim_token,
            } => (version, claim_token),
            DocumentVersionIngestionClaim::AlreadyInProgress(_version)
            | DocumentVersionIngestionClaim::AlreadyCompleted(_version) => {
                self.record_step(
                    sync_run_uid,
                    Some(object.object_uid),
                    "normalized",
                    StepOutcome {
                        status: IngestionStepStatus::Skipped,
                        counters: json!({ "blocks_total": 0, "chunks_total": 0 }),
                        summary: Some("content version already claimed".to_string()),
                        retry_count: 0,
                        error_code: None,
                        duration_ms: None,
                    },
                )
                .await?;
                return Ok(PersistedIngestion {
                    delta: KnowledgeGraphDelta::default(),
                    embeddings_created: 0,
                    ingested: false,
                });
            }
        };
        let version_uid = version.version_uid;
        let persisted = self
            .persist_claimed_version(sync_run_uid, object, version, parsed, previous_chunks)
            .await;
        if persisted.is_ok() {
            self.repository
                .complete_document_version_ingestion(sync_run_uid, version_uid, claim_token)
                .await?;
        } else {
            self.repository
                .fail_document_version_ingestion(sync_run_uid, version_uid, claim_token)
                .await?;
        }
        persisted
    }

    pub(super) async fn persist_claimed_version(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
        version: DocumentVersion,
        parsed: ParsedDocument,
        previous_chunks: Vec<KnowledgeChunk>,
    ) -> Result<PersistedIngestion> {
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "normalized",
            StepOutcome::completed(),
        )
        .await?;

        let blocks = elements_to_blocks(version.version_uid, &parsed.elements);
        self.repository
            .replace_blocks(version.version_uid, blocks.clone())
            .await?;
        let old_block_hashes = previous_chunks
            .iter()
            .flat_map(|chunk| chunk.block_hashes.iter())
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let blocks_new = blocks
            .iter()
            .filter(|block| !old_block_hashes.contains(&block.block_hash))
            .count();
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "blocks_diffed",
            StepOutcome::completed_with_counters(json!({
                    "blocks_total": blocks.len(),
                    "blocks_new": blocks_new,
            })),
        )
        .await?;

        let chunks = blocks_to_chunks(version.version_uid, &blocks, self.chunking);
        let semantic_report = match self
            .semantic_graph_extractions(object.tenant_id, &object, &chunks)
            .await
        {
            Ok(report) => report,
            Err(error) => {
                self.record_failure_step(
                    sync_run_uid,
                    Some(object.object_uid),
                    "semantic_graph_extracted",
                    &error,
                )
                .await?;
                return Err(error);
            }
        };
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "semantic_graph_extracted",
            StepOutcome::completed_with_counters(json!({
                "chunks_total": chunks.len(),
                "cache_hits": semantic_report.cache_hits,
                "cache_misses": semantic_report.cache_misses,
                "entities_extracted": semantic_report.entities_extracted,
                "relations_extracted": semantic_report.relations_extracted,
                "semantic_chunk_links": semantic_report.semantic_chunk_links,
                "failures": 0,
            })),
        )
        .await?;
        let delta = document_chunk_delta_with_semantics(
            &object,
            &version,
            &chunks,
            &semantic_report.extractions,
        );
        // Fold the `active` retrieval marker into the rows before the batch
        // insert. Graph identity needs no folding: `chunk_uid` is the occurrence
        // node uid, and `replace_chunks` persists it into `graph_node_uid`.
        let mut chunks = chunks;
        for chunk in &mut chunks {
            chunk.metadata = mark_metadata_active(std::mem::take(&mut chunk.metadata));
        }
        self.repository
            .replace_chunks(version.version_uid, chunks.clone())
            .await?;
        let old_hashes = previous_chunks
            .iter()
            .map(|chunk| chunk.chunk_hash.as_str())
            .collect::<HashSet<_>>();
        let chunks_new = chunks
            .iter()
            .filter(|chunk| !old_hashes.contains(chunk.chunk_hash.as_str()))
            .count();
        // Orphaning is keyed on occurrence identity, never on content: a chunk
        // carried over unchanged into a new document version is a NEW occurrence
        // with its own node, embedding, and citation, so the superseded
        // occurrence must be invalidated. Re-persisting this same version yields
        // identical `chunk_uid`s, so a retry orphans nothing of its own.
        let current_uids = chunks
            .iter()
            .map(|chunk| chunk.chunk_uid)
            .collect::<HashSet<_>>();
        let orphan_chunks = previous_chunks
            .iter()
            .filter(|chunk| !current_uids.contains(&chunk.chunk_uid))
            .cloned()
            .collect::<Vec<_>>();
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "chunks_diffed",
            StepOutcome::completed_with_counters(json!({
                    "chunks_total": chunks.len(),
                    "chunks_new": chunks_new,
                    "chunks_deleted": orphan_chunks.len(),
            })),
        )
        .await?;

        // Every occurrence gets its own embedding row and vector association keyed
        // by `chunk_uid`. Only the *computation* is ever shared, and only when the
        // complete contextual input — document title, heading path, and chunk text
        // — is identical under this pipeline's embedding model and version. Equal
        // chunk text alone is never sufficient: the same sentence under a different
        // title or heading path embeds differently, and reusing one occurrence's
        // association for another is exactly the cross-document collapse this
        // pipeline must not perform.
        let mut unique_inputs: Vec<String> = Vec::new();
        let mut input_index_by_text: HashMap<String, usize> = HashMap::new();
        let mut chunk_inputs: Vec<(Uuid, usize)> = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            let input = contextual_embedding_input(object.title.as_deref(), chunk);
            let index = *input_index_by_text.entry(input.clone()).or_insert_with(|| {
                unique_inputs.push(input);
                unique_inputs.len() - 1
            });
            chunk_inputs.push((chunk.chunk_uid, index));
        }
        let embeddings = if unique_inputs.is_empty() {
            HashMap::new()
        } else {
            let vectors = match self.embedder.embed(&unique_inputs).await {
                Ok(vectors) => vectors,
                Err(error) => {
                    let error = Error::Repository(format!("embedding failed: {error}"));
                    self.record_failure_step(
                        sync_run_uid,
                        Some(object.object_uid),
                        "embedded",
                        &error,
                    )
                    .await?;
                    return Err(error);
                }
            };
            // F08: reject the batch on an embedding cardinality mismatch rather than
            // zipping, which would silently drop or misalign chunk embeddings. The
            // failed version claim stays retryable; no graph/vector write happens.
            if vectors.len() != unique_inputs.len() {
                let error = Error::EmbeddingCardinalityMismatch {
                    expected: unique_inputs.len(),
                    actual: vectors.len(),
                };
                self.record_failure_step(sync_run_uid, Some(object.object_uid), "embedded", &error)
                    .await?;
                return Err(error);
            }
            chunk_inputs
                .iter()
                .map(|(chunk_uid, index)| (*chunk_uid, vectors[*index].clone()))
                .collect::<HashMap<_, _>>()
        };
        // `embeddings_created` counts per-occurrence associations written;
        // `embeddings_reused` counts the associations served from another
        // occurrence's computation in this pass, so their difference is the number
        // of embedder computations paid for.
        self.record_counter_step(
            sync_run_uid,
            Some(object.object_uid),
            "embedded",
            StepOutcome::completed_with_counters(json!({
                    "embeddings_created": embeddings.len(),
                    "embeddings_reused": chunks.len().saturating_sub(unique_inputs.len()),
                    "chunks_embedded": embeddings.len(),
            })),
            KnowledgeSyncCounters {
                chunks_embedded: embeddings.len() as u64,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;

        let graph_span = tracing::info_span!(
            "knowledge_graph_write",
            tenant_id = %object.tenant_id,
            connection_id = %object.connection_uid,
            sync_run_id = %sync_run_uid,
            object_id = %object.object_uid,
            provider = %self.provider,
            parser = %self.parser_label,
            kind = "upsert",
            status = tracing::field::Empty,
            error_code = tracing::field::Empty,
            graph_node_count = delta.nodes.len(),
            graph_edge_count = delta.edges.len()
        );
        let graph_report = match self
            .graph
            .upsert_delta(
                &delta,
                &embeddings,
                self.embedder.model_id(),
                self.embedder.model_version(),
            )
            .instrument(graph_span.clone())
            .await
        {
            Ok(report) => {
                record_span_outcome(&graph_span, "completed", None);
                report
            }
            Err(error) => {
                let classification = self
                    .record_failure_step(
                        sync_run_uid,
                        Some(object.object_uid),
                        "graph_upserted",
                        &error,
                    )
                    .await?;
                record_span_outcome(&graph_span, "failed", Some(classification.error_code));
                return Err(error);
            }
        };
        self.record_counter_step(
            sync_run_uid,
            Some(object.object_uid),
            "graph_upserted",
            StepOutcome::completed_with_counters(json!({
                    "graph_nodes_upserted": graph_report.nodes_upserted,
                    "graph_edges_upserted": graph_report.edges_upserted,
            })),
            KnowledgeSyncCounters {
                graph_nodes_upserted: graph_report.nodes_upserted,
                graph_edges_upserted: graph_report.edges_upserted,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;

        // Invalidate exactly the persisted occurrence identities that were
        // superseded. The uid comes from the stored chunk row, never from a
        // recomputed tenant-plus-content-hash seed, so a shared-content chunk in
        // another document is untouched.
        let orphan_uids = orphan_chunks
            .iter()
            .map(|chunk| chunk.chunk_uid)
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
            graph_node_count = orphan_uids.len(),
            graph_edge_count = 0
        );
        let invalidation_report = match self
            .graph
            .invalidate_chunks(&orphan_uids)
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
        let tombstones = orphan_chunks
            .iter()
            .map(|chunk| chunk.chunk_uid)
            .collect::<Vec<_>>();
        self.repository.tombstone_chunks(&tombstones).await?;
        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "vector_indexed",
            StepOutcome::completed_with_counters(json!({
                    "vector_rows_upserted": embeddings.len(),
                    "vector_rows_deleted": invalidation_report.vector_rows_deleted,
            })),
        )
        .await?;
        let contact_delta = derive_contact_groups_from_object(&object);
        let contact_group_count = contact_delta.groups.len();
        let mut contact_memberships = 0_u64;
        for group in contact_delta.groups {
            let group_uid = group.group_uid;
            self.repository.upsert_contact_group(group).await?;
            let memberships = contact_delta
                .memberships
                .iter()
                .filter(|membership| membership.group_uid == group_uid)
                .cloned()
                .collect::<Vec<_>>();
            contact_memberships = contact_memberships.saturating_add(memberships.len() as u64);
            self.repository
                .replace_contact_group_memberships(group_uid, memberships)
                .await?;
        }
        self.record_counter_step(
            sync_run_uid,
            Some(object.object_uid),
            "contact_groups_derived",
            StepOutcome::completed_with_counters(json!({
                    "contact_groups": contact_group_count,
                    "contact_group_memberships_changed": contact_memberships,
                    "records_ingested": 1,
            })),
            KnowledgeSyncCounters {
                records_ingested: 1,
                ..KnowledgeSyncCounters::default()
            },
        )
        .await?;
        Ok(PersistedIngestion {
            delta,
            embeddings_created: embeddings.len() as u64,
            ingested: true,
        })
    }

    pub(super) async fn semantic_graph_extractions(
        &self,
        tenant_id: moa_core::types::identifiers::TenantId,
        object: &KnowledgeObject,
        chunks: &[KnowledgeChunk],
    ) -> Result<SemanticGraphExtractionReport> {
        let chunk_hashes = chunks
            .iter()
            .map(|chunk| chunk.chunk_hash.clone())
            .collect::<Vec<_>>();
        // The active extractor's cache identity determines which cached rows a
        // lookup hits: the model-backed and deterministic extractors stamp
        // distinct `(model, prompt_version)` values, so switching between them
        // re-extracts instead of serving the other extractor's output.
        let identity = match &self.semantic_model_extractor {
            Some(extractor) => extractor.cache_identity(),
            None => SemanticExtractionCacheIdentity::deterministic(),
        };
        let cached = self
            .repository
            .cached_semantic_graph_extractions(
                tenant_id,
                &chunk_hashes,
                identity.schema_version,
                identity.model,
                identity.prompt_version,
            )
            .await?;
        let mut cached_by_hash = cached
            .into_iter()
            .map(|extraction| (extraction.chunk_hash.clone(), extraction))
            .collect::<HashMap<_, _>>();
        let mut cache_hits = 0_u64;
        let mut cache_misses = 0_u64;
        let mut extracted = Vec::with_capacity(chunks.len());
        let mut new_extractions = Vec::new();

        for chunk in chunks {
            if let Some(extraction) = cached_by_hash.remove(&chunk.chunk_hash) {
                cache_hits = cache_hits.saturating_add(1);
                extracted.push(extraction);
            } else {
                cache_misses = cache_misses.saturating_add(1);
                let extraction = self.extract_chunk(object, chunk).await;
                new_extractions.push(extraction.clone());
                extracted.push(extraction);
            }
        }
        self.repository
            .upsert_semantic_graph_extractions(tenant_id, new_extractions)
            .await?;

        Ok(SemanticGraphExtractionReport {
            cache_hits,
            cache_misses,
            entities_extracted: extracted
                .iter()
                .map(|extraction| extraction.entities.len() as u64)
                .sum(),
            relations_extracted: extracted
                .iter()
                .map(|extraction| extraction.relations.len() as u64)
                .sum(),
            semantic_chunk_links: semantic_chunk_link_count(chunks, &extracted) as u64,
            extractions: extracted,
        })
    }

    /// Extracts one chunk's semantics, preferring the model-backed extractor.
    ///
    /// When a model extractor is configured it is the production path; a model
    /// call, timeout, or parse failure falls back to the deterministic keyword
    /// extractor for this chunk (with a warning) so a single bad response never
    /// fails the sync run. Each returned extraction carries its own honest
    /// `model`/`prompt_version`, so a fallback is cached under the deterministic
    /// identity and re-attempted by the model on the next re-ingestion.
    async fn extract_chunk(
        &self,
        object: &KnowledgeObject,
        chunk: &KnowledgeChunk,
    ) -> SemanticGraphExtraction {
        if let Some(extractor) = &self.semantic_model_extractor {
            match extractor.extract(object, chunk).await {
                Ok(extraction) => return extraction,
                Err(error) => {
                    tracing::warn!(
                        tenant_id = %object.tenant_id,
                        object_id = %object.object_uid,
                        chunk_hash = %chunk.chunk_hash,
                        error = %error,
                        "semantic graph model extraction failed; falling back to deterministic extractor"
                    );
                }
            }
        }
        extract_chunk_semantics(object, chunk, self.semantic_generic_entities)
    }
}

/// Returns `metadata` as a JSON object with the `active` retrieval flag set.
///
/// Non-object metadata is replaced with an empty object before the flag is
/// inserted, so a freshly persisted chunk is always visible to active
/// retrieval.
fn mark_metadata_active(metadata: Value) -> Value {
    let mut object = match metadata {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    object.insert("active".to_string(), Value::Bool(true));
    Value::Object(object)
}

/// Builds the embedding input for one chunk with its document context.
///
/// Chunk text alone loses where the chunk sits: "Click Save" embeds
/// identically whether it ends a billing article or a DNS guide. Prepending
/// the document title and heading path (contextual retrieval) disambiguates
/// the vector without changing the stored chunk text, chunk hash, or the
/// evidence excerpt shown to the model. Only newly embedded chunks pick this
/// up — unchanged chunk hashes keep their cached vectors, so mixed corpora
/// converge as content changes; rebuild embeddings to convert wholesale.
fn contextual_embedding_input(
    document_title: Option<&str>,
    chunk: &crate::domain::KnowledgeChunk,
) -> String {
    let mut context = Vec::new();
    if let Some(title) = document_title {
        let title = title.trim();
        if !title.is_empty() {
            context.push(title.to_string());
        }
    }
    for heading in &chunk.heading_path {
        let heading = heading.trim();
        if !heading.is_empty() && context.last().map(String::as_str) != Some(heading) {
            context.push(heading.to_string());
        }
    }
    if context.is_empty() {
        return chunk.text.clone();
    }
    format!("{}\n\n{}", context.join(" > "), chunk.text)
}
