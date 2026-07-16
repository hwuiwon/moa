# 17 - Observability

_Turn latency, execution metrics and traces, export lag, and fast interpretation._

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
| `pipeline_compile` | Full context pipeline build. Processor spans such as `history_compiler` remain nested under it. |
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

Healthy trace shape:

```text
session_turn
├── pipeline_compile
│   ├── identity_processor
│   ├── agent_instruction_processor
│   ├── instruction_processor
│   ├── tool_definition_processor
│   ├── query_rewrite
│   ├── skill_injector
│   ├── digest_processor
│   ├── memory_retriever
│   ├── history_compiler
│   ├── runtime_context
│   └── compactor
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

Execution metrics describe durable logical work, not one process's active
workers. A plan has no application fan-out ceiling below its approved run
budget. Restate scoped concurrency, provider concurrency/rate pacing, and
governed tool or hand capacity queue physical work without changing logical
coverage.

### Metrics

All execution metric labels come from closed enums. IDs, plan hashes, artifact
or capability references, prompts, user text, error/gap prose, and entity names
are trace attributes or analytics fields, never Prometheus labels.

| Metric | Type | Labels / use |
|---|---|---|
| `moa_execution_routes_total` | counter | `decision`, `mode`, `reason`; route volume and escalation |
| `moa_execution_planner_calls_total` | counter | `call`, `outcome`; planner repairs and rejection pressure |
| `moa_execution_compile_duration_seconds` | histogram (`DURATION_SECONDS`) | `source`, `outcome`; compiler latency |
| `moa_execution_run_state_transitions_total` | counter | `state`; durable run transitions |
| `moa_execution_task_state_transitions_total` | counter | `state`, `kind`; durable task transitions |
| `moa_execution_run_queue_to_start_seconds` | histogram (`DURATION_SECONDS`) | no labels; backpressure before first work |
| `moa_execution_task_duration_seconds` | histogram (`DURATION_SECONDS`) | `kind`, `outcome`; terminal task duration |
| `moa_execution_task_retries_total` | counter | `kind`, `failure_class`; accepted new generations only |
| `moa_execution_map_fanout_items` | histogram (`CARDINALITY`) | no labels; first committed map materialization |
| `moa_execution_run_cost_microusd` | histogram (`COST_MICROUSD`) | `usage=reserved|actual` |
| `moa_execution_run_tokens` | histogram (`TOKENS`) | `usage=reserved|actual` |
| `moa_execution_run_tasks` | histogram (`CARDINALITY`) | `usage=reserved|actual` |
| `moa_execution_run_tool_calls` | histogram (`CARDINALITY`) | `usage=reserved|actual` |
| `moa_execution_run_retrieved_bytes` | histogram (`BYTES`) | `usage=reserved|actual` |
| `moa_execution_run_coverage_ratio` | histogram (`RATIO`) | terminal `status` |
| `moa_execution_reducer_depth` | histogram (`CARDINALITY`) | `kind=capability|agent` |
| `moa_execution_runs_terminal_total` | counter | terminal `status`, typed `reason` |

Bucket boundaries are shared by registration and render tests; Prometheus adds
`+Inf`:

```text
DURATION_SECONDS = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60, 120, 300, 600, 1800, 3600]
CARDINALITY = [0, 1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1000, 2500, 5000, 10000]
COST_MICROUSD = [0, 100, 1000, 10000, 100000, 1000000, 10000000, 100000000, 1000000000]
TOKENS = [0, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536, 131072, 262144, 524288, 1048576]
BYTES = [0, 1024, 4096, 16384, 65536, 262144, 1048576, 4194304, 16777216, 67108864, 268435456, 1073741824]
RATIO = [0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1]
```

The bounded label values are:

- route: `decision=needs_input|routed`,
  `mode=none|respond|act|run`, and
  `reason=simple_response|bounded_interactive_work|preflight_input_missing|explicit_run|bulk_collection|durable_or_resumable|high_fanout|approval_or_signal|selected_execution_template|act_escalation`;
- planner:
  `call=initial_plan|initial_repair|amendment|amendment_repair` and
  `outcome=accepted|needs_input|unsupported|schema_rejected|immutable_goal_changed|compiler_rejected|oversized|provider_error`;
- compiler:
  `source=generated_plan|skill_template|experiment_template|amendment|skill_regression`
  and `outcome=accepted|needs_input|unsupported|rejected`;
- run state:
  `awaiting_confirmation|queued|running|waiting_input|waiting_review|waiting_replan|completed|partial|blocked|unsupported|failed|cancelled`;
- task state:
  `pending|reserved|running|waiting_input|waiting_replan|completed|skipped|failed|cancelled`;
- task kind:
  `capability|agent|review|wait_signal|output|completion_verifier`;
- terminal task outcome: `completed|skipped|failed|cancelled`;
- failure class:
  `retryable|dependency_failed|invalid_input|invalid_output|authorization_denied|budget_exceeded|deadline_exceeded|cancelled|unsupported|terminal`;
- coverage and terminal status:
  `completed|partial|blocked|unsupported|failed|cancelled`;
- terminal reason:
  `completed|goal_incomplete|budget_exceeded|deadline_exceeded|cancelled|no_progress|duplicate_plan|duplicate_amendment|repeated_failure|budget_exhausted|task_failure|unsupported_plan|blocked|internal_failure`.

Emission points are durable and one-shot. Route emits after Session acceptance;
planner emits once per completed provider call, including provider errors;
compiler duration wraps each real compiler invocation. State counters emit only
after a committed transition. Queue-to-start observes the sole first
queued-to-running transition as `started_at - queued_at`, clamped at zero;
pre-confirmation cancellation has no observation. Task duration observes
terminalization from `started_at`, or from `created_at` when the task never
started. Map fan-out and reducer depth emit only for first committed
materialization. All five reserved/actual histograms, coverage, and terminal
count emit only on the sole nonterminal-to-terminal run update.

Mutation metrics emit only from committed `Applied` evidence. A semantically
identical retry returns `Replayed` and emits nothing, including
commit-before-handler-result recovery. Conflicts, stale generations, read paths,
and repeated transport sends likewise do not increment mutation metrics.

Operationally:

- high queue-to-start with normal task duration means backpressure, not slow
  task execution;
- rising retry counts identify generation churn by typed failure class;
- map fan-out and reducer depth show the compiled execution shape;
- terminal `reserved` observations must be zero for all five budget dimensions;
  a non-zero value is a reservation-leak invariant violation;
- coverage is `satisfied requirements / total requirements`, with `0/0 = 1.0`.

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
  -> ExecutionRun
  -> ExecutionTask
  -> model call or governed capability/tool call
  -> ActionPolicy and optional action review
  -> action-review resolution outbox retry
  -> resumed ExecutionTask
  -> ExecutionRun fan-in
  -> terminal synthesis turn
```

Execution spans may additionally carry capability name/version, originating
turn ID, action-review ID, and synthesis turn ID as attributes only.

Exact W3C propagation is reserved for contexts persisted as durable data rather
than reconstructed from the current handler attempt. Action reviews preserve
two distinct contexts. Review creation stores the
original execution-task context as the future link target. Terminal resolution
stores the resolver's current context as the retry callback's remote parent.
The reaper reinjects the resolution parent; `ExecutionTask/resolve_action_review`
adopts it and links the separately stored original task context. Replay and
claim retry preserve both byte-for-byte. Invalid `traceparent` is treated as
absent; invalid `tracestate` is dropped while a valid parent remains.
Non-empty `tracestate` follows W3C Level 2 limits and MOA's 512-byte cap.

## ClickHouse Lineage And Analytics Export

When `[clickhouse]` is configured, two background flows carry their own
metrics:

- Lineage sink (rows to ClickHouse instead of Timescale):
  `moa_lineage_compliance_chain_skipped_total` counts rows written without
  `prev_hash` links because compliance chaining requires the Postgres backend
  — a non-zero rate on a compliance tenant is a misconfiguration signal.
- Analytics exporter (`moa-orchestrator/src/analytics_export/`):
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
