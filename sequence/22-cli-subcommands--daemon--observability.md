# Step 22: CLI Subcommands + Daemon + Observability

## What this step is about
All CLI subcommands, the daemon mode for persistent background operation, and OpenTelemetry instrumentation.

## Files to read
- `docs/03-communication-layer.md` — CLI subcommands, daemon mode
- `docs/10-technology-stack.md` — OTel crates

## Tasks
1. **CLI subcommands**: `status`, `sessions`, `sessions --workspace .`, `attach <id>`, `resume`, `resume <id>`, `memory search "query"`, `memory show <path>`, `config`, `config set <key> <val>`, `init`, `doctor`
2. **`moa daemon start/stop/status/logs`**: Background process via Unix socket. TUI connects to daemon. Sessions persist when TUI exits.
3. **OTel instrumentation**: `tracing-opentelemetry` + `opentelemetry-otlp`. Traces for: pipeline stages, LLM calls, tool execution, session lifecycle. Exportable to Grafana/Jaeger.
4. **`moa doctor`**: Check API keys, Docker availability, disk space, session DB health, memory index health.

## Deliverables
Updated `moa-cli/src/main.rs` with all subcommands, `moa-cli/src/daemon.rs`, OTel setup in `moa-core` or a new `moa-telemetry` module.

## Acceptance criteria
1. All subcommands work as documented
2. `moa daemon start` → sessions keep running after TUI exit
3. `moa attach <id>` connects TUI to a running daemon session
4. `moa doctor` reports system health
5. OTel traces visible in Jaeger/Grafana when configured

---

