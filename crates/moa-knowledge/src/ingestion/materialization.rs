//! Knowledge ingestion materialization operations.

use super::steps::record_span_outcome;
use super::*;

impl KnowledgeIngestionPipeline {
    pub(super) async fn persist_parsed(
        &self,
        sync_run_uid: Uuid,
        object: KnowledgeObject,
        parsed: ParsedDocument,
    ) -> Result<PersistedIngestion> {
        let content_hash = content_hash(&normalize_text(&parsed.text));
        let latest_version = self
            .ingestion_repository
            .latest_document_version(object.object_uid)
            .await?;
        let latest_chunks = if let Some(latest) = &latest_version {
            self.ingestion_repository
                .chunks_for_version(latest.version_uid)
                .await?
        } else {
            Vec::new()
        };
        let latest_version_completed = if let Some(latest) = &latest_version {
            latest.content_hash == content_hash
                && self
                    .ingestion_repository
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
            .ingestion_repository
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
            .ingestion_repository
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
            self.ingestion_repository
                .complete_document_version_ingestion(sync_run_uid, version_uid, claim_token)
                .await?;
        } else {
            self.ingestion_repository
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
        self.ingestion_repository
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
        let delta = document_chunk_delta(&object, &version, &chunks);
        // Fold the `active` retrieval marker into the rows before the batch
        // insert. Graph identity needs no folding: `chunk_uid` is the occurrence
        // node uid, and `replace_chunks` persists it into `graph_node_uid`.
        let mut chunks = chunks;
        for chunk in &mut chunks {
            chunk.metadata = mark_metadata_active(std::mem::take(&mut chunk.metadata));
        }
        self.ingestion_repository
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
        self.ingestion_repository
            .tombstone_chunks(&tombstones)
            .await?;
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
            self.contact_group_repository
                .upsert_contact_group(group)
                .await?;
            let memberships = contact_delta
                .memberships
                .iter()
                .filter(|membership| membership.group_uid == group_uid)
                .cloned()
                .collect::<Vec<_>>();
            contact_memberships = contact_memberships.saturating_add(memberships.len() as u64);
            self.contact_group_repository
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
/// evidence excerpt shown to the model. Unchanged chunk hashes keep their
/// cached vectors, so a replacement partition must be re-ingested through this
/// same path before it serves queries.
///
/// The format itself lives in `moa_core` so ingestion and vector backends share
/// one exact representation without depending on each other.
fn contextual_embedding_input(
    document_title: Option<&str>,
    chunk: &crate::domain::KnowledgeChunk,
) -> String {
    moa_core::types::memory::contextual_chunk_embedding_input(
        document_title,
        &chunk.heading_path,
        &chunk.text,
    )
}
