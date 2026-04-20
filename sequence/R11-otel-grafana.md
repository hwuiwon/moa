# R11 — OTel Instrumentation and Grafana Dashboards

## Purpose

Wire OpenTelemetry across `moa-orchestrator`, stand up the Grafana Alloy collector in-cluster, configure Tempo (traces), Mimir (metrics), and Loki (logs), and ship four core dashboards. Establish the turn-as-trace / session-as-link pattern and enforce the Loki structured-metadata convention for tenant/session IDs.

End state: every handler invocation emits a trace; trace appears in Grafana Tempo within seconds; Mimir has the four dashboards; logs are queryable in Loki with structured metadata for per-tenant filtering.

## Prerequisites

- R01–R10 complete. Kubernetes deployment works.
- Access to Grafana Cloud (preferred for Phase 1–2) or a self-hosted Grafana LGTM stack.
- Alloy binary available as a container (grafana/alloy:latest).

## Read before starting

- `docs/12-restate-architecture.md` — "Observability integration" section
- Grafana Alloy docs: https://grafana.com/docs/alloy
- Loki structured metadata docs (v3): https://grafana.com/docs/loki/latest/get-started/labels/structured-metadata
- OpenTelemetry Rust SDK: https://docs.rs/opentelemetry

## Steps

### 1. Add OTel dependencies

`moa-orchestrator/Cargo.toml`:

```toml
[dependencies]
opentelemetry = "0.26"
opentelemetry_sdk = { version = "0.26", features = ["rt-tokio"] }
opentelemetry-otlp = { version = "0.26", features = ["trace", "metrics", "logs", "grpc-tonic"] }
tracing = { workspace = true }
tracing-subscriber = { workspace = true, features = ["env-filter", "json", "fmt"] }
tracing-opentelemetry = "0.27"
```

### 2. Initialize tracing + OTel

`moa-orchestrator/src/telemetry.rs`:

```rust
use opentelemetry::{KeyValue, global};
use opentelemetry_sdk::{trace::TracerProvider, Resource};
use opentelemetry_otlp::WithExportConfig;
use tracing::Subscriber;
use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Registry};

pub fn init() -> anyhow::Result<()> {
    let otlp_endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4317".to_string());

    let resource = Resource::new(vec![
        KeyValue::new("service.name", "moa-orchestrator"),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
        KeyValue::new(
            "deployment.environment",
            std::env::var("DEPLOY_ENV").unwrap_or_else(|_| "dev".to_string()),
        ),
    ]);

    let tracer_provider = opentelemetry_otlp::new_pipeline()
        .tracing()
        .with_exporter(
            opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint(&otlp_endpoint),
        )
        .with_trace_config(
            opentelemetry_sdk::trace::Config::default()
                .with_resource(resource.clone())
                .with_sampler(opentelemetry_sdk::trace::Sampler::ParentBased(Box::new(
                    opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(1.0),
                ))),
        )
        .install_batch(opentelemetry_sdk::runtime::Tokio)?;

    global::set_tracer_provider(tracer_provider.clone());
    let tracer = tracer_provider.tracer("moa-orchestrator");

    let subscriber = Registry::default()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .with(tracing_opentelemetry::layer().with_tracer(tracer));

    tracing::subscriber::set_global_default(subscriber)?;
    Ok(())
}

pub fn shutdown() {
    global::shutdown_tracer_provider();
}
```

Update `main.rs`:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telemetry::init()?;
    // ... rest
    telemetry::shutdown();
    Ok(())
}
```

### 3. Instrument every handler

Use `#[tracing::instrument]` on every handler and add relevant attributes:

```rust
impl Session for SessionImpl {
    #[tracing::instrument(
        skip(ctx, msg),
        fields(
            moa.session_id = %ctx.key(),
            moa.tenant_id = tracing::field::Empty,
            moa.workspace_id = tracing::field::Empty,
            restate.service = "Session",
            restate.handler = "post_message"
        )
    )]
    async fn post_message(ctx: ObjectContext<'_>, msg: UserMessage) -> Result<(), HandlerError> {
        let meta: SessionMeta = ctx.get(K_META).await?.unwrap();
        tracing::Span::current().record("moa.tenant_id", &tracing::field::display(meta.tenant_id));
        tracing::Span::current().record("moa.workspace_id", &tracing::field::display(meta.workspace_id));
        // ... handler body
    }
}
```

Apply to all handlers: `SessionStore::*`, `LLMGateway::*`, `ToolExecutor::*`, `Session::*`, `SubAgent::*`, `Workspace::*`, `Consolidate::run`.

### 4. LLM span attributes (gen_ai semantic conventions)

On `LLMGateway::complete`:

```rust
#[tracing::instrument(
    skip(ctx, req),
    fields(
        gen_ai.system = %req.provider_name(),
        gen_ai.request.model = %req.model,
        gen_ai.request.max_tokens = req.max_tokens,
        gen_ai.response.model = tracing::field::Empty,
        gen_ai.response.finish_reasons = tracing::field::Empty,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
    )
)]
async fn complete(ctx: Context<'_>, req: CompletionRequest) -> Result<CompletionResponse, HandlerError> {
    // ... after response:
    let span = tracing::Span::current();
    span.record("gen_ai.response.model", &tracing::field::display(&response.model));
    span.record("gen_ai.usage.input_tokens", response.input_tokens);
    span.record("gen_ai.usage.output_tokens", response.output_tokens);
    // ...
}
```

### 5. Logs structured metadata for Loki

Do not put high-cardinality IDs in Loki labels (labels are indexed and cardinality explodes). Use structured metadata:

```rust
tracing::info!(
    target = "moa.session",
    // These go into the log line as JSON fields, not labels:
    session_id = %session_id,
    tenant_id = %tenant_id,
    workspace_id = %workspace_id,
    turn_count = turn_count,
    "turn completed"
);
```

Alloy is configured (below) to extract these fields as Loki structured metadata. Labels remain low-cardinality: `service`, `level`, `env`.

### 6. Alloy deployment

`k8s/observability/00-namespace.yaml`:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: observability
```

`k8s/observability/10-alloy-config.yaml`:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: alloy-config
  namespace: observability
data:
  config.alloy: |
    otelcol.receiver.otlp "default" {
      grpc { endpoint = "0.0.0.0:4317" }
      http { endpoint = "0.0.0.0:4318" }
      output {
        traces  = [otelcol.processor.batch.default.input]
        metrics = [otelcol.processor.batch.default.input]
        logs    = [otelcol.processor.batch.default.input]
      }
    }

    otelcol.processor.batch "default" {
      output {
        traces  = [otelcol.exporter.otlphttp.tempo.input]
        metrics = [otelcol.exporter.prometheus.mimir.input]
        logs    = [otelcol.exporter.loki.loki.input]
      }
    }

    otelcol.exporter.otlphttp "tempo" {
      client {
        endpoint = "https://tempo-prod-xx.grafana.net/otlp"
        auth = otelcol.auth.basic.grafana_cloud.handler
      }
    }

    otelcol.exporter.prometheus "mimir" {
      forward_to = [prometheus.remote_write.mimir.receiver]
    }

    prometheus.remote_write "mimir" {
      endpoint {
        url = "https://prometheus-prod-xx.grafana.net/api/prom/push"
        basic_auth {
          username = env("GRAFANA_CLOUD_METRICS_USER")
          password = env("GRAFANA_CLOUD_METRICS_KEY")
        }
      }
    }

    otelcol.exporter.loki "loki" {
      forward_to = [loki.write.loki.receiver]
    }

    loki.write "loki" {
      endpoint {
        url = "https://logs-prod-xx.grafana.net/loki/api/v1/push"
        basic_auth {
          username = env("GRAFANA_CLOUD_LOGS_USER")
          password = env("GRAFANA_CLOUD_LOGS_KEY")
        }
      }
    }

    otelcol.auth.basic "grafana_cloud" {
      username = env("GRAFANA_CLOUD_TRACES_USER")
      password = env("GRAFANA_CLOUD_TRACES_KEY")
    }
```

Credentials via secret `grafana-cloud` with the four user/key pairs. Adjust endpoints for your Grafana Cloud tenant or point at self-hosted Tempo/Mimir/Loki in Phase 3.

`k8s/observability/20-alloy-deployment.yaml`:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: alloy
  namespace: observability
spec:
  replicas: 2
  selector: { matchLabels: { app: alloy } }
  template:
    metadata: { labels: { app: alloy } }
    spec:
      containers:
        - name: alloy
          image: grafana/alloy:latest
          args: ["run", "/etc/alloy/config.alloy"]
          ports:
            - { containerPort: 4317, name: otlp-grpc }
            - { containerPort: 4318, name: otlp-http }
          envFrom:
            - secretRef: { name: grafana-cloud }
          volumeMounts:
            - name: config
              mountPath: /etc/alloy
      volumes:
        - name: config
          configMap: { name: alloy-config }
---
apiVersion: v1
kind: Service
metadata:
  name: alloy
  namespace: observability
spec:
  selector: { app: alloy }
  ports:
    - { name: otlp-grpc, port: 4317, targetPort: 4317 }
    - { name: otlp-http, port: 4318, targetPort: 4318 }
```

### 7. Four core dashboards

Store JSON files in `k8s/observability/dashboards/`. Import via Grafana UI or via the Dashboard provisioning API.

**Dashboard 1: Session Health** (`dashboards/01-session-health.json`)

Panels:
- Active sessions (count) — PromQL: `sum(rate(moa_session_post_message_total[5m])) - sum(rate(moa_session_completed_total[5m]))`
- Turn duration p50/p95/p99 — histogram_quantile over `moa_session_run_turn_duration_seconds_bucket`
- Approval latency p50/p95 — histogram_quantile over `moa_approval_wait_seconds_bucket`
- Errors per tenant tier — `sum by (tier) (rate(moa_session_errors_total[5m]))`
- Sessions by status (Running/WaitingApproval/Idle/Cancelled) — group by `status` label

**Dashboard 2: LLM Gateway** (`dashboards/02-llm-gateway.json`)

Panels:
- Tokens/sec per model — `sum by (gen_ai_request_model) (rate(gen_ai_usage_input_tokens_total[1m]) + rate(gen_ai_usage_output_tokens_total[1m]))`
- 429 rate per provider — `sum by (provider) (rate(moa_llm_429_total[5m]))`
- Cache hit rate — `sum(rate(moa_llm_cache_hits_total[5m])) / sum(rate(moa_llm_calls_total[5m]))`
- $/min rolling — `sum(rate(moa_llm_cost_cents_total[1m])) / 100 * 60`
- Requests queued vs in-flight (if applicable)

**Dashboard 3: Restate Internals** (`dashboards/03-restate-internals.json`)

Panels (metrics from Restate server itself):
- Invocations/sec per handler — `sum by (handler) (rate(restate_invocations_total[1m]))`
- Journal size distribution (p50/p95/p99 entries per invocation) — histogram over `restate_journal_entries_per_invocation`
- Awakeable-waiting count — gauge `restate_awakeables_waiting`
- Retry rate per handler — `sum by (handler) (rate(restate_invocation_retries_total[5m]))`
- Invocations paused (hit max attempts) — `restate_invocations_paused`

**Dashboard 4: Sandbox Fleet** (`dashboards/04-sandbox-fleet.json`)

Panels (from `moa-hands` metrics):
- Provisioned vs active sandboxes — `moa_sandbox_total` / `moa_sandbox_active`
- Provisioning latency p95 — `histogram_quantile(0.95, rate(moa_sandbox_provision_duration_seconds_bucket[5m]))`
- Hand death rate — `rate(moa_hand_deaths_total[5m])`
- Idle reaper kills/min — `rate(moa_sandbox_idle_reaped_total[1m])`

### 8. Turn-as-trace, session-as-link

Each `post_message` invocation is a root span. Add a span link from each turn back to a session-root span:

```rust
// In post_message, before entering turn loop:
let session_link = opentelemetry::trace::Link::new(
    opentelemetry::trace::SpanContext::new(
        trace_id_from_session(session_id),
        opentelemetry::trace::SpanId::INVALID,
        opentelemetry::trace::TraceFlags::default(),
        false,
        opentelemetry::trace::TraceState::default(),
    ),
    vec![],
);
tracing::Span::current().add_link(session_link);
```

`trace_id_from_session` is a deterministic UUID-v5 derivation so the same session always produces the same synthetic root trace_id — letting Tempo group all turns for a session together in the UI.

### 9. Metric exposition

For custom metrics, use the OTel Meter API:

```rust
use opentelemetry::metrics::{Counter, Histogram};

pub struct Metrics {
    pub turn_duration: Histogram<f64>,
    pub approval_wait: Histogram<f64>,
    pub errors_total: Counter<u64>,
    pub llm_cost_cents: Counter<u64>,
    pub llm_cache_hits: Counter<u64>,
}

impl Metrics {
    pub fn init() -> Self {
        let meter = opentelemetry::global::meter("moa-orchestrator");
        Self {
            turn_duration: meter.f64_histogram("moa_session_run_turn_duration_seconds").init(),
            approval_wait: meter.f64_histogram("moa_approval_wait_seconds").init(),
            errors_total: meter.u64_counter("moa_session_errors_total").init(),
            llm_cost_cents: meter.u64_counter("moa_llm_cost_cents_total").init(),
            llm_cache_hits: meter.u64_counter("moa_llm_cache_hits_total").init(),
        }
    }
}
```

Record at relevant call sites. Pass `Metrics` through a `OnceLock` or Arc to handlers.

### 10. Verify end-to-end

`k8s/scripts/observability-smoke.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

# Generate a test session.
kubectl -n moa-system port-forward svc/moa-restate 8080:8080 &
PF=$!
trap "kill $PF" EXIT
sleep 3

SESSION_ID=$(uuidgen)
# ... create + post message

# Wait for traces to propagate (30s typical).
sleep 35

# Query Tempo for the session.
echo "Open Grafana and search Tempo for moa.session_id=$SESSION_ID"
```

## Files to create or modify

- `moa-orchestrator/Cargo.toml` — add OTel deps
- `moa-orchestrator/src/telemetry.rs` — new
- `moa-orchestrator/src/main.rs` — init telemetry
- `moa-orchestrator/src/metrics.rs` — new, custom metrics
- All handler files — add `#[tracing::instrument]` with attributes
- `k8s/observability/00-namespace.yaml` — new
- `k8s/observability/10-alloy-config.yaml` — new
- `k8s/observability/20-alloy-deployment.yaml` — new
- `k8s/observability/dashboards/*.json` — four dashboards
- `k8s/scripts/observability-smoke.sh` — new

## Acceptance criteria

- [ ] Orchestrator logs show OTel spans being exported on startup (no errors).
- [ ] Creating a session via the smoke test: trace appears in Grafana Tempo within 60 seconds.
- [ ] Turn spans nested under post_message root span in Tempo.
- [ ] LLM spans carry `gen_ai.*` attributes.
- [ ] Logs queryable in Loki by `service=moa-orchestrator` and filterable by `session_id` via structured metadata.
- [ ] All four dashboards load and show data after a few minutes of traffic.
- [ ] Restate server metrics appear in Dashboard 3.
- [ ] No label cardinality explosion: `cardinality(logs)` per label group stays <10k.
- [ ] Traces survive pod restart: killing an orchestrator pod mid-turn — the turn's trace shows the retry/replay with `restate.attempt` attribute incremented.

## Notes

- **Grafana Cloud Pro vs self-host**: the architecture decision picks Grafana Cloud through Phase 2, then self-hosts at Phase 3. R11 wires Grafana Cloud. When migrating to self-hosted, the Alloy config changes (endpoints + no basic_auth if using internal mTLS); Rust code doesn't change.
- **Langfuse in parallel**: the architecture doc mentions Langfuse v3 for LLM-specific observability. R11 doesn't wire it; the `LLMGateway::complete` can async-emit to Langfuse in addition to OTel via a follow-up PR. Keep OTel as the trace-of-record.
- **Sampling**: 100% in Phase 1–2 (the `ParentBased + TraceIdRatioBased(1.0)` above). At scale, reduce for background traffic. Keep 100% sampling on error traces via a tail-based sampler at Alloy.
- **Don't use Loki labels for tenant_id / session_id**: each unique value creates a new index stream. At 1k tenants × 100 sessions/day, this is catastrophic. Use structured metadata (indexed at query time, not ingest time).
- **Resource attributes**: `service.name`, `service.version`, `deployment.environment` are the only resource-level attributes. Everything else (tenant, session, user) is a span attribute.
- **gen_ai conventions evolve**: track https://opentelemetry.io/docs/specs/semconv/gen-ai/ for changes. R11 uses the current v1.x convention.

## What R12 expects

- Full observability stack running.
- Dashboards render.
- Ability to diagnose any session end-to-end via Grafana alone.
- This is the readiness gate before turning on live tenant traffic. Do not proceed to R12 without this working.
