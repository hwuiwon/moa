# Execution Honesty Evaluation

The execution eval is designed to falsify two claims:

1. The generated goal contract preserves what the user asked for.
2. A durable run never reports `completed` for work that its persisted goal
   contract says is incomplete.

These are separate boundaries. A planner can omit a requirement and then
execute its smaller contract perfectly. That is a contract omission, not an
execution false completion. Reports retain both rates.

## Source Of Truth

`ExecutionEvalSnapshot` is a bounded read model over production state. Its
inputs are the same scheduling projection, ordered task records, normalized
planning audits, and session events used to operate the run. The service tests
collect these through `ExecutionRepository` and the normal session event API.
There is no parallel trace format and no attempt to infer state from a model
transcript.

The snapshot retains goal and terminal state, completion-check results, gaps,
budget totals, task identity and status, planning outcomes, bounded event
counts, and fixture-provided logical capability-call evidence. It excludes raw
task outputs, prompts, full event bodies, credentials, and raw transcripts.

`ExecutionEvalReport` is strict and versioned. Unknown fields fail parsing.
Case rows contain stable IDs, typed invariant results, contract scores, route
provenance, observed status, counts, cost, latency, and hashes of terminal
outputs or final responses. They do not contain those outputs or responses.
The aggregate records exact denominators for contract omissions, impossible
cases, false completions, routing errors, repeated-run reliability, cost, and
mutation score.

## Deterministic Invariants

The invariant vocabulary covers terminal status, exact task count, map
coverage, completion checks, gaps, approved budget, task progress, duplicate
logical effects, capability envelopes, preservation of completed work,
bounded session events, and absence of raw task-output events.

`MustNotComplete` marks a deliberately impossible case. The headline false
completion rate is:

```text
impossible cases observed as completed / all impossible cases
```

Its required value is zero. Assertions are predicates over typed rows; an LLM
judge is never used to count tasks, determine durable status, or check budget
arithmetic.

Universe-coverage scenarios use an oracle independent of the datasource under
test. For example, a datasource can return 92% of a declared universe with a
successful response; the goal oracle still expects 100%, so the run must be
`partial`. Using the datasource response itself as the denominator would make
silent incompleteness invisible.

Contradiction is enforced only when the goal declares a conflict deliverable or
completion check. The harness does not claim to infer arbitrary domain
contradictions globally.

## Route Classification

Ordinary user turns make at most one bounded auxiliary-model classifier call.
The classifier receives the objective and bounded structural signals, has no
tools, retrieval, or native web access, uses temperature zero, and may emit at
most 256 output tokens. Its strict JSON result selects Respond, Execute, or
NeedsInput with a bounded free-form rationale, confidence basis points, and
bounded missing-input list. Execute must also supply exactly one explicit
internal strategy: Inline or Durable. The rationale is not scored for exact
wording, never selects the strategy, and remains ephemeral to the active turn.

The router does not search the user text for phrases. It does not retry, repair,
recurse, or invoke the planner. Provider failure, collection failure, oversized
or malformed output, an invalid label/strategy/rationale shape, or insufficient confidence
falls back to Execute/Inline. Attachments or a recent target also prevent a
classifier from choosing context-free Respond or NeedsInput. Uncertainty
therefore cannot fabricate a direct answer or start expensive durable work.

Trusted typed paths remain deterministic: blank input requests clarification;
an exact pinned template chooses Execute/Durable; internal execution synthesis
chooses Respond. Only an initial root Execute/Inline turn may make one typed,
evidence-preserving upgrade to Durable without reclassification or downgrade.
Production grants that authority only through the workflow-owned
`request_durable_execution` control tool, which is injected only for the
eligible turn and must be called alone; arbitrary tool results are not upgrade
signals.
Route audits persist only redacted typed provenance: route and strategy, source
and outcome, model and prompt version, hashes, confidence, usage, cost, and
duration. They do not persist classifier rationale.

The corpus labels public route and internal strategy separately. Public-route
cost compares Respond, Execute, and NeedsInput; Respond on a true Execute case
is the catastrophic error. Strategy cost is computed only where both expected
and observed routes are Execute and compares Inline with Durable. The report
separately records weighted routing cost, weighted strategy cost,
Respond-on-Execute rate, near-boundary Inline recall, Durable strategy recall,
upgrade recall/evidence preservation, NeedsInput errors, classifier fallback,
tokens, cost, and latency. A public-route miss is never counted as a strategy
miss.

## Evaluation Lanes

| Lane | Provider | Policy |
|---|---|---|
| Offline PR | scripted/replayed | Hard gate for property tests, routing, contract fidelity, report validation, and zero false completion |
| Service PR | scripted | Hard gate for the bounded named Restate production-path matrix |
| Nightly deterministic | scripted | Full execution service binary, including 500-item fan-out, restart, and adversarial scenarios |
| Weekly mutation | none/scripted tests | Targeted guard-removal test; mutation score must be at least 0.90 |
| Nightly live | real provider | Optional sampled trend only; one sample never blocks a merge |

The checked nextest profiles are `execution-eval-pr` and
`execution-eval-nightly`. Every invocation uses `--no-tests fail`, so a stale or
empty filter cannot produce a green lane.

Run the offline gate:

```bash
cargo test -p moa-execution --locked
cargo test -p moa-brain --lib --locked execution_routing --no-fail-fast
cargo test -p moa-eval --test eval_offline --locked execution_ --no-fail-fast
cargo run -p xtask --locked --features eval-tools -- execution-eval run-offline \
  --manifest crates/moa-eval/scenarios/execution/manifest.toml \
  --output target/execution-eval/offline.json
cargo run -p xtask --locked --features eval-tools -- execution-eval check \
  --report target/execution-eval/offline.json \
  --max-execution-false-completion-rate 0 \
  --max-respond-on-execute-rate 0 \
  --max-weighted-routing-cost 0 \
  --max-weighted-strategy-cost 0 \
  --min-near-boundary-inline-recall 1 \
  --min-durable-strategy-recall 1
```

Build the exact fixture binary and run the bounded service lane:

```bash
cargo build -p moa-orchestrator --bin moa-orchestrator-bin \
  --features provider-overrides,integration,execution-planning-failpoints \
  --locked
MOA_ORCHESTRATOR_BIN="$PWD/target/debug/moa-orchestrator-bin" \
MOA_CLOUD_HANDS_ALLOW_LOCAL=true \
cargo nextest run -p moa-orchestrator --locked \
  --features provider-overrides,integration,execution-planning-failpoints \
  --profile execution-eval-pr \
  --run-ignored ignored-only --no-tests fail
```

Replace the profile with `execution-eval-nightly` for the complete deterministic
matrix. `MOA_RUN_LIVE_E2E=1 scripts/run-clean-e2e.sh --live` includes the
bounded matrix. Adding `--long-eval` also runs the full deterministic matrix and
the existing long-conversation smoke eval.

Run the mutation gate with:

```bash
scripts/run-execution-mutation-eval.sh
```

It previews nonempty, explicit routing, workflow-control, and execution-runtime
mutant sets, runs each against its owning focused test lane, and writes per-lane
selection, mutation, and report logs; an append-only phase ledger; a final
status with both lane and cargo-mutants exit codes; and score evidence plus one
stable aggregate report under `target/execution-mutants/`. CI uploads the
complete tree even when a lane fails before aggregation. Each nonempty
`selected-mutants.txt` is persisted before that lane starts mutation execution,
so baseline and configuration failures retain the exact attempted selection.
The aggregate score must meet the configured threshold; timeouts are not
counted as caught mutants.

## Live Sampling And Calibration

The live corpus contains 20 cases and every case runs five times with
independent session and run IDs. The report records `pass_at_1`, `pass_all_k`,
binary variance, cost per success, task-count ratio, contract score, execution
invariants, separate weighted public-route and internal-strategy costs, and
classifier fallback telemetry. The complete S&P 500 query must never route to
Respond in any repetition.

Before any provider call, the lane forecasts the whole 20-by-5 batch through
the existing eval cost ledger. It refuses to start unless the forecast fits an
explicit positive `MOA_EXECUTION_EVAL_BUDGET_USD`. Provider credentials do not
implicitly authorize spend.

Run it only after explicit authorization:

```bash
cargo build -p moa-orchestrator --bin moa-orchestrator-bin \
  --features integration --locked
MOA_RUN_LIVE_EXECUTION_EVALS=1 \
MOA_EXECUTION_EVAL_BUDGET_USD=<approved-budget> \
MOA_ORCHESTRATOR_BIN="$PWD/target/debug/moa-orchestrator-bin" \
cargo test -p moa-orchestrator --test execution_eval_provider_e2e \
  --features integration --locked -- --ignored --test-threads=1 --nocapture
```

Deterministic structured expectations decide execution correctness. Optional
semantic judge scores require a calibration artifact with exactly 100 items,
two independent human labels per item, an adjudicated label, and the judge
label. Human label production happens outside the test code. Calibration is
accepted only when raw agreement is at least 0.90, Cohen's kappa is at least
0.80, and judge accuracy against adjudication is at least 0.85.

Reports mark judge calibration as `unavailable`, `calibrated`, or `rejected`.
Missing labels never fabricate a pass. A rejected or unavailable calibration
leaves judged metrics unavailable while deterministic invariants remain valid.

Live reports are compared only when corpus hashes, seeds, repetition count, and
case IDs match. The comparison uses paired statistics and gates only a
statistically significant regression larger than the configured practical
effect. One stochastic failure is retained as evidence but does not determine
the process result.

## Production Feedback

Every execution incident becomes a stable corpus or service scenario keyed by
its persisted failure fingerprint or minimum run evidence. The corpus grows
monotonically; old incidents are not deleted to improve scores. Operational
details are in [Data Operations](../19-data-operations.md).
