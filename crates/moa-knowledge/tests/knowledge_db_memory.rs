//! Consolidated database-backed integration tests for the knowledge crate.

#[path = "knowledge_db_memory/contact_groups_db_memory.rs"]
mod contact_groups_db_memory;
#[path = "knowledge_db_memory/ingestion_pipeline_db_memory/mod.rs"]
mod ingestion_pipeline_db_memory;
#[path = "knowledge_db_memory/observability_db_memory.rs"]
mod observability_db_memory;
#[path = "knowledge_db_memory/repository_db_memory.rs"]
mod repository_db_memory;
#[path = "knowledge_db_memory/sync_run_db_memory.rs"]
mod sync_run_db_memory;
