//! Document-ingestion request orchestration for the memory service.

use std::time::Instant;

use chrono::Utc;
use moa_core::wire::memory::{MemoryIngestDocument, MemoryIngestRequest, MemoryIngestResponse};
use moa_core::{ContactId, SessionId};
use moa_memory_ingest::{IngestionVOClient, SessionTurn, ingestion_object_key};
use moa_observability::record_memory_operation;
use restate_sdk::prelude::*;
use uuid::{Builder as UuidBuilder, Variant, Version};

use super::responses::ingest_result_from_report;

/// Ingests memory documents through the ingestion virtual object.
pub(super) async fn ingest_documents_inner(
    ctx: &Context<'_>,
    request: MemoryIngestRequest,
    contact_id: Option<ContactId>,
) -> Result<MemoryIngestResponse, HandlerError> {
    let started = Instant::now();
    let mut results = Vec::with_capacity(request.documents.len());
    for (index, document) in request.documents.into_iter().enumerate() {
        let source_name = document.source_name.clone();
        let content = document.content.clone();
        let tenant_id = request.tenant_id;
        let session_id = document_ingest_session_id(tenant_id, contact_id, index, &document);
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
                }))
            })
            .name(format!("memory_ingest_prepare_{index}"))
            .await?
            .into_inner();
        let report = ctx
            .object_client::<IngestionVOClient>(ingestion_object_key(&turn))
            .ingest_turn(Json(turn))
            .call()
            .await?
            .into_inner();
        results.push(ingest_result_from_report(document.source_name, report));
    }
    record_memory_operation(
        "ingest_documents",
        "success",
        results.len() as u64,
        started.elapsed(),
    );

    Ok(MemoryIngestResponse {
        tenant_id: request.tenant_id,
        results,
    })
}

fn ingest_transcript(source_name: &str, content: &str) -> String {
    format!("source: {source_name}\n\n{content}")
}

/// Derives the synthetic session id for one document-ingestion turn.
#[must_use]
pub fn document_ingest_session_id(
    tenant_id: moa_core::TenantId,
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
