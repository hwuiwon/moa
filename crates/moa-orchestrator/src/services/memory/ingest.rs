//! Document-ingestion request orchestration for the memory service.

use std::time::Instant;

use chrono::Utc;
use moa_core::wire::memory::{MemoryIngestDocument, MemoryIngestRequest, MemoryIngestResponse};
use moa_core::{types::contact::ContactId, types::identifiers::SessionId};
use moa_memory_ingest::{IngestionVOClient, SessionTurn, ingestion_object_key};
use moa_observability::record_memory_operation;
use restate_sdk::prelude::*;
use uuid::{Builder as UuidBuilder, Variant, Version};

use super::responses::ingest_result_from_report;

/// Maximum number of document-ingestion virtual-object calls kept in flight at once.
///
/// Bounds fan-out so a large batch cannot open an unbounded number of concurrent
/// `Ingestion/ingest_turn` calls, while still overlapping the per-document graph-memory
/// ingestion latency that dominates this handler. Documents are processed in chunks of
/// this size; four keeps the win without stressing the graph store's connection pool.
const INGEST_MAX_CONCURRENCY: usize = 4;

/// Ingests memory documents through the ingestion virtual object.
///
/// Documents are dispatched with bounded concurrency ([`INGEST_MAX_CONCURRENCY`]) rather
/// than strictly serially. This is replay-safe because every action still goes through a
/// journaled `ctx` step whose journal position is deterministic: within each chunk the
/// per-document prepare `ctx.run` steps run in order, then the ingest call futures are
/// created in document order (each `ctx.call` journals its Call command at creation, not at
/// completion), and the chunk is drained with [`DurableFuturesUnordered`], whose `next()`
/// journals the completion order through the SDK's `select` combinator. On replay the
/// recorded completion order is reused, so results are matched back to their stable
/// document slot regardless of the wall-clock order in which the calls actually finish. A
/// raw `join_all`/`buffer_unordered` fan-out is *not* used: those poll the durable futures
/// in an arbitrary order without journaling the completion sequence, which is exactly what
/// `DurableFuturesUnordered` exists to make deterministic.
pub(super) async fn ingest_documents_inner(
    ctx: &Context<'_>,
    request: MemoryIngestRequest,
    contact_id: Option<ContactId>,
) -> Result<MemoryIngestResponse, HandlerError> {
    let started = Instant::now();
    let tenant_id = request.tenant_id;
    let information_barrier = request.information_barrier.clone();
    let mut results = Vec::with_capacity(request.documents.len());
    // Enumerate the whole batch first so each document keeps its stable global index
    // (used for the synthetic session id and turn sequence) regardless of chunk boundaries.
    let indexed: Vec<(usize, MemoryIngestDocument)> =
        request.documents.into_iter().enumerate().collect();

    for chunk in indexed.chunks(INGEST_MAX_CONCURRENCY) {
        // Phase 1: prepare each turn serially. The prepare step is a cheap journaled
        // `ctx.run` (hash + timestamp), so keeping it ordered gives a deterministic journal
        // prefix before the concurrent dispatch.
        let mut prepared: Vec<(String, SessionTurn)> = Vec::with_capacity(chunk.len());
        for (index, document) in chunk {
            let index = *index;
            let source_name = document.source_name.clone();
            let content = document.content.clone();
            let information_barrier = information_barrier.clone();
            let session_id = document_ingest_session_id(tenant_id, contact_id, index, document);
            let turn = ctx
                .run(|| async move {
                    Ok(Json(SessionTurn {
                        tenant_id,
                        contact_id,
                        session_id,
                        turn_seq: u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
                        transcript: ingest_transcript(&source_name, &content),
                        dominant_pii_class: "none".to_string(),
                        finalized_at: Utc::now(),
                        barrier: information_barrier,
                    }))
                })
                .name(format!("memory_ingest_prepare_{index}"))
                .await?
                .into_inner();
            prepared.push((document.source_name.clone(), turn));
        }

        // Phase 2: dispatch the chunk's ingest calls concurrently. Push order (document
        // order) is the stable slot returned by `next()`, so out-of-order completion still
        // reassembles into document order below.
        let source_names: Vec<String> = prepared.iter().map(|(name, _)| name.clone()).collect();
        let mut inflight = DurableFuturesUnordered::new();
        for (_, turn) in prepared {
            let key = ingestion_object_key(&turn);
            inflight.push(
                crate::restate_identity::replay_safe_request(
                    ctx.object_client::<IngestionVOClient>(key)
                        .ingest_turn(Json(turn)),
                )
                .call(),
            );
        }

        let mut reports: Vec<Option<_>> = (0..source_names.len()).map(|_| None).collect();
        while let Some((slot, report)) = inflight.next().await? {
            reports[slot] = Some(report?.into_inner());
        }

        for (source_name, report) in source_names.into_iter().zip(reports) {
            let report = report.ok_or_else(|| {
                HandlerError::from(TerminalError::new(
                    "document ingest fan-in dropped a result before completion",
                ))
            })?;
            results.push(ingest_result_from_report(source_name, report));
        }
    }

    record_memory_operation(
        "ingest_documents",
        "success",
        results.len() as u64,
        started.elapsed(),
    );

    Ok(MemoryIngestResponse { tenant_id, results })
}

fn ingest_transcript(source_name: &str, content: &str) -> String {
    format!("source: {source_name}\n\n{content}")
}

/// Derives the synthetic session id for one document-ingestion turn.
#[must_use]
pub fn document_ingest_session_id(
    tenant_id: moa_core::types::identifiers::TenantId,
    contact_id: Option<ContactId>,
    index: usize,
    document: &MemoryIngestDocument,
) -> SessionId {
    let mut hasher = blake3::Hasher::new();
    update_hash_field(&mut hasher, "kind", "memory_ingest_document:v1");
    update_hash_field(&mut hasher, "tenant_id", &tenant_id.to_string());
    let owner = contact_id
        .map(|contact_id| contact_id.to_string())
        .unwrap_or_else(|| "tenant".to_string());
    update_hash_field(&mut hasher, "owner", &owner);
    update_hash_field(&mut hasher, "index", &index.to_string());
    update_hash_field(&mut hasher, "source_name", &document.source_name);
    update_hash_field(
        &mut hasher,
        "source_uri",
        document.source_uri.as_deref().unwrap_or(""),
    );
    update_hash_field(&mut hasher, "content", &document.content);

    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    let uuid = UuidBuilder::from_bytes(bytes)
        .with_variant(Variant::RFC4122)
        .with_version(Version::Custom)
        .into_uuid();
    SessionId(uuid)
}

fn update_hash_field(hasher: &mut blake3::Hasher, key: &str, value: &str) {
    hasher.update(key.as_bytes());
    hasher.update(&[0]);
    hasher.update(value.len().to_string().as_bytes());
    hasher.update(&[0]);
    hasher.update(value.as_bytes());
    hasher.update(&[0xff]);
}
