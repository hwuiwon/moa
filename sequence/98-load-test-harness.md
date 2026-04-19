# Step 98 — Custom Load Test Harness

_Build a `moa-loadtest` binary that simulates multi-tenant agent workloads. Standard tools like k6 and Locust don't model LLM agent sessions correctly — they miss TTFT, streaming shape, cache warmth, and multi-turn state. This is the harness for validating 500 QPS readiness._

---

## 1. What this step is about

No off-the-shelf load testing tool models realistic multi-tenant agent workloads. A proper agent load test must simulate: variable-length multi-turn sessions, warm-cache vs cold-cache scenarios, concurrent sandbox provisioning, mixed tool call patterns, and per-session cost tracking alongside latency.

---

## 2. Files to read

- `moa-providers/src/scripted.rs` — the `ScriptedProvider` from step 78. Reuse for infrastructure-only tests (no real LLM calls).
- `moa-orchestrator/src/local.rs` — `LocalOrchestrator::start_session`, `signal`.
- `moa-eval/` — the eval crate may have session-running utilities.

---

## 3. Goal

1. A new `moa-loadtest` binary (or `moa loadtest` subcommand) that:
   - Spawns N concurrent simulated sessions (configurable: 1-1000).
   - Each session sends M user messages with configurable inter-message delay.
   - Supports two modes: `--mock` (ScriptedProvider, tests infrastructure) and `--live` (real LLM, tests full stack).
   - Records per-session: latency per turn, total cost, cache hit rate, tool calls made, errors encountered.
   - Outputs a summary report: p50/p95/p99 turn latency, total cost, error rate, sessions completed, sessions failed.
2. Session profiles:
   - `short`: 5 turns, simple prompts. Models interactive use.
   - `long`: 40 turns, tool-heavy. Models coding sessions.
   - `mixed`: random mix of short and long. Models realistic multi-tenant traffic.
3. The harness can target a local MOA instance or a remote deployment.

---

## 4. Rules

- **Mock mode for CI.** `--mock` must complete in under 30 seconds for 100 concurrent sessions. No network, no real LLM.
- **Live mode for staging.** `--live` calls real LLMs and provisions real sandboxes. Costs money. Default to a small run (5 sessions, 3 turns each) unless `--scale N` overrides.
- **Output both human-readable and JSON.** `--output json` for CI integration.
- **Measure what matters.** Turn latency, TTFT, cache hit rate, cost. NOT requests/second (that's an HTTP metric; agents aren't HTTP requests).

---

## 5. Tasks

### 5a. Create `moa-loadtest` crate

Add to workspace `Cargo.toml`.

### 5b. Session simulator

```rust
async fn simulate_session(
    orchestrator: &LocalOrchestrator,
    profile: SessionProfile,
    results_tx: mpsc::Sender<SessionResult>,
) {
    let session = orchestrator.start_session(...).await?;
    for turn in 0..profile.turns {
        let start = Instant::now();
        orchestrator.signal(session.session_id, Signal::QueueMessage(profile.messages[turn].clone())).await?;
        // wait for turn completion
        let turn_latency = start.elapsed();
        // record metrics
    }
}
```

### 5c. CLI interface

```
moa-loadtest --mode mock --sessions 100 --profile mixed --output json
moa-loadtest --mode live --sessions 5 --profile short --model claude-haiku-4-5
```

### 5d. Report format

```
MOA Load Test Report
====================
Mode: mock | Sessions: 100 | Profile: mixed
Duration: 12.3s

Turn Latency:
  p50: 142ms  p95: 387ms  p99: 812ms

Cache Hit Rate:
  mean: 72.3%  min: 0%  max: 94.1%

Sessions: 100 completed, 0 failed
Total cost: $0.00 (mock mode)
```

---

## 6. Deliverables

- [ ] `moa-loadtest` crate with CLI.
- [ ] Mock mode using `ScriptedProvider`.
- [ ] Live mode with configurable model/provider.
- [ ] Session profiles: short, long, mixed.
- [ ] JSON + human-readable output.
- [ ] Per-session and aggregate metrics.

---

## 7. Acceptance criteria

1. `cargo run -p moa-loadtest -- --mode mock --sessions 100 --profile short` completes in <30s.
2. JSON output is parseable and contains p50/p95/p99 latency, session count, error count.
3. Live mode against Anthropic with 3 sessions produces a cost report.
4. CI can run mock mode as a regression test (exit code 0 if all sessions complete, 1 if failures).
