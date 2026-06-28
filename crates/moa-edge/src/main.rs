//! Public HTTP edge for MOA.
//!
//! The edge terminates incoming credentials, resolves identity, strips any
//! caller-supplied `X-Moa-*` headers, and forwards trusted identity headers to
//! the internal Restate ingress.

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use moa_authz::{FgaClient, FgaConfig};
use moa_core::config::{AuthzEngine, optional_config_secret};
use moa_edge::proxy::OrchestratorProxy;
use moa_edge::routes::{self, AppState, KnowledgeWebhookEdgeConfig};

/// Process arguments for `moa-edge`.
#[derive(Debug, Parser)]
struct Args {
    /// Socket address to bind.
    #[arg(long, env = "MOA_EDGE_BIND", default_value = "0.0.0.0:10000")]
    bind: String,
    /// Internal Restate ingress base URL.
    #[arg(long, env = "MOA_EDGE_UPSTREAM")]
    upstream: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let moa_config = moa_core::MoaConfig::load_from_env().context("load MOA config")?;
    let mut telemetry_config = moa_config.clone();
    if telemetry_config.observability.service_name == "moa" {
        telemetry_config.observability.service_name = "moa-edge".to_string();
    }
    let _telemetry_guard = moa_observability::init_observability(
        &telemetry_config,
        &moa_observability::TelemetryConfig {
            json_stdout: true,
            ..moa_observability::TelemetryConfig::default()
        },
    )
    .context("initialize edge observability")?;

    let database_url = moa_config.database.url.clone();
    let upstream = edge_upstream_url(&moa_config, args.upstream);
    tracing::info!(bind = %args.bind, upstream = %upstream, "starting moa-edge");

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .context("connect edge api-key database")?;
    let pool = Arc::new(pool);
    let providers = moa_auth_providers::build_providers(&moa_config, pool.clone())
        .context("build providers bundle")?;
    let fga = build_fga_client(&moa_config).context("build edge OpenFGA client")?;
    let session_store = moa_session::PostgresSessionStore::from_existing_pool_with_config(
        &moa_config,
        pool.as_ref().clone(),
    )
    .await
    .context("build edge session store")?;

    let state = AppState {
        config: Arc::new(moa_config.clone()),
        auth: providers.auth.clone(),
        fga: fga.map(Arc::new),
        auth0_webhook_secret: moa_config.auth.auth0_webhook_secret.clone(),
        knowledge_webhooks: knowledge_webhook_edge_config(&moa_config)
            .context("load knowledge webhook verifier secrets")?,
        pool: pool.clone(),
        session_store: Arc::new(session_store),
        proxy: Arc::new(OrchestratorProxy::new(&upstream).context("build orchestrator proxy")?),
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

fn build_fga_client(config: &moa_core::MoaConfig) -> anyhow::Result<Option<FgaClient>> {
    if config.authz.engine != AuthzEngine::Openfga {
        return Ok(None);
    }
    let Some(openfga) = config.authz.openfga.as_ref() else {
        return Ok(None);
    };
    FgaClient::new(FgaConfig {
        url: openfga.url.clone(),
        preshared_key: openfga.preshared_key.clone(),
        store_id: openfga.store_id.clone(),
        model_id: openfga.model_id.clone(),
        timeout_ms: openfga.timeout_ms,
    })
    .map(Some)
    .map_err(Into::into)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("moa-edge shutdown signal received");
}

fn knowledge_webhook_edge_config(
    config: &moa_core::MoaConfig,
) -> anyhow::Result<KnowledgeWebhookEdgeConfig> {
    Ok(KnowledgeWebhookEdgeConfig {
        nango_signing_key: optional_config_secret(&config.knowledge.nango.webhook_signing_key),
        merge_signature_key: optional_config_secret(&config.knowledge.merge.webhook_signature_key),
        llamaparse_signing_key: optional_config_secret(
            &config.knowledge.llamaparse.webhook_signing_key,
        ),
        llamaparse_custom_header: custom_header(
            &config.knowledge.llamaparse.webhook_header_name,
            &config.knowledge.llamaparse.webhook_header_value,
        ),
        reducto_signing_key: optional_config_secret(&config.knowledge.reducto.webhook_signing_key),
        reducto_custom_header: custom_header(
            &config.knowledge.reducto.webhook_header_name,
            &config.knowledge.reducto.webhook_header_value,
        ),
    })
}

fn custom_header(name: &Option<String>, value: &Option<String>) -> Option<(String, String)> {
    match (name, value) {
        (Some(name), Some(value)) => Some((name.clone(), value.clone())),
        _ => None,
    }
}

fn edge_upstream_url(config: &moa_core::MoaConfig, override_url: Option<String>) -> String {
    override_url
        .filter(|url| !url.trim().is_empty())
        .or_else(|| config.orchestrator.restate_ingress_url.clone())
        .or_else(|| config.orchestrator.endpoint.clone())
        .unwrap_or_else(|| "http://restate:8080".to_string())
}

#[cfg(test)]
mod tests {
    use super::edge_upstream_url;

    #[test]
    fn edge_upstream_prefers_edge_override_then_shared_restate_config() {
        // Pins: edge uses shared Restate ingress config unless the edge-specific override is set.
        let mut config = moa_core::MoaConfig::default();
        config.orchestrator.restate_ingress_url = Some("http://restate.example:8080".to_string());
        config.orchestrator.endpoint = Some("http://endpoint.example:8080".to_string());

        assert_eq!(
            edge_upstream_url(&config, None),
            "http://restate.example:8080"
        );
        assert_eq!(
            edge_upstream_url(&config, Some("http://edge-upstream.example".to_string())),
            "http://edge-upstream.example"
        );
    }
}
