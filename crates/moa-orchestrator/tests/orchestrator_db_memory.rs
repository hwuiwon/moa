//! Consolidated Postgres+memory (`_db_memory`) integration-test harness for `moa-orchestrator`.

#[path = "support/artifact_release.rs"]
mod artifact_release;

#[path = "orchestrator_db_memory/agent_definitions_db_memory.rs"]
mod agent_definitions_db_memory;
#[path = "orchestrator_db_memory/memory_retrieval_tools_db_memory.rs"]
mod memory_retrieval_tools_db_memory;
#[path = "orchestrator_db_memory/memory_service_db_memory.rs"]
mod memory_service_db_memory;
#[path = "orchestrator_db_memory/privacy_service_db_memory.rs"]
mod privacy_service_db_memory;
#[path = "support/simulator_policy.rs"]
mod simulator_policy;
#[path = "orchestrator_db_memory/tenant_purge_repository_db_memory.rs"]
mod tenant_purge_repository_db_memory;
