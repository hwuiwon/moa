//! Shared Prometheus-backed runtime metrics helpers for MOA.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};

use crate::config::MetricsConfig;
use crate::error::{MoaError, Result};
use crate::types::{ModelId, ModelTier, SessionStatus, WorkspaceId};

const LATENCY_BUCKETS: &[f64] = &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0];
const CACHE_HIT_RATE_BUCKETS: &[f64] = &[0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0];

static PROMETHEUS_ENDPOINT: OnceLock<SocketAddr> = OnceLock::new();

/// Initializes the global Prometheus exporter when metrics are enabled.
pub fn init_metrics(config: &MetricsConfig) -> Result<()> {
    if !config.enabled || PROMETHEUS_ENDPOINT.get().is_some() {
        return Ok(());
    }

    let addr = parse_metrics_listen_addr(config)?;
    let builder = PrometheusBuilder::new()
        .with_http_listener(addr)
        .set_buckets(LATENCY_BUCKETS)
        .map_err(|error| MoaError::ConfigError(error.to_string()))?
        .set_buckets_for_metric(
            Matcher::Full("moa_cache_hit_rate".to_string()),
            CACHE_HIT_RATE_BUCKETS,
        )
        .map_err(|error| MoaError::ConfigError(error.to_string()))?;

    builder
        .install()
        .map_err(|error| MoaError::ProviderError(error.to_string()))?;
    register_metric_descriptions();
    let _ = PROMETHEUS_ENDPOINT.set(addr);

    Ok(())
}

/// Returns the configured scrape URL when the metrics listen address parses successfully.
#[must_use]
pub fn metrics_endpoint_url(config: &MetricsConfig) -> Option<String> {
    parse_metrics_listen_addr(config)
        .ok()
        .map(format_metrics_endpoint_url)
}

/// Records one created session.
pub fn record_session_created(workspace_id: &WorkspaceId, status: &SessionStatus) {
    counter!(
        "moa_sessions_total",
        "workspace" => workspace_id.to_string(),
        "status" => session_status_label(status).to_string()
    )
    .increment(1);
}

/// Sets the current number of active sessions.
pub fn record_sessions_active(count: u64) {
    gauge!("moa_sessions_active").set(count as f64);
}

/// Records one completed assistant turn.
pub fn record_turn_completed(model: &ModelId, model_tier: ModelTier) {
    counter!(
        "moa_turns_total",
        "model" => model.to_string(),
        "model_tier" => model_tier.as_str().to_string()
    )
    .increment(1);
}

/// Records one outbound LLM API request.
pub fn record_llm_request(provider: &str, model: &str) {
    counter!(
        "moa_llm_requests_total",
        "provider" => provider.to_string(),
        "model" => model.to_string()
    )
    .increment(1);
}

/// Records uncached input tokens, including cache-write prompt tokens.
pub fn record_tokens_input_uncached(provider: &str, model: &str, tokens: u64) {
    if tokens == 0 {
        return;
    }

    counter!(
        "moa_tokens_input_uncached_total",
        "provider" => provider.to_string(),
        "model" => model.to_string()
    )
    .increment(tokens);
}

/// Records cached input tokens served from provider-side prefix caches.
pub fn record_tokens_input_cached(provider: &str, model: &str, tokens: u64) {
    if tokens == 0 {
        return;
    }

    counter!(
        "moa_tokens_input_cached_total",
        "provider" => provider.to_string(),
        "model" => model.to_string()
    )
    .increment(tokens);
}

/// Records output tokens emitted by an LLM response.
pub fn record_tokens_output(provider: &str, model: &str, tokens: u64) {
    if tokens == 0 {
        return;
    }

    counter!(
        "moa_tokens_output_total",
        "provider" => provider.to_string(),
        "model" => model.to_string()
    )
    .increment(tokens);
}

/// Records the ratio of input tokens that were served from cache for one request.
pub fn record_cache_hit_rate(provider: &str, model: &str, ratio: f64) {
    histogram!(
        "moa_cache_hit_rate",
        "provider" => provider.to_string(),
        "model" => model.to_string()
    )
    .record(ratio.clamp(0.0, 1.0));
}

/// Records the time to first token for one LLM request.
pub fn record_llm_ttft(provider: &str, model: &str, duration: Duration) {
    histogram!(
        "moa_llm_ttft_seconds",
        "provider" => provider.to_string(),
        "model" => model.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records the total streaming duration for one LLM request.
pub fn record_llm_streaming_duration(provider: &str, model: &str, duration: Duration) {
    histogram!(
        "moa_llm_streaming_seconds",
        "provider" => provider.to_string(),
        "model" => model.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records one tool call completion and its latency.
pub fn record_tool_call(tool_name: &str, status: &str, duration: Duration) {
    counter!(
        "moa_tool_calls_total",
        "tool_name" => tool_name.to_string(),
        "status" => status.to_string()
    )
    .increment(1);
    histogram!(
        "moa_tool_call_duration_seconds",
        "tool_name" => tool_name.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records one tool-output truncation event.
pub fn record_tool_output_truncated_metric(tool_name: &str) {
    counter!(
        "moa_tool_output_truncated_total",
        "tool_name" => tool_name.to_string()
    )
    .increment(1);
}

/// Records one live broadcast lag event count.
pub fn record_broadcast_lag(channel: &str, dropped: u64) {
    if dropped == 0 {
        return;
    }

    counter!(
        "moa_broadcast_lag_events_dropped_total",
        "channel" => channel.to_string()
    )
    .increment(dropped);
}

/// Records one applied compaction tier.
pub fn record_compaction_tier_applied(tier: u8) {
    counter!(
        "moa_compaction_tier_applied_total",
        "tier" => tier.to_string()
    )
    .increment(1);
}

/// Records one end-to-end turn latency sample.
pub fn record_turn_latency(duration: Duration) {
    histogram!("moa_turn_latency_seconds").record(duration.as_secs_f64());
}

/// Records one pipeline compilation duration sample.
pub fn record_pipeline_compile_duration_metric(duration: Duration) {
    histogram!("moa_pipeline_compile_seconds").record(duration.as_secs_f64());
}

/// Records one sandbox provisioning duration sample.
pub fn record_sandbox_provision_duration(provider: &str, tier: &str, duration: Duration) {
    histogram!(
        "moa_sandbox_provision_seconds",
        "provider" => provider.to_string(),
        "tier" => tier.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Sets the current embedding queue depth gauge.
pub fn record_embedding_queue_depth(depth: u64) {
    gauge!("moa_embedding_queue_depth").set(depth as f64);
}

fn parse_metrics_listen_addr(config: &MetricsConfig) -> Result<SocketAddr> {
    config.listen.parse::<SocketAddr>().map_err(|error| {
        MoaError::ConfigError(format!(
            "invalid metrics.listen `{}`: {error}",
            config.listen
        ))
    })
}

fn format_metrics_endpoint_url(addr: SocketAddr) -> String {
    let host = match addr.ip() {
        IpAddr::V4(ip) if ip == Ipv4Addr::UNSPECIFIED => "localhost".to_string(),
        IpAddr::V6(ip) if ip == Ipv6Addr::UNSPECIFIED => "localhost".to_string(),
        ip => ip.to_string(),
    };
    format!("http://{host}:{}/metrics", addr.port())
}

fn register_metric_descriptions() {
    describe_gauge!("moa_sessions_active", "Currently active MOA sessions.");
    describe_gauge!(
        "moa_embedding_queue_depth",
        "Approximate number of wiki pages waiting for embeddings."
    );
    describe_counter!(
        "moa_sessions_total",
        "Total sessions created, labeled by workspace and initial status."
    );
    describe_counter!(
        "moa_turns_total",
        "Total assistant turns completed, labeled by model and routing tier."
    );
    describe_counter!(
        "moa_llm_requests_total",
        "Total outbound LLM API requests, labeled by provider and model."
    );
    describe_counter!(
        "moa_tokens_input_cached_total",
        "Total cached input tokens served from provider-side caches."
    );
    describe_counter!(
        "moa_tokens_input_uncached_total",
        "Total non-cached input tokens, including cache-write prompt tokens."
    );
    describe_counter!(
        "moa_tokens_output_total",
        "Total output tokens emitted by provider responses."
    );
    describe_counter!(
        "moa_tool_calls_total",
        "Total tool calls completed, labeled by tool name and status."
    );
    describe_counter!(
        "moa_tool_output_truncated_total",
        "Number of successful tool calls whose outputs were truncated."
    );
    describe_counter!(
        "moa_broadcast_lag_events_dropped_total",
        "Live broadcast events dropped because a subscriber lagged behind."
    );
    describe_counter!(
        "moa_compaction_tier_applied_total",
        "Number of times each compaction tier was applied."
    );
    describe_histogram!(
        "moa_turn_latency_seconds",
        "End-to-end turn latency in seconds."
    );
    describe_histogram!(
        "moa_llm_ttft_seconds",
        "Time to first token for LLM requests in seconds."
    );
    describe_histogram!(
        "moa_llm_streaming_seconds",
        "Total LLM request streaming duration in seconds."
    );
    describe_histogram!(
        "moa_tool_call_duration_seconds",
        "Tool execution duration in seconds."
    );
    describe_histogram!(
        "moa_pipeline_compile_seconds",
        "Context pipeline compilation duration in seconds."
    );
    describe_histogram!(
        "moa_sandbox_provision_seconds",
        "Sandbox provisioning duration in seconds."
    );
    describe_histogram!(
        "moa_cache_hit_rate",
        "Ratio of cached input tokens to total input tokens for one request."
    );
}

fn session_status_label(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Created => "created",
        SessionStatus::Running => "running",
        SessionStatus::Paused => "paused",
        SessionStatus::WaitingApproval => "waiting_approval",
        SessionStatus::Completed => "completed",
        SessionStatus::Cancelled => "cancelled",
        SessionStatus::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;
    use tokio::time::{Instant, sleep};

    use super::*;

    #[test]
    fn metrics_endpoint_url_uses_localhost_for_unspecified_listener() {
        let url = metrics_endpoint_url(&MetricsConfig {
            enabled: true,
            listen: "0.0.0.0:9090".to_string(),
        });

        assert_eq!(url.as_deref(), Some("http://localhost:9090/metrics"));
    }

    #[tokio::test]
    async fn prometheus_endpoint_exports_recorded_metric_families() {
        let port = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind ephemeral test port")
            .local_addr()
            .expect("local addr")
            .port();
        let config = MetricsConfig {
            enabled: true,
            listen: format!("127.0.0.1:{port}"),
        };
        init_metrics(&config).expect("metrics exporter should initialize");

        record_llm_request("mock", "gpt-5.4");
        record_tokens_input_uncached("mock", "gpt-5.4", 8);
        record_tokens_output("mock", "gpt-5.4", 4);
        record_cache_hit_rate("mock", "gpt-5.4", 0.5);
        record_turn_latency(Duration::from_millis(25));

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("http client");
        let url = metrics_endpoint_url(&config).expect("metrics url");
        let deadline = Instant::now() + Duration::from_secs(5);
        let scrape = loop {
            match client.get(&url).send().await {
                Ok(response) if response.status().is_success() => {
                    break response.text().await.expect("scrape body");
                }
                Ok(_) | Err(_) if Instant::now() < deadline => {
                    sleep(Duration::from_millis(50)).await;
                }
                Ok(response) => panic!("unexpected scrape status: {}", response.status()),
                Err(error) => panic!("metrics scrape failed: {error}"),
            }
        };

        assert!(scrape.contains("moa_llm_requests_total"));
        assert!(scrape.contains("moa_tokens_input_uncached_total"));
        assert!(scrape.contains("moa_tokens_output_total"));
        assert!(scrape.contains("moa_cache_hit_rate"));
        assert!(scrape.contains("moa_turn_latency_seconds"));
    }
}
