//! Multi-chunk Slack send and edit orchestration.

use async_trait::async_trait;
use moa_core::{error::MoaError, error::Result, types::channel::MessageId};
use tracing::warn;

use crate::renderer::SlackRenderChunk;

use super::refs::{SlackMessageRef, SlackOutboundMessageRefs, SlackTarget};

/// Chunk-level Slack transport operations used by the tracked send and edit
/// orchestration.
///
/// Extracting the three chunk-level API calls behind a trait lets the
/// multi-chunk reference-persistence and compensation logic be exercised
/// deterministically without a live Slack endpoint.
#[async_trait]
pub(super) trait SlackChunkTransport: Send + Sync {
    /// Posts one rendered chunk and returns its durable Slack reference.
    async fn send_chunk(
        &self,
        target: &SlackTarget,
        chunk: &SlackRenderChunk,
    ) -> Result<SlackMessageRef>;

    /// Updates one already-sent chunk in place.
    async fn update_chunk(
        &self,
        message_ref: &SlackMessageRef,
        chunk: &SlackRenderChunk,
    ) -> Result<()>;

    /// Deletes one already-sent chunk, tolerating an already-absent message.
    async fn delete_ref(&self, message_ref: &SlackMessageRef) -> Result<()>;
}

/// Sends a fresh multi-chunk Slack message, persisting each confirmed chunk
/// reference before the next send and compensating on a mid-message failure.
///
/// Recording references only after every chunk succeeds leaves a middle-chunk
/// error with earlier chunks visible in Slack but untracked in the reference
/// store, so retry, edit, and delete cannot repair them. This persists each
/// confirmed reference immediately and, because the caller supplies no
/// idempotency key to resume under, deletes the already-sent chunks on failure
/// so Slack never shows a visible-but-untracked partial message.
pub(super) async fn send_multi_chunk_tracked<T: SlackChunkTransport + ?Sized>(
    transport: &T,
    refs: &SlackOutboundMessageRefs,
    target: &SlackTarget,
    message_id: &MessageId,
    chunks: &[SlackRenderChunk],
) -> Result<Vec<SlackMessageRef>> {
    let mut sent_refs = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        match transport.send_chunk(target, chunk).await {
            Ok(sent_ref) => {
                sent_refs.push(sent_ref);
                refs.record_after_external_side_effect(
                    message_id,
                    sent_refs.clone(),
                    "chat.postMessage",
                )
                .await;
            }
            Err(error) => {
                compensate_partial_send(transport, refs, message_id, &sent_refs).await;
                return Err(error);
            }
        }
    }
    Ok(sent_refs)
}

/// Deletes the already-sent chunks and clears the partial reference record after
/// a multi-chunk send fails partway through, so no visible-but-untracked message
/// remains in Slack.
async fn compensate_partial_send<T: SlackChunkTransport + ?Sized>(
    transport: &T,
    refs: &SlackOutboundMessageRefs,
    message_id: &MessageId,
    sent_refs: &[SlackMessageRef],
) {
    for message_ref in sent_refs {
        if let Err(error) = transport.delete_ref(message_ref).await {
            warn!(
                message_id = %message_id,
                slack.channel_id = %message_ref.channel_id,
                slack.ts = %message_ref.ts,
                error = %error,
                "Slack multi-chunk send failed; compensating delete of an already-sent chunk also failed"
            );
        }
    }
    refs.remove_after_external_side_effect(message_id, "chat.postMessage_compensation")
        .await;
}

/// Applies an edit to an existing Slack message, persisting each newly grown
/// chunk reference incrementally.
///
/// When the rendered message grows, new chunks are sent before the updated
/// reference list would historically be persisted, so a late failure left the
/// new chunks visible but untracked. Persisting after each newly-sent chunk
/// keeps every confirmed chunk tracked even if a later chunk fails.
pub(super) async fn apply_edit_tracked<T: SlackChunkTransport + ?Sized>(
    transport: &T,
    refs: &SlackOutboundMessageRefs,
    message_id: &MessageId,
    existing: &[SlackMessageRef],
    rendered: &[SlackRenderChunk],
) -> Result<Vec<SlackMessageRef>> {
    let overlap = existing.len().min(rendered.len());
    let mut updated_refs = Vec::with_capacity(rendered.len());

    for index in 0..overlap {
        let message_ref = existing[index].clone();
        transport
            .update_chunk(&message_ref, &rendered[index])
            .await?;
        updated_refs.push(message_ref);
    }

    if rendered.len() > existing.len() {
        let target = existing
            .last()
            .cloned()
            .map(|message_ref| message_ref.target())
            .ok_or_else(|| {
                MoaError::ValidationError(format!("slack message id {message_id} has no refs"))
            })?;
        for chunk in rendered.iter().skip(existing.len()) {
            let sent_ref = transport.send_chunk(&target, chunk).await?;
            updated_refs.push(sent_ref);
            // Persist each newly-sent chunk before the next send so a mid-growth
            // failure still leaves the chunk tracked, never visible-but-untracked.
            refs.record_after_external_side_effect(message_id, updated_refs.clone(), "chat.update")
                .await;
        }
    }

    if existing.len() > rendered.len() {
        for message_ref in existing.iter().skip(rendered.len()) {
            transport.delete_ref(message_ref).await?;
        }
    }

    refs.record_after_external_side_effect(message_id, updated_refs.clone(), "chat.update")
        .await;
    Ok(updated_refs)
}
