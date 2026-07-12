//! Consolidated db-memory skill integration tests.

#[path = "support/skill_graph.rs"]
mod skill_graph;

#[path = "skills_db_memory/embedding_backfill_db_memory.rs"]
mod embedding_backfill_db_memory;
#[path = "skills_db_memory/lessons_db_memory.rs"]
mod lessons_db_memory;
#[path = "skills_db_memory/mining_db_memory.rs"]
mod mining_db_memory;
#[path = "skills_db_memory/registry_db_memory.rs"]
mod registry_db_memory;
#[path = "skills_db_memory/render_db_memory.rs"]
mod render_db_memory;
