//! Chaos experiment harness (`_docker` lane).
//!
//! Every module runs one hypothesis-driven experiment against the local
//! compose stack and then sweeps the durability invariants. Requirements:
//! the compose stack up (`make dev`), OpenFGA env exported
//! (`MOA_AUTHZ_OPENFGA_STORE_ID`/`MODEL_ID`), and `MOA_RUN_CHAOS_TESTS=1`.
//! Run via `make chaos-smoke` or `make chaos-matrix`.

mod chaos_docker_support;

#[path = "chaos_docker/combo_kill_fga_down_docker.rs"]
mod combo_kill_fga_down_docker;
#[path = "chaos_docker/combo_storm_pg_restart_docker.rs"]
mod combo_storm_pg_restart_docker;
#[path = "chaos_docker/openfga_outage_docker.rs"]
mod openfga_outage_docker;
#[path = "chaos_docker/orchestrator_kill_docker.rs"]
mod orchestrator_kill_docker;
#[path = "chaos_docker/postgres_latency_docker.rs"]
mod postgres_latency_docker;
#[path = "chaos_docker/postgres_partition_docker.rs"]
mod postgres_partition_docker;
#[path = "chaos_docker/postgres_restart_docker.rs"]
mod postgres_restart_docker;
#[path = "chaos_docker/provider_mid_stream_abort_docker.rs"]
mod provider_mid_stream_abort_docker;
#[path = "chaos_docker/provider_storm_docker.rs"]
mod provider_storm_docker;
