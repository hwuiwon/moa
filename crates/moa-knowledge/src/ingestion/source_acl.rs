//! Provider ACL capture, placed ahead of every content fence.
//!
//! Ingestion is full of skip fences — an unchanged change token, an unchanged
//! content hash — that exist so a sync does not re-parse and re-embed a document
//! nobody edited. Those fences are about *content*, and permissions change
//! independently of content: a folder is unshared without a byte moving.
//!
//! So the ACL is captured first, unconditionally, on every record. A permission
//! change therefore takes effect on the very next sync pass, flipping visibility
//! and bumping the tenant ACL epoch (which invalidates warm retrieval caches)
//! without paying for a parse or an embedding.

use super::*;
use crate::domain::ProviderAclSnapshot;

impl<R, P, E, G> KnowledgeIngestionPipeline<R, P, E, G>
where
    R: KnowledgeRepository,
    P: DocumentParser,
    E: EmbeddingProvider,
    G: KnowledgeGraphWriter,
{
    /// Captures one record's provider ACL and atomically makes it current.
    ///
    /// Runs before the change-token and content-hash fences. Returns a typed
    /// error for a permission-bearing record whose ACL could not be enumerated —
    /// but only *after* the incomplete capture has been recorded, so the object
    /// is already hidden by the time the error propagates.
    pub(super) async fn capture_record_acl(
        &self,
        sync_run_uid: Uuid,
        object: &KnowledgeObject,
        record: &ProviderRecord,
    ) -> Result<()> {
        let provider_acl = &record.acl;

        // Entries are already keyed and the adapter removed the readable
        // provider identities before returning this record.
        let snapshot = ProviderAclSnapshot::normalized(
            object.tenant_id,
            object.connection_uid,
            object.object_uid,
            provider_acl.provider_revision.clone(),
            provider_acl.complete,
            provider_acl.entries.clone(),
            Utc::now(),
        )?;
        let entry_count = snapshot.entries.len();
        let complete = snapshot.complete;

        // Persist first. An incomplete capture still lands, which is what moves
        // the object to `incomplete` and hides it; only then does the typed error
        // propagate. Reversing that order would leave a revoked document
        // retrievable for as long as the sync kept failing.
        self.repository
            .replace_object_acl_snapshot(snapshot)
            .await?;

        if !complete {
            let error = Error::Provider {
                provider: self.provider.clone(),
                message: format!(
                    "provider returned an incomplete ACL for source `{}`; its content is hidden \
                     until a complete permission listing is captured",
                    object.source_id
                ),
            };
            self.record_failure_step(
                sync_run_uid,
                Some(object.object_uid),
                "source_acl_captured",
                &error,
            )
            .await?;
            return Err(error);
        }

        self.record_step(
            sync_run_uid,
            Some(object.object_uid),
            "source_acl_captured",
            StepOutcome::completed_with_counters(json!({ "acl_entries": entry_count })),
        )
        .await?;
        Ok(())
    }
}
