//! Consolidated `_db_memory` integration harness for `moa-memory-graph`.

use std::sync::{Arc, OnceLock};

use moa_crypto::{KeyManagementProvider, LocalKmsProvider};

fn test_kms() -> Arc<dyn KeyManagementProvider> {
    static KMS: OnceLock<Arc<dyn KeyManagementProvider>> = OnceLock::new();
    KMS.get_or_init(|| Arc::new(LocalKmsProvider::new()))
        .clone()
}

#[path = "memory_graph_db_memory/backfill_db_memory.rs"]
mod backfill_db_memory;
#[path = "memory_graph_db_memory/barrier_need_to_know_db_memory.rs"]
mod barrier_need_to_know_db_memory;
#[path = "memory_graph_db_memory/changelog_outbox_db_memory.rs"]
mod changelog_outbox_db_memory;
#[path = "memory_graph_db_memory/concurrent_writers_db_memory.rs"]
mod concurrent_writers_db_memory;
#[path = "memory_graph_db_memory/contact_write_db_memory.rs"]
mod contact_write_db_memory;
#[path = "memory_graph_db_memory/edge_validity_db_memory.rs"]
mod edge_validity_db_memory;
#[path = "memory_graph_db_memory/encryption_db_memory.rs"]
mod encryption_db_memory;
#[path = "memory_graph_db_memory/knowledge_labels_db_memory.rs"]
mod knowledge_labels_db_memory;
#[path = "memory_graph_db_memory/lexical_ranking_db_memory.rs"]
mod lexical_ranking_db_memory;
#[path = "memory_graph_db_memory/node_index_db_memory.rs"]
mod node_index_db_memory;
#[path = "memory_graph_db_memory/read_smoke_db_memory.rs"]
mod read_smoke_db_memory;
#[path = "memory_graph_db_memory/scored_walk_db_memory.rs"]
mod scored_walk_db_memory;
#[path = "memory_graph_db_memory/write_protocol_db_memory.rs"]
mod write_protocol_db_memory;
