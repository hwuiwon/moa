# R01 — Scaffold Restate

## Purpose

Shape the workspace around `restate-sdk` and convert `moa-orchestrator` into a
binary crate with an HTTP server that registers a trivial `Health` Service.
End state: the binary compiles, runs locally, registers with a local
`restate-server`, and responds to an RPC health check.

This prompt ships no production behavior. Its job is to establish the runtime
shape before any real handlers are written.

## Prerequisites

- R00 read and understood.
- Local Rust toolchain 1.80+.
- `restate-server` v1.5+ installed locally (download from restate.dev or `cargo install restate-server`).

## Read before starting

- `docs/12-restate-architecture.md` sections "Core Restate concepts" and "Crate impact"
- `Cargo.toml` (workspace root) — current dependency shape
- `moa-orchestrator/Cargo.toml` and `moa-orchestrator/src/lib.rs`

## Steps

### 1. Normalize workspace dependencies

In workspace root `Cargo.toml` and in each member crate:

- Remove unused orchestration SDK crates and stale transitive pins.
- Remove feature entries that are no longer used.
- Keep `tokio`, `serde`, `tracing`, `tracing-subscriber`, `opentelemetry*`, `uuid`, `chrono`, `thiserror` — all still needed.

### 2. Add Restate dependencies

Add to workspace root `Cargo.toml` `[workspace.dependencies]`:

```toml
restate-sdk = "0.8"
restate-sdk-shared-core = "0.8"  # if needed for type reuse
```

Add to `moa-orchestrator/Cargo.toml`:

```toml
[dependencies]
restate-sdk = { workspace = true }
tokio = { workspace = true, features = ["full"] }
tracing = { workspace = true }
tracing-subscriber = { workspace = true, features = ["env-filter", "json"] }
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
anyhow = "1"
clap = { workspace = true, features = ["derive"] }
# moa crates used later:
moa-core = { path = "../moa-core" }
```

### 3. Convert `moa-orchestrator` from lib to bin

Replace `moa-orchestrator/src/lib.rs` usage with `moa-orchestrator/src/main.rs` as the binary entry. Keep `lib.rs` if library exports are needed by tests, but the default target becomes the binary.

`moa-orchestrator/Cargo.toml`:

```toml
[[bin]]
name = "moa-orchestrator"
path = "src/main.rs"
```

### 4. Delete unused orchestration code

Delete unused workflow-era files if they still exist in your tree:

- `moa-orchestrator/src/workflows/*`
- `moa-orchestrator/src/activities/*`
- Any unused workflow client or activity-context abstractions.

Do not preserve dead code "just in case." Clean sweeps only.

### 5. Scaffold the binary

`moa-orchestrator/src/main.rs`:

```rust
use clap::Parser;
use restate_sdk::prelude::*;

mod config;
mod services;

#[derive(Parser, Debug)]
struct Args {
    /// HTTP port for Restate handler endpoint
    #[arg(long, default_value_t = 9080)]
    port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();

    let args = Args::parse();

    tracing::info!(port = args.port, "starting moa-orchestrator");

    HttpServer::new(
        Endpoint::builder()
            .bind(services::health::HealthImpl.serve())
            .build(),
    )
    .listen_and_serve(format!("0.0.0.0:{}", args.port).parse()?)
    .await
}
```

### 6. Scaffold the Health service

`moa-orchestrator/src/services/mod.rs`:

```rust
pub mod health;
```

`moa-orchestrator/src/services/health.rs`:

```rust
use restate_sdk::prelude::*;

#[restate_sdk::service]
pub trait Health {
    async fn ping(ctx: Context<'_>) -> Result<String, HandlerError>;
    async fn version(ctx: Context<'_>) -> Result<VersionInfo, HandlerError>;
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct VersionInfo {
    pub crate_version: String,
    pub restate_sdk_version: String,
    pub git_sha: Option<String>,
}

pub struct HealthImpl;

impl Health for HealthImpl {
    async fn ping(_ctx: Context<'_>) -> Result<String, HandlerError> {
        Ok("pong".to_string())
    }

    async fn version(_ctx: Context<'_>) -> Result<VersionInfo, HandlerError> {
        Ok(VersionInfo {
            crate_version: env!("CARGO_PKG_VERSION").to_string(),
            restate_sdk_version: "0.8".to_string(),
            git_sha: option_env!("GIT_SHA").map(String::from),
        })
    }
}
```

### 7. Scaffold the config module

`moa-orchestrator/src/config.rs`:

```rust
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct OrchestratorConfig {
    pub restate_admin_url: String,
    pub postgres_url: String,
    pub llm_gateway_url: Option<String>,
}

impl OrchestratorConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            restate_admin_url: std::env::var("RESTATE_ADMIN_URL")
                .unwrap_or_else(|_| "http://localhost:9070".to_string()),
            postgres_url: std::env::var("POSTGRES_URL")
                .map_err(|_| anyhow::anyhow!("POSTGRES_URL required"))?,
            llm_gateway_url: std::env::var("LLM_GATEWAY_URL").ok(),
        })
    }
}
```

Config is loaded but not used in R01 — fields are placeholders for later prompts.

### 8. Local smoke test script

`moa-orchestrator/scripts/local-smoke.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

echo "Starting restate-server in background..."
restate-server --node-name local --data-dir .restate-dev &
RESTATE_PID=$!
trap "kill $RESTATE_PID" EXIT
sleep 2

echo "Starting moa-orchestrator..."
RUST_LOG=info cargo run -p moa-orchestrator -- --port 9080 &
ORCH_PID=$!
trap "kill $RESTATE_PID $ORCH_PID" EXIT
sleep 3

echo "Registering deployment..."
restate deployments register http://localhost:9080 --yes

echo "Calling Health/ping..."
restate invocation call Health/ping
```

## Files to create or modify

- `Cargo.toml` (workspace root) — add Restate deps and normalize shared deps
- `moa-orchestrator/Cargo.toml` — convert to bin, update deps
- `moa-orchestrator/src/main.rs` — new binary entry
- `moa-orchestrator/src/config.rs` — new
- `moa-orchestrator/src/services/mod.rs` — new
- `moa-orchestrator/src/services/health.rs` — new
- `moa-orchestrator/scripts/local-smoke.sh` — new
- Delete: any unused workflow-era files in `moa-orchestrator/src/`

## Acceptance criteria

- [ ] `cargo build -p moa-orchestrator` succeeds with zero warnings related to unused orchestration code.
- [ ] Repo-wide dependency grep shows no unused orchestration SDK references.
- [ ] `cargo test -p moa-orchestrator` passes.
- [ ] `./moa-orchestrator/scripts/local-smoke.sh` completes successfully, with `restate invocation call Health/ping` returning `"pong"`.
- [ ] `restate invocation call Health/version` returns a VersionInfo with non-empty `crate_version`.
- [ ] The binary logs at least one structured JSON line on startup via `tracing`.

## Notes

- If `restate-server` is not available, install via `curl -sL https://restate.dev/install.sh | bash`, or use `docker run -p 9070:9070 -p 8080:8080 restatedev/restate:1.5`.
- The HTTP endpoint port (`9080` here) is what Restate polls for handler discovery. It is *not* the Restate admin API port (`9070`).
- Do not add service registration logic yet — `restate deployments register` handles that for R01. Later prompts will automate it.
- Resist adding more services in this prompt. The scope is intentionally minimal. R02 adds the first real handler.

## What R02 expects

- `moa-orchestrator` binary compiles and runs.
- Health service registered and callable.
- Config struct present and env-loading works.
- Service scaffolding pattern established (`services/` module, one file per service).
