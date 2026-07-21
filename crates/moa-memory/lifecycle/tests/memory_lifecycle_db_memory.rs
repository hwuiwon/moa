//! Consolidated `_db_memory` integration harness for `moa-memory-lifecycle`.

use std::sync::{Arc, OnceLock};

use moa_crypto::{KeyManagementProvider, LocalKmsProvider};

fn test_kms() -> Arc<dyn KeyManagementProvider> {
    static KMS: OnceLock<Arc<dyn KeyManagementProvider>> = OnceLock::new();
    KMS.get_or_init(|| Arc::new(LocalKmsProvider::new()))
        .clone()
}

#[path = "memory_lifecycle_db_memory/consolidation_contact_scope_db_memory.rs"]
mod consolidation_contact_scope_db_memory;
#[path = "memory_lifecycle_db_memory/consolidation_pass_db_memory.rs"]
mod consolidation_pass_db_memory;
#[path = "memory_lifecycle_db_memory/digest_postgres_db_memory.rs"]
mod digest_postgres_db_memory;
#[path = "memory_lifecycle_db_memory/lesson_curation_db_memory.rs"]
mod lesson_curation_db_memory;
#[path = "memory_lifecycle_db_memory/quality_postgres_db_memory.rs"]
mod quality_postgres_db_memory;
