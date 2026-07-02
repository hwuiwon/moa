//! Consolidated `_db_memory` integration harness for `moa-memory-vector`.

#[path = "memory_vector_db_memory/embedder_switch_db_memory.rs"]
mod embedder_switch_db_memory;
#[path = "memory_vector_db_memory/pgvector_store_db_memory.rs"]
mod pgvector_store_db_memory;
#[path = "memory_vector_db_memory/vector_sync_outbox_db_memory.rs"]
mod vector_sync_outbox_db_memory;
