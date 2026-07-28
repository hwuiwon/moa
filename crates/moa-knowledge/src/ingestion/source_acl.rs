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
use crate::domain::{ConnectionAclMode, ProviderAclCapability, ProviderAclSnapshot, RecordAcl};

/// The ACL identity one ingestion run writes under.
///
/// Required at construction: a pipeline that could be built without it would be
/// a pipeline that ingests provider content while having no opinion about who
/// may read it.
pub struct KnowledgeSourceAclContext {
    /// The connection's admission mode, derived from `capability`.
    mode: ConnectionAclMode,
    /// The adapter's declared capability, used to validate every record's ACL.
    capability: ProviderAclCapability,
}

impl KnowledgeSourceAclContext {
    /// Derives one run's admission mode from the adapter's declared capability.
    ///
    /// The mode is *derived*, never passed in. That is deliberate: an operator
    /// or a caller choosing the mode is exactly how a permission-bearing
    /// connector ends up tenant-public, so the downgrade is made
    /// unrepresentable rather than validated and rejected.
    ///
    /// No fingerprint key is needed here: principals were already keyed by the
    /// adapter, so the ingestion pipeline never touches a raw identity.
    #[must_use]
    pub fn for_capability(capability: ProviderAclCapability) -> Self {
        Self {
            mode: capability.required_mode(),
            capability,
        }
    }

    /// Returns the connection's admission mode.
    #[must_use]
    pub fn mode(&self) -> ConnectionAclMode {
        self.mode
    }

    /// Returns the adapter's declared capability.
    #[must_use]
    pub fn capability(&self) -> ProviderAclCapability {
        self.capability
    }
}

impl std::fmt::Debug for KnowledgeSourceAclContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("KnowledgeSourceAclContext")
            .field("mode", &self.mode)
            .field("capability", &self.capability)
            .finish()
    }
}

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
        record
            .acl
            .validate_for(&self.provider, self.source_acl.capability())?;

        let provider_acl = match &record.acl {
            // Nothing to capture: the connector has no per-record permissions, so
            // the connection's `tenant_public` mode is the whole answer.
            RecordAcl::UniformlyPublic => {
                self.record_step(
                    sync_run_uid,
                    Some(object.object_uid),
                    "source_acl_captured",
                    StepOutcome::completed_with_counters_and_summary(
                        json!({ "acl_entries": 0 }),
                        "connector is uniformly public inside the tenant",
                    ),
                )
                .await?;
                return Ok(());
            }
            RecordAcl::Provider(provider_acl) => provider_acl,
        };

        // Entries are already keyed: the adapter fingerprinted each principal as
        // it normalized the payload, so nothing here has ever held a readable
        // provider identity.
        let snapshot = ProviderAclSnapshot::normalized(
            snapshot_uid(object.object_uid, &provider_acl.provider_revision),
            object.tenant_id,
            object.connection_uid,
            object.object_uid,
            provider_acl.provider_revision.clone(),
            provider_acl.provenance,
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

/// Derives a deterministic snapshot identifier for one object revision.
///
/// Deterministic so a replayed sync converges on the same snapshot row instead
/// of accumulating one per attempt; the repository's `(tenant, object, revision,
/// hash)` unique index then makes a genuinely different entry set under the same
/// revision a distinct row rather than a silent overwrite.
fn snapshot_uid(object_uid: Uuid, provider_revision: &str) -> Uuid {
    stable_uid(&format!(
        "source-acl-snapshot:{object_uid}:{provider_revision}"
    ))
}
