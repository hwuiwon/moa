//! Public HTTP edge for MOA.
//!
//! The edge terminates incoming credentials, resolves identity, strips any
//! caller-supplied `X-Moa-*` headers, and forwards trusted identity headers to
//! the internal Restate ingress.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use moa_authz::{FgaClient, FgaConfig};
use moa_config::{AuthzEngine, optional_config_secret};
use moa_edge::connector_credential_proxy::ConnectorCredentialProxy;
use moa_edge::mcp::{self, McpHttpConfig};
use moa_edge::proxy::OrchestratorProxy;
use moa_edge::routes::{AppState, KnowledgeWebhookEdgeConfig};
use moa_messaging::ProviderDeliverySink;
use tokio_util::sync::CancellationToken;

/// Process arguments for `moa-edge`.
#[derive(Debug, Parser)]
struct Args {
    /// Socket address to bind.
    #[arg(long, env = "MOA_EDGE_BIND", default_value = "0.0.0.0:10000")]
    bind: String,
    /// Internal Restate ingress base URL.
    #[arg(long, env = "MOA_EDGE_UPSTREAM")]
    upstream: Option<String>,
    /// Private orchestrator origin for connector credential ingress.
    #[arg(long, env = "MOA_EDGE_CONNECTOR_CREDENTIAL_UPSTREAM")]
    connector_credential_upstream: String,
    /// Exposes connector management and credential routes during staged rollout.
    #[arg(
        long,
        env = "MOA_EDGE_CONNECTOR_MANAGEMENT_ENABLED",
        default_value_t = false
    )]
    connector_management_enabled: bool,
    /// Maximum edge Postgres pool connections.
    #[arg(long, env = "MOA_EDGE_DB_MAX_CONNECTIONS", default_value_t = 50)]
    db_max_connections: u32,
    /// Minimum edge Postgres pool connections kept warm.
    #[arg(long, env = "MOA_EDGE_DB_MIN_CONNECTIONS", default_value_t = 5)]
    db_min_connections: u32,
    /// Timeout, in seconds, to acquire a connection before failing fast.
    #[arg(long, env = "MOA_EDGE_DB_ACQUIRE_TIMEOUT_SECONDS", default_value_t = 3)]
    db_acquire_timeout_seconds: u64,
    /// Maximum lifetime, in seconds, of a pooled connection.
    #[arg(long, env = "MOA_EDGE_DB_MAX_LIFETIME_SECONDS", default_value_t = 1800)]
    db_max_lifetime_seconds: u64,
    /// Comma-delimited exact Host headers accepted by the MCP endpoint.
    #[arg(
        long,
        env = "MOA_EDGE_MCP_ALLOWED_HOSTS",
        default_value = "localhost:10000,127.0.0.1:10000,[::1]:10000"
    )]
    mcp_allowed_hosts: String,
    /// Comma-delimited exact browser origins accepted by the MCP endpoint.
    #[arg(
        long,
        env = "MOA_EDGE_MCP_ALLOWED_ORIGINS",
        default_value = "http://localhost:10000,http://127.0.0.1:10000,http://[::1]:10000"
    )]
    mcp_allowed_origins: String,
    /// Maximum inbound MCP tool calls admitted per authenticated principal each minute.
    #[arg(long, env = "MOA_EDGE_MCP_TOOL_CALLS_PER_MINUTE", default_value_t = 60)]
    mcp_tool_calls_per_minute: u32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let moa_config = moa_config::MoaConfig::load_from_env().context("load MOA config")?;
    let mut telemetry_config = moa_config.clone();
    if telemetry_config.observability.service_name == "moa" {
        telemetry_config.observability.service_name = "moa-edge".to_string();
    }
    let telemetry_guard = moa_observability::init_observability(
        &telemetry_config,
        &moa_observability::TelemetryConfig { json_stdout: true },
    )
    .context("initialize edge observability")?;

    let database_url = moa_config.database.url.clone();
    let upstream = edge_upstream_url(&moa_config, args.upstream);
    let mcp_config = McpHttpConfig::parse(&args.mcp_allowed_hosts, &args.mcp_allowed_origins)
        .and_then(|config| config.with_tool_calls_per_minute(args.mcp_tool_calls_per_minute))
        .context("validate MCP HTTP configuration")?;
    tracing::info!(bind = %args.bind, upstream = %upstream, "starting moa-edge");

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(args.db_max_connections)
        .min_connections(args.db_min_connections)
        .acquire_timeout(Duration::from_secs(args.db_acquire_timeout_seconds))
        .max_lifetime(Duration::from_secs(args.db_max_lifetime_seconds))
        .connect(&database_url)
        .await
        .context("connect edge api-key database")?;
    let pool = Arc::new(pool);
    let auth = moa_auth_providers::build_auth_provider(&moa_config, pool.clone())
        .context("build authentication provider")?;
    let oauth_server = Arc::new(
        moa_auth_providers::OAuthServer::from_config(&moa_config.auth.oauth, pool.clone())
            .await
            .context("bootstrap OAuth authorization server")?,
    );
    // The audit writer is owned by this process for its whole lifetime: started
    // before anything can produce an event, drained explicitly at shutdown.
    // Startup fails if it cannot start, rather than silently turning every audit
    // event for the process lifetime into a counted drop.
    let audit = moa_ocsf::AuditRuntime::start(pool.as_ref().clone())
        .context("start edge security audit writer")?;
    let fga = build_fga_client(&moa_config)
        .context("build edge OpenFGA client")?
        .map(|client| {
            client.with_security_audit(moa_authz::SecurityAudit {
                pool: pool.as_ref().clone(),
                emitter: audit.emitter(),
                // Allow decisions are high volume; the edge audits denials only.
                emit_allows: false,
            })
        });
    let session_store = moa_session::PostgresSessionStore::from_existing_pool_with_config(
        &moa_config,
        pool.as_ref().clone(),
    )
    .await
    .context("build edge session store")?;
    let delivery = Arc::new(
        ProviderDeliverySink::from_env(&moa_config.messaging)
            .context("build edge delivery sink")?,
    );

    let state = AppState {
        connector_management_enabled: args.connector_management_enabled,
        config: Arc::new(moa_config.clone()),
        auth,
        oauth_server,
        oauth_access_tokens: Arc::new(moa_auth_providers::OAuthAccessTokenProvider::new(
            pool.clone(),
        )),
        fga: fga.map(Arc::new),
        knowledge_webhooks: knowledge_webhook_edge_config(&moa_config)
            .context("load knowledge webhook verifier secrets")?,
        pool: pool.clone(),
        session_store: Arc::new(session_store),
        delivery,
        proxy: Arc::new(OrchestratorProxy::new(&upstream).context("build orchestrator proxy")?),
        connector_credentials: Arc::new(
            ConnectorCredentialProxy::new(&args.connector_credential_upstream)
                .context("build private connector credential proxy")?,
        ),
        clickhouse_lineage: moa_config
            .clickhouse
            .as_ref()
            .map(|clickhouse| Arc::new(moa_lineage_sink::ClickHouseStore::connect(clickhouse))),
        audit: audit.emitter(),
        clickhouse_analytics: moa_config.clickhouse.as_ref().map(|clickhouse| {
            Arc::new(
                moa_analytics::AnalyticsClickHouseClient::connect(clickhouse).with_query_budgets(
                    moa_config.analytics.clickhouse_max_execution_time_secs,
                    moa_config.analytics.clickhouse_max_rows_to_read,
                    moa_config.analytics.clickhouse_max_bytes_to_read,
                ),
            )
        }),
    };
    let listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("bind {}", args.bind))?;

    let shutdown = CancellationToken::new();
    tokio::spawn(cancel_on_shutdown_signal(shutdown.clone()));
    axum::serve(
        listener,
        mcp::router(state, mcp_config, shutdown.child_token()),
    )
    .with_graceful_shutdown(shutdown.cancelled_owned())
    .await
    .context("serve moa-edge")?;

    // Shutdown order, and every step of it matters. The server has stopped
    // accepting and has finished its in-flight requests by the time
    // `axum::serve` returns, so nothing can produce a new audit event. Only then
    // is it safe to drain the audit writer, and only after that is it safe to
    // flush telemetry - flushing first would discard the spans and metrics
    // describing the drain itself.
    let dropped = audit.shutdown().await;
    if dropped > 0 {
        tracing::warn!(
            dropped,
            "security audit events were dropped during this process lifetime; the audit \
             trail is incomplete"
        );
    }
    telemetry_guard.shutdown();

    Ok(())
}

fn build_fga_client(config: &moa_config::MoaConfig) -> anyhow::Result<Option<FgaClient>> {
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

/// Cancels on SIGINT or SIGTERM.
///
/// SIGTERM is what Kubernetes actually sends. Listening for SIGINT alone meant
/// every rolling update terminated the edge by its grace-period SIGKILL, so no
/// graceful drain of any kind ran in the one environment where it matters.
async fn cancel_on_shutdown_signal(shutdown: CancellationToken) {
    let interrupt = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = interrupt => tracing::info!("moa-edge received SIGINT"),
                    _ = terminate.recv() => tracing::info!("moa-edge received SIGTERM"),
                }
            }
            Err(error) => {
                tracing::error!(%error, "could not install SIGTERM handler; SIGINT only");
                let _ = interrupt.await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = interrupt.await;
        tracing::info!("moa-edge shutdown signal received");
    }

    shutdown.cancel();
}

fn knowledge_webhook_edge_config(
    config: &moa_config::MoaConfig,
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

fn edge_upstream_url(config: &moa_config::MoaConfig, override_url: Option<String>) -> String {
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
        let mut config = moa_config::MoaConfig::default();
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
