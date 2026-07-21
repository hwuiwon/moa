//! Root-turn request compilation and trusted sandbox manifest preparation.

use std::sync::Arc;

use moa_core::{
    error::MoaError, traits::Identity, traits::SessionStore as _,
    types::completion::CompletionRequest, types::hands::SandboxFile, types::identifiers::SessionId,
    types::tools::TrustedSandboxFileEntry, types::tools::TrustedSandboxFileManifestPayload,
    types::tools::TrustedSandboxFileManifestRef,
};
use moa_lineage_citation::ChunkRef;
use moa_lineage_core::TurnId;
use restate_sdk::prelude::*;
use sha2::{Digest, Sha256};

use crate::brain_bridge::{PreparedTurnRequest, QueryRewriteCacheEntry, prepare_turn_request};
use crate::turn_driver::progress as driver_progress;
use crate::workflows::errors::moa_error_to_handler_error;
use moa_session::PostgresSessionStore;

#[derive(Clone, Debug)]
pub(super) struct BuiltTurnRequest {
    pub(super) request: CompletionRequest,
    pub(super) active_canary: Option<String>,
    pub(super) trusted_sandbox_files: Vec<SandboxFile>,
    pub(super) trusted_sandbox_manifest: Option<TrustedSandboxFileManifestRef>,
    pub(super) citation_sources: Vec<ChunkRef>,
}

pub(super) async fn build_request_inside_workflow(
    ctx: &WorkflowContext<'_>,
    session_store: Arc<PostgresSessionStore>,
    session_id: SessionId,
    turn_id: TurnId,
    identity: Identity,
) -> Result<Option<BuiltTurnRequest>, HandlerError> {
    let active_user_sequence_num = ctx
        .get::<Json<u64>>(driver_progress::RootTurnStateKey::USER_MESSAGE_SEQUENCE)
        .await?
        .map(Json::into_inner);
    let cached_query_rewrite = ctx
        .get::<Json<QueryRewriteCacheEntry>>(driver_progress::RootTurnStateKey::QUERY_REWRITE_CACHE)
        .await?
        .map(Json::into_inner);
    let prepared = ctx
        .run(|| async move {
            prepare_turn_request(
                session_id,
                turn_id,
                identity,
                active_user_sequence_num,
                cached_query_rewrite,
            )
            .await
            .map(Json::from)
            .map_err(moa_error_to_handler_error)
        })
        .name("prepare_turn_request")
        .await?
        .into_inner();
    if let Some(cache) = prepared.query_rewrite_cache {
        ctx.set(
            driver_progress::RootTurnStateKey::QUERY_REWRITE_CACHE,
            Json::from(cache),
        );
    } else {
        ctx.clear(driver_progress::RootTurnStateKey::QUERY_REWRITE_CACHE);
    }

    Ok(match prepared.prepared {
        PreparedTurnRequest::Idle => None,
        PreparedTurnRequest::Request(request) => {
            let trusted_sandbox_manifest = store_trusted_sandbox_manifest(
                ctx,
                session_store,
                session_id,
                &prepared.trusted_sandbox_files,
            )
            .await?;
            Some(BuiltTurnRequest {
                request: *request,
                active_canary: prepared.active_canary,
                trusted_sandbox_files: prepared.trusted_sandbox_files,
                trusted_sandbox_manifest,
                citation_sources: prepared.citation_sources,
            })
        }
    })
}

async fn store_trusted_sandbox_manifest(
    ctx: &WorkflowContext<'_>,
    store: Arc<PostgresSessionStore>,
    session_id: SessionId,
    files: &[SandboxFile],
) -> Result<Option<TrustedSandboxFileManifestRef>, HandlerError> {
    if files.is_empty() {
        return Ok(None);
    }

    let payload = TrustedSandboxFileManifestPayload {
        files: files.to_vec(),
    };
    let payload_text = serde_json::to_string(&payload)
        .map_err(MoaError::from)
        .map_err(moa_error_to_handler_error)?;
    let manifest_sha256 = sha256_hex(payload_text.as_bytes());
    let entries = trusted_sandbox_file_entries(files);
    let claim_check = ctx
        .run(|| async move {
            store
                .store_text_artifact(session_id, &payload_text)
                .await
                .map(Json::from)
                .map_err(moa_error_to_handler_error)
        })
        .name("store_trusted_sandbox_file_manifest")
        .await?
        .into_inner();

    Ok(Some(TrustedSandboxFileManifestRef {
        blob_id: claim_check.blob_id,
        size: claim_check.size,
        manifest_sha256,
        files: entries,
    }))
}

fn trusted_sandbox_file_entries(files: &[SandboxFile]) -> Vec<TrustedSandboxFileEntry> {
    files
        .iter()
        .map(|file| TrustedSandboxFileEntry {
            path: file.path.clone(),
            content_sha256: sha256_hex(&file.content),
            size: file.content.len(),
            executable: file.executable,
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
