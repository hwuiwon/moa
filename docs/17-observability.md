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

## Broadcast Lag

MOA uses Tokio broadcast channels for live session updates:

- `event_tx` for persisted session-event previews;
- `runtime_tx` for live runtime updates used by gateway/API observers.

When a subscriber falls behind, Tokio returns `RecvError::Lagged(n)`. MOA does
not treat that as fatal for best-effort live previews.

Signals to watch:

- warn logs containing `broadcast subscriber fell behind, dropped events`;
- `moa_broadcast_lag_events_dropped_total`.

Important labels:

- `channel=event`
- `channel=runtime`
- `policy=skip_with_gap|backfill_from_store|abort`

Do not put `session_id` on this Prometheus counter. Keep session-specific lag
details in logs or traces and use durable event replay for drilldown.

Runtime behavior:

| Policy | Behavior | Use |
|---|---|---|
| `SkipWithGap` | Emit a gap marker and refresh from durable session log | Gateway/API observers |
| `BackfillFromStore` | Reload from `SessionStore::get_events` after last sequence | Complete ordered consumers |
| `Abort` | Stop the consumer | Automated observers that are cheaper to restart |

Interpretation:

- high `event` lag means the event-preview subscriber is slow or the buffer is
  undersized;
- high `runtime` lag means a live UI or relay subscriber is not draining fast
  enough;
- zero counters under normal load means there is no reason to increase channel
  sizes.

## Behavior-Lab Simulations

Experiment run and trial traces use aggregate metrics for dashboards and trace
attributes for drilldown IDs. Prometheus labels are intentionally bounded:
`status`, `stop_reason`, `target_kind`, `source`, and `role` are safe labels.
Do not add prompt text, persona/profile/scenario text, transcript content,
connector payloads, model output, `run_uid`, `trial_uid`, `session_id`,
`workflow_run_uid`, `score_run_id`, trial keys, or artifact revision IDs as
metric labels.

For a slow or failing behavior-lab run:

1. Start with the experiment run UID from the UI/API response. The
   `ExperimentRun` trace records `moa.experiment.run_uid`,
   `moa.experiment.run_score_run_id`, `moa.experiment.session_id` for
   agent-loop targets, and `moa.experiment.workflow_run_uid` for workflow
   targets.
2. If the run came from an experiment plan, inspect the child trial row for the
   failing `trial_uid`. `ExperimentTrialRun` attaches the active trace ID to the
   trial record after the stable trial row exists and before target execution.
   The trial trace also records `moa.experiment.run_uid`,
   `moa.experiment.trial_uid`, `moa.experiment.trial_key`,
   `moa.experiment.score_run_id`, `moa.experiment.session_id`, and
   `moa.experiment.workflow_run_uid` when those links exist.
3. Use the trial `trace_id` to open the trace backend, then pivot by
   `session_id` to the target session trace or by `workflow_run_uid` to the
   artifact workflow run. Use the run `score_run_id` and trial `score_run_id`
   to inspect score rows without guessing which analytics run belongs to the
   experiment.
4. Check lifecycle metrics first:
   `moa_experiment_runs_total{status,target_kind}`,
   `moa_experiment_trials_total{status,stop_reason,target_kind}`, and
   `moa_experiment_trial_duration_seconds{status,target_kind}`. Action-review
   pressure should be investigated through session `ActionReviewRequested`
   events and workspace action-review rows.
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
