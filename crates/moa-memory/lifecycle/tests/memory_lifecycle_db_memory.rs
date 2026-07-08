//! Consolidated `_db_memory` integration harness for `moa-memory-lifecycle`.

#[path = "memory_lifecycle_db_memory/consolidation_contact_scope_db_memory.rs"]
mod consolidation_contact_scope_db_memory;
#[path = "memory_lifecycle_db_memory/consolidation_pass_db_memory.rs"]
mod consolidation_pass_db_memory;
#[path = "memory_lifecycle_db_memory/digest_postgres_db_memory.rs"]
mod digest_postgres_db_memory;
#[path = "memory_lifecycle_db_memory/entity_resolution_db_memory.rs"]
mod entity_resolution_db_memory;
#[path = "memory_lifecycle_db_memory/quality_postgres_db_memory.rs"]
mod quality_postgres_db_memory;
