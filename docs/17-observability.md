# 17 - Observability

_Turn latency spans, broadcast lag, metrics, and fast interpretation._

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
`procedure_run_uid`, `score_run_id`, trial keys, or artifact revision IDs as
metric labels.

For a slow or failing behavior-lab run:

1. Start with the experiment run UID from the UI/API response. The
   `ExperimentRun` trace records `moa.experiment.run_uid`,
   `moa.experiment.run_score_run_id`, `moa.experiment.session_id` for
   agent-loop targets, and `moa.experiment.procedure_run_uid` for procedure
   targets.
2. If the run came from an experiment plan, inspect the child trial row for the
   failing `trial_uid`. `ExperimentTrialRun` attaches the active trace ID to the
   trial record after the stable trial row exists and before target execution.
   The trial trace also records `moa.experiment.run_uid`,
   `moa.experiment.trial_uid`, `moa.experiment.trial_key`,
   `moa.experiment.score_run_id`, `moa.experiment.session_id`, and
   `moa.experiment.procedure_run_uid` when those links exist.
3. Use the trial `trace_id` to open the trace backend, then pivot by
   `session_id` to the target session trace or by `procedure_run_uid` to the
   procedure run. Use the run `score_run_id` and trial `score_run_id`
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
