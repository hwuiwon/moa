//! Bounded chunk encryption for durable streaming objects.
//!
//! A checkpoint receives one fresh KMS data-encryption key. Each bounded chunk
//! is sealed independently with a unique random nonce, while authenticated
//! metadata binds it to its tenant, workspace, checkpoint, format, position,
//! and plaintext digest. The plaintext DEK is zeroized when publication ends.

use std::collections::HashSet;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    Error, KeyHandle, KeyManagementProvider, NONCE_LEN, WrappedDek, aead,
    types::{EncryptionContext, GeneratedDataKey},
};

/// Stable portable-checkpoint encryption context classification.
pub const CHECKPOINT_ENCRYPTION_CLASS: &str = "sandbox_workspace_checkpoint";

/// Identity bound into the wrapped DEK and every encrypted chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkEncryptionContext {
    /// Immutable tenant owner.
    pub tenant_id: Uuid,
    /// Durable workspace used as the KMS data-subject identity.
    pub workspace_id: Uuid,
    /// Immutable checkpoint identity.
    pub checkpoint_id: Uuid,
    /// Portable checkpoint format version.
    pub format_version: u16,
}

impl ChunkEncryptionContext {
    fn kms_context(self) -> EncryptionContext {
        EncryptionContext::new(
            self.tenant_id,
            self.workspace_id,
            self.checkpoint_id.to_string(),
            CHECKPOINT_ENCRYPTION_CLASS,
        )
    }

    fn chunk_aad(self, index: u32, plaintext_digest: &[u8; 32]) -> Vec<u8> {
        let mut aad = Vec::with_capacity(24 + 16 * 3 + 2 + 4 + 32);
        aad.extend_from_slice(b"moa/checkpoint-chunk/v1");
        aad.extend_from_slice(self.tenant_id.as_bytes());
        aad.extend_from_slice(self.workspace_id.as_bytes());
        aad.extend_from_slice(self.checkpoint_id.as_bytes());
        aad.extend_from_slice(&self.format_version.to_be_bytes());
        aad.extend_from_slice(&index.to_be_bytes());
        aad.extend_from_slice(plaintext_digest);
        aad
    }
}

/// One independently authenticated encrypted chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedChunk {
    /// Zero-based chunk position.
    pub index: u32,
    /// SHA-256 digest of the plaintext chunk.
    pub plaintext_digest: [u8; 32],
    /// Unique AES-GCM nonce under this checkpoint's DEK.
    pub nonce: [u8; NONCE_LEN],
    /// Ciphertext with its authentication tag.
    pub ciphertext: Vec<u8>,
}

/// One checkpoint envelope using exactly one wrapped DEK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkedEnvelope {
    /// KMS-wrapped checkpoint DEK.
    pub wrapped_dek: WrappedDek,
    /// Durable KMS key handle needed for unwrap and crypto-shred.
    pub key_handle: KeyHandle,
    /// Encrypted chunks in exact index order.
    pub chunks: Vec<EncryptedChunk>,
}

/// Encrypts bounded chunks under one fresh checkpoint DEK.
///
/// Empty input is supported and still receives a fresh wrapped DEK, allowing an
/// empty workspace checkpoint to remain a first-class immutable revision.
pub async fn encrypt_chunks<K>(
    kms: &K,
    context: ChunkEncryptionContext,
    chunks: &[Vec<u8>],
    max_chunk_bytes: usize,
) -> Result<ChunkedEnvelope, Error>
where
    K: KeyManagementProvider + ?Sized,
{
    if max_chunk_bytes == 0 {
        return Err(Error::InvalidBatch(
            "chunk size limit must be greater than zero".to_string(),
        ));
    }
    if chunks.len() > u32::MAX as usize {
        return Err(Error::InvalidBatch(
            "checkpoint has more chunks than the format can address".to_string(),
        ));
    }
    if let Some(chunk) = chunks.iter().find(|chunk| chunk.len() > max_chunk_bytes) {
        return Err(Error::InvalidBatch(format!(
            "checkpoint chunk is {} bytes, exceeding the {max_chunk_bytes}-byte limit",
            chunk.len()
        )));
    }

    let GeneratedDataKey {
        plaintext,
        wrapped,
        handle,
    } = kms.generate_data_key(&context.kms_context()).await?;
    let mut nonces = HashSet::with_capacity(chunks.len());
    let mut encrypted = Vec::with_capacity(chunks.len());

    for (position, chunk) in chunks.iter().enumerate() {
        let index = u32::try_from(position)
            .map_err(|_| Error::InvalidBatch("checkpoint chunk index exceeds u32".to_string()))?;
        let plaintext_digest: [u8; 32] = Sha256::digest(chunk).into();
        let nonce = loop {
            let candidate = aead::random_nonce();
            if nonces.insert(candidate) {
                break candidate;
            }
        };
        let aad = context.chunk_aad(index, &plaintext_digest);
        let ciphertext = aead::seal(plaintext.expose(), &nonce, chunk, &aad)?;
        encrypted.push(EncryptedChunk {
            index,
            plaintext_digest,
            nonce,
            ciphertext,
        });
    }

    Ok(ChunkedEnvelope {
        wrapped_dek: wrapped,
        key_handle: handle,
        chunks: encrypted,
    })
}

/// Decrypts and verifies all chunks in exact index order.
pub async fn decrypt_chunks<K>(
    kms: &K,
    context: ChunkEncryptionContext,
    envelope: &ChunkedEnvelope,
    max_chunk_bytes: usize,
) -> Result<Vec<Vec<u8>>, Error>
where
    K: KeyManagementProvider + ?Sized,
{
    if max_chunk_bytes == 0 {
        return Err(Error::InvalidBatch(
            "chunk size limit must be greater than zero".to_string(),
        ));
    }
    let plaintext = kms
        .decrypt_data_key(
            &envelope.wrapped_dek,
            &envelope.key_handle,
            &context.kms_context(),
        )
        .await?;
    let mut seen_nonces = HashSet::with_capacity(envelope.chunks.len());
    let mut opened = Vec::with_capacity(envelope.chunks.len());

    for (position, chunk) in envelope.chunks.iter().enumerate() {
        let expected_index = u32::try_from(position)
            .map_err(|_| Error::InvalidBatch("checkpoint chunk index exceeds u32".to_string()))?;
        if chunk.index != expected_index || !seen_nonces.insert(chunk.nonce) {
            return Err(Error::MalformedCiphertext);
        }
        let aad = context.chunk_aad(chunk.index, &chunk.plaintext_digest);
        let bytes = aead::open(plaintext.expose(), &chunk.nonce, &chunk.ciphertext, &aad)?;
        if bytes.len() > max_chunk_bytes
            || <[u8; 32]>::from(Sha256::digest(&bytes)) != chunk.plaintext_digest
        {
            return Err(Error::MalformedCiphertext);
        }
        opened.push(bytes);
    }
    Ok(opened)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LocalKmsProvider;

    fn context() -> ChunkEncryptionContext {
        ChunkEncryptionContext {
            tenant_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            checkpoint_id: Uuid::new_v4(),
            format_version: 1,
        }
    }

    // Pins: all bounded chunks round-trip under one wrapped key while receiving
    // unique nonces and position-bound authentication.
    #[tokio::test]
    async fn chunked_round_trip_uses_one_dek_and_unique_nonces_offline() {
        let kms = LocalKmsProvider::new();
        let context = context();
        let chunks = vec![b"first".to_vec(), b"second".to_vec(), Vec::new()];

        let envelope = encrypt_chunks(&kms, context, &chunks, 64)
            .await
            .expect("bounded chunks should encrypt");
        let opened = decrypt_chunks(&kms, context, &envelope, 64)
            .await
            .expect("matching checkpoint context should decrypt");

        assert_eq!(opened, chunks);
        assert_eq!(envelope.chunks.len(), 3);
        assert_ne!(envelope.chunks[0].nonce, envelope.chunks[1].nonce);
        assert_ne!(envelope.chunks[1].nonce, envelope.chunks[2].nonce);
    }

    // Pins: ciphertext cannot be reordered between chunk positions even when
    // all other checkpoint identity fields match.
    #[tokio::test]
    async fn chunk_reordering_fails_authentication_offline() {
        let kms = LocalKmsProvider::new();
        let context = context();
        let mut envelope =
            encrypt_chunks(&kms, context, &[b"first".to_vec(), b"second".to_vec()], 64)
                .await
                .expect("bounded chunks should encrypt");
        envelope.chunks.swap(0, 1);

        let error = decrypt_chunks(&kms, context, &envelope, 64)
            .await
            .expect_err("reordered chunks must be rejected");

        assert!(matches!(error, Error::MalformedCiphertext));
    }

    // Pins: callers cannot bypass the configured plaintext memory bound.
    #[tokio::test]
    async fn oversized_chunk_is_rejected_before_kms_use_offline() {
        let kms = LocalKmsProvider::new();
        let error = encrypt_chunks(&kms, context(), &[vec![0; 65]], 64)
            .await
            .expect_err("oversized chunks must be rejected");

        assert!(matches!(error, Error::InvalidBatch(_)));
    }
}
