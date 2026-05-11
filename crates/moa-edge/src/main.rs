//! Public HTTP edge for MOA.
//!
//! The edge terminates incoming credentials, resolves identity, strips any
//! caller-supplied `X-Moa-*` headers, and forwards trusted identity headers to
//! the internal Restate ingress.

mod headers;
mod proxy;
mod routes;

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;

/// Command line arguments for `moa-edge`.
#[derive(Debug, Parser)]
struct Args {
    /// Socket address to bind.
    #[arg(long, env = "MOA_EDGE_BIND", default_value = "0.0.0.0:10000")]
    bind: String,
    /// Internal Restate ingress base URL.
    #[arg(long, env = "MOA_EDGE_UPSTREAM", default_value = "http://restate:8080")]
    upstream: String,
    /// Postgres URL for local API-key authentication.
    #[arg(
        long,
        env = "MOA_EDGE_DATABASE_URL",
        default_value = "postgres://moa_owner:dev@postgres:5432/moa"
    )]
    database_url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,moa_edge=debug")),
        )
        .json()
        .init();

    let args = Args::parse();
    tracing::info!(bind = %args.bind, upstream = %args.upstream, "starting moa-edge");

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(&args.database_url)
        .await
        .context("connect edge api-key database")?;
    let pool = Arc::new(pool);
    let moa_config = moa_core::MoaConfig::load().context("load MOA config")?;
    let providers =
        moa_auth_providers::build_providers(&moa_config, pool).context("build providers bundle")?;

    let state = routes::AppState {
        auth: providers.auth.clone(),
        proxy: Arc::new(
            proxy::OrchestratorProxy::new(&args.upstream).context("build orchestrator proxy")?,
        ),
    };
    let listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("bind {}", args.bind))?;

    axum::serve(listener, routes::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve moa-edge")?;

    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("moa-edge shutdown signal received");
}
