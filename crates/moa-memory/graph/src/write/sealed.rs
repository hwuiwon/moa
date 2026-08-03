//! Restricted node-content preparation performed before graph SQL mutations.

use std::collections::BTreeMap;

use moa_crypto::{EncryptionContext, EncryptionRequest};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{Error, PostgresGraphStore, Result, node::NodeWriteIntent};

/// Placeholder written into the indexed plaintext `name` column of a
/// restricted/PHI node, so the generated `name_tsv` full-text index only ever
/// sees this token and never the sealed secret.
pub(crate) const REDACTED_NAME_PLACEHOLDER: &str = "[RESTRICTED]";

/// Placeholder written into the indexed plaintext `properties_summary` column of
/// a restricted/PHI node, keeping the generated `properties_tsv` index free of
/// the sealed secret.
fn redacted_properties() -> Value {
    json!({ "redacted": true })
}

/// Version of the plaintext document stored inside `content_sealed`.
pub(crate) const SEALED_CONTENT_VERSION: u8 = 1;

/// Complete mutable content encrypted as one atomic document.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct SealedNodeContent {
    /// Payload format version.
    pub(crate) version: u8,
    /// Human-readable node name.
    pub(crate) name: String,
    /// Dynamic node properties.
    pub(crate) properties: Value,
}

/// The indexed plaintext columns plus any sealed ciphertext for one node write.
///
/// Produced by [`prepare_node_fields`] in the intent-prep phase and consumed by
/// the node-row writer. For `none`/`pii` nodes it carries the real name and
/// properties with no ciphertext; for `restricted`/`phi` nodes it carries the
/// redaction placeholders plus the sealed blobs and flags the embedding for
/// exclusion.
pub(super) struct PreparedNodeFields {
    /// Value bound into the indexed plaintext `name` column.
    pub(super) name: String,
    /// Value bound into the indexed plaintext `properties_summary` column.
    pub(super) properties: Value,
    /// Envelope ciphertext of the complete content document, or `None`.
    pub(super) content_sealed: Option<Vec<u8>>,
}

/// Seals one node's restricted/PHI content ahead of the SQL transaction.
///
/// This is the intent-prep step: it performs async KMS + AEAD work only and
/// touches no database rows, so it can run before `begin_required()` and must
/// never participate in row-lock ordering or the bulk deadlock-retry loop. For
/// `none`/`pii` nodes it is a cheap identity (no crypto). For `restricted`/`phi`
/// nodes it seals one versioned `{name, properties}` payload under the node's
/// explicit `(tenant, data_subject_id)` KEK and substitutes redaction
/// placeholders into the indexed plaintext columns. Restricted content with an
/// embedding is rejected rather than silently dropping caller input.
pub(super) async fn prepare_node_fields(
    store: &PostgresGraphStore,
    intent: &NodeWriteIntent,
    tenant_id: Uuid,
) -> Result<PreparedNodeFields> {
    prepare_node_fields_batch(store, std::slice::from_ref(intent), &[tenant_id])
        .await?
        .pop()
        .ok_or_else(|| Error::Conflict("node preparation returned no fields".to_string()))
}

/// Prepares a node batch and performs one KMS call per `(tenant, subject)` group.
pub(super) async fn prepare_node_fields_batch(
    store: &PostgresGraphStore,
    intents: &[NodeWriteIntent],
    tenant_ids: &[Uuid],
) -> Result<Vec<PreparedNodeFields>> {
    if intents.len() != tenant_ids.len() {
        return Err(Error::Conflict(
            "node preparation tenant cardinality mismatch".to_string(),
        ));
    }

    let mut prepared = intents
        .iter()
        .map(|intent| {
            if intent.pii_class.is_sealed() {
                None
            } else {
                Some(PreparedNodeFields {
                    name: intent.name.clone(),
                    properties: intent.properties.clone(),
                    content_sealed: None,
                })
            }
        })
        .collect::<Vec<_>>();
    let mut groups: BTreeMap<(Uuid, Uuid), Vec<(usize, EncryptionRequest)>> = BTreeMap::new();

    for (index, (intent, tenant_id)) in intents.iter().zip(tenant_ids).enumerate() {
        if !intent.pii_class.is_sealed() {
            continue;
        }
        if intent.embedding.is_some() {
            return Err(Error::SealedEmbedding);
        }
        let payload = serde_json::to_vec(&SealedNodeContent {
            version: SEALED_CONTENT_VERSION,
            name: intent.name.clone(),
            properties: intent.properties.clone(),
        })?;
        let context = EncryptionContext::new(
            *tenant_id,
            intent.data_subject_id,
            intent.uid.to_string(),
            intent.pii_class.as_str(),
        );
        groups
            .entry((*tenant_id, intent.data_subject_id))
            .or_default()
            .push((index, EncryptionRequest::new(payload, context)));
    }

    for requests in groups.into_values() {
        let encryption_requests = requests
            .iter()
            .map(|(_, request)| request.clone())
            .collect::<Vec<_>>();
        let ciphertexts =
            moa_crypto::encrypt_batch(store.kms().as_ref(), &encryption_requests).await?;
        for ((index, _), ciphertext) in requests.into_iter().zip(ciphertexts) {
            prepared[index] = Some(PreparedNodeFields {
                name: REDACTED_NAME_PLACEHOLDER.to_string(),
                properties: redacted_properties(),
                content_sealed: Some(ciphertext.to_bytes()),
            });
        }
    }

    prepared
        .into_iter()
        .map(|fields| {
            fields.ok_or_else(|| {
                Error::Conflict("sealed node preparation returned no fields".to_string())
            })
        })
        .collect()
}
