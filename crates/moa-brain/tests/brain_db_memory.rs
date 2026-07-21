//! Consolidated `_db_memory`-lane integration harness for moa-brain.

use std::sync::{Arc, OnceLock};

use moa_crypto::{KeyManagementProvider, LocalKmsProvider};

fn test_kms() -> Arc<dyn KeyManagementProvider> {
    static KMS: OnceLock<Arc<dyn KeyManagementProvider>> = OnceLock::new();
    KMS.get_or_init(|| Arc::new(LocalKmsProvider::new()))
        .clone()
}

mod support;

#[path = "brain_db_memory/artifact_skill_injection_db_memory.rs"]
mod artifact_skill_injection_db_memory;
#[path = "brain_db_memory/hybrid_retrieval_db_memory.rs"]
mod hybrid_retrieval_db_memory;
#[path = "brain_db_memory/pipeline_stages_db_memory.rs"]
mod pipeline_stages_db_memory;
#[path = "brain_db_memory/retrieval_lineage_db_memory.rs"]
mod retrieval_lineage_db_memory;
#[path = "brain_db_memory/tenant_contact_knowledge_retrieval_db_memory.rs"]
mod tenant_contact_knowledge_retrieval_db_memory;
