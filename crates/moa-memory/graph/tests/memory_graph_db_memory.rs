//! Consolidated `_db_memory` integration harness for `moa-memory-graph`.

#[path = "memory_graph_db_memory/changelog_outbox_db_memory.rs"]
mod changelog_outbox_db_memory;
#[path = "memory_graph_db_memory/concurrent_writers_db_memory.rs"]
mod concurrent_writers_db_memory;
#[path = "memory_graph_db_memory/contact_write_db_memory.rs"]
mod contact_write_db_memory;
#[path = "memory_graph_db_memory/knowledge_labels_db_memory.rs"]
mod knowledge_labels_db_memory;
#[path = "memory_graph_db_memory/lexical_ranking_db_memory.rs"]
mod lexical_ranking_db_memory;
#[path = "memory_graph_db_memory/node_index_db_memory.rs"]
mod node_index_db_memory;
#[path = "memory_graph_db_memory/read_smoke_db_memory.rs"]
mod read_smoke_db_memory;
#[path = "memory_graph_db_memory/write_protocol_db_memory.rs"]
mod write_protocol_db_memory;
