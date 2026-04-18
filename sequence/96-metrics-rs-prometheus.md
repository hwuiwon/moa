# Step 96 — Structured Metrics via metrics-rs + Prometheus

_Replace ad-hoc `tracing::info!` counter lines with the `metrics` crate and a Prometheus exporter. Every metric from steps 79-82, 92, and 94 becomes a first-class Prometheus metric, scrapeable by any monitoring stack._

---

## 1. What this step is about

Steps 79-82 added per-turn log lines for cache hit rate, event replay counts, latency decomposition, and broadcast lag. Step 92 moved session-level aggregates into Postgres views. But none of these are exposed as Prometheus metrics — the standard interface for operational monitoring.

The `metrics` crate is Rust's standard metrics facade (like `tracing` is for logs). `metrics-exporter-prometheus` provides a Prometheus scrape endpoint.

---

## 2. Files to read

- `moa-runtime/src/*.rs` — startup, HTTP server wiring.
- `moa-orchestrator/src/local.rs` — turn-loop metrics emission points (steps 79-82).
- `moa-providers/src/instrumentation.rs` — existing OTel spans.
- `moa-core/src/types/provider.rs` — `TokenUsage`.

---

## 3. Goal

1. Add `metrics` + `metrics-exporter-prometheus` as workspace dependencies.
2. Start a Prometheus scrape endpoint at `/metrics` on a configurable port (default 9090).
3. Register and emit the following metrics:

**Gauges:**
- `moa_sessions_active` — currently running sessions
- `moa_embedding_queue_depth` — wiki pages awaiting embedding (step 91)

**Counters:**
- `moa_sessions_total` — sessions created (by workspace, status)
- `moa_turns_total` — turns completed (by model, model_tier)
- `moa_llm_requests_total` — LLM API calls (by provider, model)
- `moa_tokens_input_cached_total`, `moa_tokens_input_uncached_total`, `moa_tokens_output_total`
- `moa_tool_calls_total` — by tool_name, status (success/error)
- `moa_tool_output_truncated_total` — from step 94
- `moa_broadcast_lag_events_dropped_total` — from step 82
- `moa_compaction_tier_applied_total` — by tier (1/2/3) from step 88

**Histograms:**
- `moa_turn_latency_seconds` — end-to-end turn duration
- `moa_llm_ttft_seconds` — time to first token
- `moa_llm_streaming_seconds` — total streaming duration
- `moa_tool_call_duration_seconds` — by tool_name
- `moa_pipeline_compile_seconds` — context compilation
- `moa_sandbox_provision_seconds` — hand provisioning latency
- `moa_cache_hit_rate` — per-turn cache hit rate (histogram of ratios)

4. All metrics use labels for dimensionality (workspace, model, tool_name, etc.) but avoid unbounded cardinality (no session_id labels on histograms).
5. `moa doctor` reports: "Metrics endpoint: http://localhost:9090/metrics — OK".

---

## 4. Rules

- **Use `metrics` crate, not `prometheus` crate directly.** `metrics` is the facade; exporters are swappable.
- **Register all metrics at startup.** Use `metrics::describe_*` for each metric with a help string.
- **No session_id on histograms/counters.** Prometheus cardinality explosion. Use workspace_id if needed.
- **Histogram buckets for latency: 10ms, 50ms, 100ms, 250ms, 500ms, 1s, 2.5s, 5s, 10s, 30s.**
- **Config:**
  ```toml
  [metrics]
  enabled = true
  listen = "0.0.0.0:9090"
  ```

---

## 5. Tasks

### 5a. Add dependencies

```toml
metrics = "0.24"
metrics-exporter-prometheus = { version = "0.16", features = ["http-listener"] }
```

### 5b. Initialize at startup

In `moa-runtime` initialization:
```rust
let builder = PrometheusBuilder::new()
    .with_http_listener(config.metrics.listen)
    .install()?;
```

### 5c. Emit at existing instrumentation points

Replace `tracing::info!(cache_hit_rate = ...)` with:
```rust
metrics::histogram!("moa_cache_hit_rate").record(usage.cache_hit_rate());
metrics::counter!("moa_tokens_input_cached_total").increment(usage.input_tokens_cache_read as u64);
```

### 5d. Metric descriptions

```rust
metrics::describe_histogram!("moa_turn_latency_seconds", "End-to-end turn latency");
metrics::describe_counter!("moa_llm_requests_total", "Total LLM API requests");
// ...
```

### 5e. Tests

- Start the metrics endpoint, make an HTTP request to `/metrics`, parse Prometheus text format, assert expected metric names appear.
- Run a 3-turn session, scrape metrics, assert `moa_turns_total` >= 3.

---

## 6. Deliverables

- [ ] `metrics` + `metrics-exporter-prometheus` in workspace deps.
- [ ] Prometheus scrape endpoint at `/metrics`.
- [ ] All metrics listed in section 3 registered and emitted.
- [ ] Existing `tracing::info!` metric lines augmented (not replaced) with `metrics::*` calls.
- [ ] `moa doctor` reports metrics endpoint health.
- [ ] Config section `[metrics]`.

---

## 7. Acceptance criteria

1. `curl http://localhost:9090/metrics` returns valid Prometheus text format with all registered metrics.
2. After a 5-turn session, `moa_turns_total` shows 5, `moa_cache_hit_rate` histogram has 5 observations.
3. Grafana (or any Prometheus-compatible tool) can scrape and visualize the metrics.
4. `cargo test --workspace` green.
