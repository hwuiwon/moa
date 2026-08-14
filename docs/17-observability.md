# 17 - Observability

_Turn latency, execution traces, export lag, and fast interpretation._

## Turn Latency

Each `session_turn` trace emits four named child spans:

```text
session_turn
├── pipeline_compile
├── llm_call
├── tool_dispatch
└── event_persist
```

| Span | Covers |
|---|---|
| `pipeline_compile` | Full context pipeline build. Processor spans such as `history` remain nested under it. |
| `llm_call` | Provider request and streamed response lifetime, including TTFT. |
| `tool_dispatch` | Tool-call coordination for the turn. Individual tools appear as spans such as `tool:file_read`. |
| `event_persist` | Turn commit overhead: event writes, status updates, and post-turn store updates. |

The `session_turn` root span records:

- `moa.turn.pipeline_compile_ms`
- `moa.turn.llm_call_ms`
- `moa.turn.tool_dispatch_ms`
- `moa.turn.event_persist_ms`
- `moa.turn.llm_ttft_ms`

The `llm_call` span records GenAI provider, operation, request/response model,
usage, cache token counts, and time to first chunk through `gen_ai.*`
attributes. MOA-specific cost and cache-rate details stay under `moa.*`.

### Telemetry Resource Identity

Traces, metrics, and structured logs share one OpenTelemetry resource. A non-empty
`MOA_SERVICE_INSTANCE_ID` becomes `service.instance.id`; Kubernetes injects the
pod UID for both edge and orchestrator so collector discovery and backend
series remain per-pod across rollout. Setting the collector base URL is the
single OTLP switch; it enables all three signals. `observability.otlp_headers`
applies to every OTLP exporter over either supported transport. Structured JSON
stdout remains available for container operations while the OTLP log bridge
provides the correlated backend copy.

Healthy trace shape:

```text
session_turn
├── pipeline_compile
│   ├── identity
│   ├── agent_instructions
│   ├── instructions
│   ├── tools
│   ├── query_rewrite
│   ├── history
│   ├── skills
│   ├── memory_digest
│   ├── graph_memory
│   └── runtime_context
├── llm_call
├── tool_dispatch
└── event_persist
```

Fast interpretation:

- `llm_call` dominates: model/provider latency is the primary lever.
- `pipeline_compile` grows over a session: inspect replay and compiled context
  size.
- `tool_dispatch` dominates: inspect shell commands, file scans, or repeated
  tool loops.
- `event_persist` is high: inspect session-store writes and post-turn
  maintenance.

## Durable Progress

Hosted session streams do not depend on process-local broadcast tails for
correctness. `moa-edge` reads the contact/session progress projection through
the orchestrator, emits durable session events in sequence, and adds transient
progress frames for active turns when the progress cursor changes.

Polling behavior:

- the stream polls progress at a 1s baseline while durable events are flowing;
- repeated polls without durable events grow the interval gradually;
- elapsed-time-only progress changes do not emit new transient frames;
- terminal SSE frames are derived from the durable progress snapshot.

Progress itself is written through Restate workflow state and session events.
`TurnExecution/progress` and `Session/progress` are the cross-process boundary;
Slack/chat channels may also receive live status edits when channel progress
delivery is enabled. Treat process-local broadcast channels as test-harness or
in-process helper plumbing, not as the hosted observation source of truth.

## Execution Runs

Durable execution is observed through `moa.execution_run` and
its node/task/attempt/compensation/trigger/outbox/external-job projections,
bounded Restate activation state, compact session events, and trace attributes.
Pending and every waiting phase are storage-only; only admitted attempts may
own active capacity or hands.

Fleet health uses bounded labels only. Alert-driving series cover oldest-ready age,
overdue deadlines, trigger/outbox lag and dead letters, oldest active-attempt and
external-job age, admission utilization by resource and fleet/tenant-peak scope,
durable reconciliation and retention last-success ages, and parked tasks retaining
hands. Bounded aggregate diagnostics such as the per-phase run census and tenant
maximum share remain available to operational dashboards even when they do not
have a dedicated alert. None carry a tenant, run, task, deployment, or provider
account identifier. IDs belong in traces and Postgres drilldown.

`k8s/scripts/validate-observability.sh` enforces the invariant directly: every
`pub fn record_*` in `runtime_metrics.rs` must have a caller outside that file and
outside `tests/`, so a recorder can never again be declared without a production
producer. Alert and dashboard inventories separately pin the operational consumers.

Reconciliation and retention expose separate durable health receipts. Trigger/outbox
repair drives `moa_execution_maintenance_*`; terminal-evidence retention drives
`moa_execution_retention_*`. A missing receipt exports as unready with infinite age.
Retention normally completes a bounded pass at least once per hour, so
`MOAExecutionRetentionStale` warns when the receipt is absent, unready, or older than
two hours. No retention backlog series is exported until the repository can provide a
bounded, authoritative backlog snapshot.

### Replay-Safe Trace Correlation

Journaled Restate calls and sends do not inject the handler's current
`traceparent`. Handler spans are attempt-local, so rebuilding that header after
a process restart would change the Restate command and cause a journal
mismatch. Durable execution hops are correlated by bounded domain attributes,
with `moa.execution.run_uid` present on admission, Session activation, run, and
task spans. Task-specific spans also carry `task_id`, `plan_hash`,
`plan_revision`, and `node_id`.

The operational path remains:

```text
session turn
  -> route / planner / compiler
  -> ExecutionRunController activation
  -> ExecutionTaskAttempt activation
  -> model call or governed capability/tool call
  -> ActionPolicy and optional action review
  -> persisted wait/trigger or action-review dispatch
  -> later bounded attempt/controller activation
  -> terminal synthesis turn
```

Execution spans may additionally carry capability name/version, originating
turn ID, action-review ID, and synthesis turn ID as attributes only.

Exact W3C propagation is reserved for contexts persisted as durable data rather
than reconstructed from the current handler attempt. Action reviews preserve
two distinct contexts. Review creation stores the
original execution-task context as the future link target. Terminal resolution
stores the resolver's current context as the retry callback's remote parent.
The maintenance delivery reinjects the resolution parent; the bounded task
resolution activation adopts it and links the separately stored original task context. Replay and
claim retry preserve both byte-for-byte. Invalid `traceparent` is treated as
absent; invalid `tracestate` is dropped while a valid parent remains.
Non-empty `tracestate` follows W3C Level 2 limits and MOA's 512-byte cap.

## Lineage And ClickHouse Analytics Export

Lineage and analytics export are independent background flows:

- Postgres lineage sink: `moa_lineage_written_total` counts durably written
  rows. `[clickhouse]` does not select a lineage backend.
- ClickHouse analytics exporter (`crates/moa-analytics-export/`):
  `moa_analytics_export_lag_seconds{table}` (gauge; freshness of each
  ClickHouse read model — the operative dashboard-staleness signal),
  `moa_analytics_export_rows_total{table}`,
  `moa_analytics_export_skipped_rows_total{table}` (tenant ids that failed
  UUID parsing), and `moa_analytics_export_errors_total`. ClickHouse being
  down surfaces as rising lag with errors incrementing; Postgres and the
  product write path are unaffected, and the exporter resumes from its cursor.

## Behavior-Lab Simulations

Experiment run and trial traces use aggregate metrics for dashboards and trace
attributes for drilldown IDs. Prometheus labels are intentionally bounded:
`status`, `stop_reason`, `target_kind`, `source`, and `role` are safe labels.
Do not add prompt text, persona/profile/scenario text, transcript content,
connector payloads, model output, `run_uid`, `trial_uid`, `session_id`,
`execution_run_uid`, `score_run_id`, trial keys, or artifact revision IDs as
metric labels.

For a slow or failing behavior-lab run:

1. Start with the experiment run UID from the UI/API response. The
   `ExperimentRun` trace records `moa.experiment.run_uid`,
   `moa.experiment.run_score_run_id`, `moa.experiment.session_id` for
   agent-loop targets, and `moa.experiment.execution_run_uid` for execution
   targets.
2. If the run came from an experiment plan, inspect the child trial row for the
   failing `trial_uid`. `ExperimentTrialRun` attaches the active trace ID to the
   trial record after the stable trial row exists and before target execution.
   The trial trace also records `moa.experiment.run_uid`,
   `moa.experiment.trial_uid`, `moa.experiment.trial_key`,
   `moa.experiment.score_run_id`, `moa.experiment.session_id`, and
   `moa.experiment.execution_run_uid` when those links exist.
3. Use the trial `trace_id` to open the trace backend, then pivot by
   `session_id` to the target session trace or by `execution_run_uid` to the
   execution run. Use the run `score_run_id` and trial `score_run_id`
   to inspect score rows without guessing which analytics run belongs to the
   experiment.
4. Check lifecycle metrics first:
   `moa_experiment_runs_total{status,target_kind}` and
   `moa_experiment_trials_total{status,stop_reason,target_kind}`. Terminal trial
   duration lives in the analytics tables, not Prometheus. Action-review pressure
   should be investigated through session `ActionReviewRequested` events and
   tenant action-review rows.
5. For simulator pressure, compare
   `moa_simulation_turns_total{target_kind}`,
   `moa_simulation_tokens_total{role="simulator"}`,
   `moa_simulation_tokens_total{role="target"}`,
   `moa_simulation_cost_cents_total{role="simulator"}`, and
   `moa_simulation_cost_cents_total{role="target"}`. The only role label
   values are `simulator` and `target`. Simulator usage comes from simulator
   model calls; target usage comes from new target session events observed after
   each simulator turn.
6. For score and learning issues, inspect
   `moa_experiment_score_rows_total{source}` and
   `moa_experiment_learning_candidates_total{status}`. The score-row `source`
   label values are exactly `scores`, `trial_rollup`, `trial_breakdown`,
   `scenario_breakdown`, `compare`, `scenario_compare`, and
   `variant_compare`; run IDs stay out of labels.
