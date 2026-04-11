# Step 38 — Telemetry Infrastructure Upgrade

_Extend ObservabilityConfig, support OTLP/HTTP + gRPC, add resource attributes, Langfuse auth header support._

---

## 1. What this step is about

MOA already has a basic OTel setup in `moa-core/src/telemetry.rs` — it can export spans via gRPC to a generic OTLP endpoint. This step upgrades that foundation to support the dual-export architecture (Langfuse + Grafana Tempo) by:

- Extending `ObservabilityConfig` with environment, release, custom headers, and OTLP protocol selection
- Adding an OTLP/HTTP exporter option (Langfuse only accepts HTTP, not gRPC)
- Setting OTel resource attributes (`service.name`, `service.version`, `deployment.environment`) so traces carry global context
- Supporting Langfuse Basic Auth headers via config

After this step, `moa` can export traces to an OTel Collector (which fans out to Langfuse + Tempo), or directly to Langfuse's OTLP/HTTP endpoint.

---

## 2. Files/directories to read

Before starting, read these files to understand the current state:

- **`moa-core/src/telemetry.rs`** — Current `init_observability()`, `TelemetryGuard`, `TelemetryConfig`. This is the file you will rewrite.
- **`moa-core/src/config.rs`** — `ObservabilityConfig` struct (line ~614) and its defaults. You'll extend this.
- **`moa-core/Cargo.toml`** — Current OTel crate dependencies.
- **`moa-cli/src/main.rs`** — How `init_observability()` is called at startup.
- **`Cargo.toml` (workspace root)** — Workspace-level OTel dependency versions (`opentelemetry = "0.31"`, etc.).
- **`docs/sample-config.toml`** — Sample config (no observability section yet).

Also reference:
- Langfuse OTel docs: `https://langfuse.com/integrations/native/opentelemetry` — for endpoint URLs, auth format, required headers
- OTel Rust SDK docs: `https://docs.rs/opentelemetry-otlp/latest/` — for `HttpExporter` vs `TonicExporter`

---

## 3. Goal

A working telemetry layer where:
1. `ObservabilityConfig` can express "export to Langfuse via HTTP with auth" or "export to OTel Collector via gRPC" or both
2. Resource attributes like `service.name`, `deployment.environment`, and `service.version` are set globally on all spans
3. The config-driven approach works: change `config.toml` → traces go to a different backend, no code changes
4. Existing functionality (gRPC export, console logging, file logging, debug mode) is preserved

---

## 4. Rules

- **No new crate dependencies** beyond what's already in the workspace. `opentelemetry-otlp` already supports HTTP via the `http-json` or `http-proto` feature — enable the right feature flag.
- **Do not add `opentelemetry-langfuse`** or any Langfuse-specific crate. We use standard OTLP with Langfuse-recognized attributes.
- **Backward compatible.** Existing `config.toml` files with `[observability] enabled = false` must continue to work unchanged.
- **No hardcoded Langfuse URLs.** The endpoint is always configurable.
- **`TelemetryGuard`** must still own the provider lifetime and flush on drop.
- All new config fields must have sensible defaults (disabled/empty).

---

## 5. Tasks

### 5a. Extend `ObservabilityConfig` in `moa-core/src/config.rs`

Add these fields to `ObservabilityConfig`:

```rust
pub struct ObservabilityConfig {
    // Existing
    pub enabled: bool,
    pub service_name: String,
    pub otlp_endpoint: Option<String>,
    
    // New fields
    /// OTLP transport protocol: "grpc" or "http".
    pub otlp_protocol: OtlpProtocol,
    /// Custom headers for OTLP export (e.g., Langfuse auth).
    /// Format: key=value pairs.
    pub otlp_headers: HashMap<String, String>,
    /// Deployment environment (e.g., "production", "staging", "development").
    /// Maps to `deployment.environment` OTel resource attribute and `langfuse.environment`.
    pub environment: Option<String>,
    /// Application release/version tag (e.g., git SHA).
    /// Maps to OTel `service.version` resource attribute and `langfuse.release`.
    pub release: Option<String>,
    /// Sampling ratio 0.0 to 1.0. Default: 1.0 (all traces).
    pub sample_rate: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OtlpProtocol {
    Grpc,
    Http,
}
```

Default: `otlp_protocol = Grpc`, `otlp_headers = empty`, `environment = None`, `release = None`, `sample_rate = 1.0`.

### 5b. Add feature flags to workspace `Cargo.toml`

Enable the `http-proto` feature on `opentelemetry-otlp` so the HTTP exporter is available:

```toml
opentelemetry-otlp = { version = "0.31", features = ["grpc-tonic", "http-proto"] }
```

### 5c. Rewrite `init_observability()` in `moa-core/src/telemetry.rs`

The new function should:

1. Build a `Resource` with: `service.name`, `service.version` (from `release`), `deployment.environment` (from `environment`)
2. Create the appropriate exporter based on `otlp_protocol`:
   - `Grpc` → use `SpanExporter::builder().with_tonic()` (existing path)
   - `Http` → use `SpanExporter::builder().with_http()` with `with_headers()` for auth
3. Apply `otlp_headers` to the exporter (both gRPC and HTTP support custom metadata/headers)
4. Configure sampling via `opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(sample_rate)`
5. Build the `SdkTracerProvider` with the batch exporter, resource, and sampler
6. Layer it into the `tracing_subscriber` registry as before

### 5d. Add helper for Langfuse auth header construction

Add a config convenience method or document in sample config:

```toml
[observability]
enabled = true
otlp_protocol = "http"
otlp_endpoint = "http://localhost:3000/api/public/otel"
environment = "development"
release = "v0.1.0"

[observability.otlp_headers]
Authorization = "Basic cGstbGYteHh4eHg6c2stbGYteHh4eHg="
x-langfuse-ingestion-version = "4"
```

### 5e. Update `docs/sample-config.toml`

Add the full `[observability]` section with comments explaining each field and the Langfuse auth pattern.

---

## 6. How it should be implemented

Start with `config.rs` — add the new fields and defaults. Then update `telemetry.rs` to branch on `OtlpProtocol`. The HTTP exporter path should look roughly like:

```rust
OtlpProtocol::Http => {
    let mut exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(opentelemetry_otlp::Protocol::HttpBinary); // protobuf over HTTP
    
    if let Some(endpoint) = config.observability.otlp_endpoint.as_ref() {
        exporter = exporter.with_endpoint(endpoint);
    }
    
    if !config.observability.otlp_headers.is_empty() {
        let headers: HashMap<String, String> = config.observability.otlp_headers.clone();
        exporter = exporter.with_headers(headers);
    }
    
    exporter.build()?
}
```

For the resource:

```rust
let mut resource_builder = Resource::builder()
    .with_service_name(config.observability.service_name.clone());

if let Some(env) = &config.observability.environment {
    resource_builder = resource_builder.with_attribute(
        opentelemetry::KeyValue::new("deployment.environment", env.clone())
    );
}
if let Some(release) = &config.observability.release {
    resource_builder = resource_builder.with_attribute(
        opentelemetry::KeyValue::new("service.version", release.clone())
    );
}
```

For sampling:

```rust
use opentelemetry_sdk::trace::Sampler;

let sampler = if config.observability.sample_rate < 1.0 {
    Sampler::TraceIdRatioBased(config.observability.sample_rate)
} else {
    Sampler::AlwaysOn
};

let provider = SdkTracerProvider::builder()
    .with_batch_exporter(exporter)
    .with_resource(resource)
    .with_sampler(sampler)
    .build();
```

---

## 7. Deliverables

- [ ] `moa-core/src/config.rs` — Extended `ObservabilityConfig` with `otlp_protocol`, `otlp_headers`, `environment`, `release`, `sample_rate`, and `OtlpProtocol` enum
- [ ] `moa-core/src/telemetry.rs` — Rewritten `init_observability()` supporting both gRPC and HTTP export, resource attributes, sampling, and custom headers
- [ ] `moa-core/Cargo.toml` — Updated `opentelemetry-otlp` feature flags to include `http-proto`
- [ ] `Cargo.toml` (workspace root) — Updated `opentelemetry-otlp` features
- [ ] `docs/sample-config.toml` — Full `[observability]` section with Langfuse example

---

## 8. Acceptance criteria

1. **gRPC path still works.** `[observability] enabled = true, otlp_protocol = "grpc", otlp_endpoint = "http://localhost:4317"` → spans arrive at an OTel Collector's gRPC receiver.
2. **HTTP path works.** `otlp_protocol = "http"` with Langfuse endpoint + auth headers → spans arrive at Langfuse.
3. **Resource attributes present.** Every exported span's resource includes `service.name`, `service.version` (if release set), and `deployment.environment` (if environment set).
4. **Custom headers sent.** When `otlp_headers` is non-empty, headers are included in export requests.
5. **Sampling works.** `sample_rate = 0.5` → roughly half of traces are exported.
6. **Backward compatible.** Config with only `enabled = false` still works. Config with only `enabled = true` + `otlp_endpoint` (no protocol field) defaults to gRPC.
7. **Clean shutdown.** `TelemetryGuard` drop flushes pending spans.
8. **All existing tests pass.** No regressions.

---

## 9. Testing

### Unit tests (in `moa-core/src/telemetry.rs` or `moa-core/tests/`)

**Test 1: Config deserialization — gRPC default**
```rust
#[test]
fn observability_config_defaults_to_grpc() {
    let toml = r#"
        [observability]
        enabled = true
        service_name = "moa"
    "#;
    let config: MoaConfig = parse_toml(toml);
    assert_eq!(config.observability.otlp_protocol, OtlpProtocol::Grpc);
    assert_eq!(config.observability.sample_rate, 1.0);
    assert!(config.observability.otlp_headers.is_empty());
}
```

**Test 2: Config deserialization — HTTP with headers**
```rust
#[test]
fn observability_config_http_with_langfuse_headers() {
    let toml = r#"
        [observability]
        enabled = true
        otlp_protocol = "http"
        otlp_endpoint = "http://langfuse:3000/api/public/otel"
        environment = "staging"
        release = "abc123"
        sample_rate = 0.5
        
        [observability.otlp_headers]
        Authorization = "Basic cGstbGYteHh4eHg6c2stbGYteHh4eHg="
        x-langfuse-ingestion-version = "4"
    "#;
    let config: MoaConfig = parse_toml(toml);
    assert_eq!(config.observability.otlp_protocol, OtlpProtocol::Http);
    assert_eq!(config.observability.environment.as_deref(), Some("staging"));
    assert_eq!(config.observability.otlp_headers.len(), 2);
}
```

**Test 3: Backward compatibility — minimal config**
```rust
#[test]
fn observability_config_backward_compat() {
    let toml = r#"
        [observability]
        enabled = false
    "#;
    let config: MoaConfig = parse_toml(toml);
    assert!(!config.observability.enabled);
    assert_eq!(config.observability.otlp_protocol, OtlpProtocol::Grpc);
}
```

**Test 4: Resource attributes construction**
```rust
#[test]
fn resource_includes_environment_and_release() {
    let config = ObservabilityConfig {
        environment: Some("production".into()),
        release: Some("v1.2.3".into()),
        service_name: "moa".into(),
        ..Default::default()
    };
    let resource = build_resource(&config);
    // Assert resource contains deployment.environment = "production"
    // Assert resource contains service.version = "v1.2.3"
    // Assert resource contains service.name = "moa"
}
```

**Test 5: init_observability returns guard when disabled**
```rust
#[test]
fn init_observability_disabled_returns_guard() {
    let config = MoaConfig::default(); // enabled = false
    let telemetry = TelemetryConfig::default();
    let guard = init_observability(&config, &telemetry).unwrap();
    assert!(guard.provider.is_none());
}
```

### Integration test (manual / CI with collector)

**Test 6: End-to-end gRPC export**
1. Start an OTel Collector with a logging exporter: `docker run -p 4317:4317 otel/opentelemetry-collector:latest`
2. Set config: `enabled = true, otlp_protocol = "grpc", otlp_endpoint = "http://localhost:4317"`
3. Run `moa exec "hello"` — verify spans appear in collector logs

**Test 7: End-to-end HTTP export to Langfuse**
1. Start self-hosted Langfuse: `docker compose up -d` (from Langfuse repo)
2. Create a project, get public/secret keys
3. Set config with HTTP protocol and Langfuse auth headers
4. Run `moa exec "hello"` — verify trace appears in Langfuse UI

---

## 10. Additional notes

- The `opentelemetry-otlp` HTTP exporter defaults to `http-proto` (protobuf encoding). Langfuse supports both `http-proto` and `http-json`. Prefer proto for smaller payloads.
- The `x-langfuse-ingestion-version: 4` header enables real-time trace preview in Langfuse. Always include it.
- When using an OTel Collector as intermediary, MOA can use gRPC to the Collector, and the Collector uses HTTP to Langfuse. In this case, the Langfuse auth headers go in the Collector config, not in MOA's config. MOA's config stays simple: `otlp_protocol = "grpc", otlp_endpoint = "http://collector:4317"`.
- `Resource` attributes are set once at process startup and attached to every span. They're the right place for static dimensions like environment and release.
