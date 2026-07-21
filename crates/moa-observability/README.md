# moa-observability

Runtime metrics, tracing bootstrap, and Restate observability helpers shared
by MOA binaries. Exposes Prometheus-backed metric recorders, OpenTelemetry
tracing setup, and W3C trace-context propagation across service boundaries.

## Structure

- `propagation` — W3C trace-context propagation across MOA service
  boundaries (`traceparent`/`tracestate` headers).
- `restate_observability` — Restate-side observability helpers shared by
  orchestrator handlers.
- `runtime_metrics` — shared Prometheus-backed runtime metric names,
  histogram buckets, and `record_*` helpers.
- `telemetry` — tracing and OpenTelemetry bootstrap for MOA binaries
  (`init_observability`, `TelemetryGuard`).
- `trace_context` — trace-context span attribute helpers.
- `turn_latency` — per-turn latency instrumentation utilities shared across
  orchestration layers.
