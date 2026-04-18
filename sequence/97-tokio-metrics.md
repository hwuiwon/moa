# Step 97 — tokio-metrics Runtime Monitoring

_Add `tokio-metrics` to track runtime health: worker thread utilization, task scheduling delays, and poll durations. Export via the step 96 Prometheus endpoint. Detect when the tokio runtime is overloaded before sessions degrade._

---

## 1. What this step is about

At 500 QPS, the tokio runtime manages thousands of concurrent futures (session tasks, LLM streams, tool calls, LISTEN subscribers, embedding workers). If the runtime becomes saturated — workers constantly polling, no idle time — latency spikes but without tokio-level metrics, the cause is invisible.

`tokio-metrics` exposes `RuntimeMonitor` (global runtime stats) and `TaskMonitor` (per-task-class stats). Combined with step 96's Prometheus endpoint, this gives clear answers to "is the runtime the bottleneck?"

---

## 2. Files to read

- `moa-runtime/src/*.rs` — runtime initialization.
- `moa-orchestrator/src/local.rs` — where session tasks are spawned.
- `tokio-metrics` docs (https://docs.rs/tokio-metrics).
- `opentelemetry-instrumentation-tokio` crate — bridges tokio-metrics to OTel.

---

## 3. Goal

1. Build with `RUSTFLAGS="--cfg tokio_unstable"` to enable runtime metrics.
2. A `RuntimeMonitor` samples every 5 seconds and exports:
   - `tokio_workers_count` (gauge)
   - `tokio_total_park_count` (counter)
   - `tokio_global_queue_depth` (gauge)
   - `tokio_worker_mean_poll_time_us` (gauge)
   - `tokio_budget_forced_yield_count` (counter)
3. `TaskMonitor` wrappers on session tasks export:
   - `moa_session_task_mean_poll_duration_us` (gauge)
   - `moa_session_task_mean_first_poll_delay_us` (gauge)
4. All metrics exported via step 96's Prometheus endpoint.

---

## 4. Rules

- **`tokio_unstable` only in dev/staging builds.** The `RUSTFLAGS` approach is fine for now; document it in `.cargo/config.toml`.
- **RuntimeMonitor is a single background task.** It polls every 5 seconds and updates Prometheus gauges.
- **TaskMonitor per task class, not per session.** One TaskMonitor for "session tasks", one for "embedding workers", etc. Don't create one per session ID.
- **Graceful degradation.** If built without `tokio_unstable`, the metrics just don't appear. No compilation errors.

---

## 5. Tasks

### 5a. Add dependencies

```toml
tokio-metrics = "0.4"
```

### 5b. `.cargo/config.toml`

```toml
[build]
rustflags = ["--cfg", "tokio_unstable"]
```

### 5c. RuntimeMonitor background task

```rust
#[cfg(tokio_unstable)]
pub fn spawn_runtime_monitor() {
    let handle = tokio::runtime::Handle::current();
    let monitor = tokio_metrics::RuntimeMonitor::new(&handle);
    tokio::spawn(async move {
        for interval in monitor.intervals() {
            metrics::gauge!("tokio_workers_count").set(interval.workers_count as f64);
            metrics::gauge!("tokio_global_queue_depth").set(interval.global_queue_depth as f64);
            metrics::gauge!("tokio_worker_mean_poll_time_us").set(
                interval.mean_poll_duration().as_micros() as f64
            );
            metrics::counter!("tokio_budget_forced_yield_count")
                .increment(interval.budget_forced_yield_count);
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}
```

### 5d. TaskMonitor for session tasks

```rust
let session_monitor = TaskMonitor::new();
// When spawning session tasks:
let instrumented = session_monitor.instrument(session_future);
tokio::spawn(instrumented);
```

### 5e. Tests

- With `tokio_unstable` enabled, `tokio_workers_count` gauge appears in Prometheus scrape output.
- Without `tokio_unstable`, no compilation errors, no metrics.

---

## 6. Deliverables

- [ ] `tokio-metrics` dependency.
- [ ] `.cargo/config.toml` with `tokio_unstable`.
- [ ] `RuntimeMonitor` background task exporting to Prometheus.
- [ ] `TaskMonitor` on session tasks.
- [ ] Conditional compilation for non-unstable builds.

---

## 7. Acceptance criteria

1. Prometheus scrape shows `tokio_workers_count`, `tokio_global_queue_depth`, `tokio_worker_mean_poll_time_us`.
2. Under load, `tokio_worker_mean_poll_time_us` correlates with measured turn latency.
3. `cargo build` without `tokio_unstable` still succeeds.
