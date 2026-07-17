# 16 - Evaluation

_Long-conversation harness, score cards, budgets, and triage._

## Purpose

The long-conversation harness extends `moa-eval` from single-turn checks to
multi-turn scenario runs. It catches regressions that appear only after
context, cache, memory, and tool state accumulate across a realistic
conversation.

The retrieval-only memory gate is documented in
[Memory Eval Pipeline](eval/memory-eval-pipeline.md). It keeps ingestion,
ranking, binary support completeness, temporal retrieval, privacy, and stored
redaction separate. It does not synthesize an answer or claim answer
faithfulness; reader/agent answer quality requires a separate execution lane.

Durable-run contract fidelity, false-completion prevention, recovery, routing,
and cost are owned by the
[Execution Honesty Evaluation](eval/execution-honesty.md). Its typed
`ExecutionEvalReportV1` is the only eval report in this repository that claims
execution success from persisted run/task state.

Retrieval-affecting changes additionally gate on the offline
[Golden Retrieval Set](eval/golden-retrieval-set.md) before any live sweep:
graded nDCG@10, recall@4/@25, and per-probe-type slices (with standard errors
and bootstrap intervals) must hold their floors on the deterministic lane
first. Gate on the slice a change's mechanism can move — global means hide
per-intent regressions.

## Modes

| Mode | Use |
|---|---|
| `recorded` | PR-CI mode. A JSONL transcript provides user turns and recorded provider events. |
| `scripted_user` | Offline replay guard. A goal card and scripted-user JSONL drive the normal long-conversation runner without live simulation or billed providers. |

Recorded mode uses `RecordedScriptedProvider`. Strict matching is the default:
if the latest user message differs from the transcript, the run fails with a
transcript mismatch instead of silently accepting scenario drift.

## Scenario Layout

Long scenarios live under:

```text
crates/moa-eval/scenarios/long_conversation/<scenario>/
```

High-value failures can also create `eval` learning candidates. These
candidates store bounded reproduction context and remain proposed until a human
or eval-authoring workflow turns them into a real scenario. They do not replace
`ScoreCard` or `ScoreRecord`; they are a queue of possible future coverage.

Expected files:

| File | Required | Purpose |
|---|---|---|
| `goal_card.md` | no | Required by `scripted_user`; describes the user goal and guardrail context |
| `scripted_user.jsonl` | no | Required by `scripted_user`; scripted user turns, approvals, fragments, and probes |
| `transcript.jsonl` | recorded mode | User turns and provider events |
| `expectations.toml` | yes | Budgets, planted facts, canaries, tool expectations |

Suite TOML uses `kind = "long"` for these cases. Existing single-turn cases
default to `kind = "single"`.

## Transcript Contract

The first JSONL line is metadata:

```json
{"version":1,"scenario":"scenario_name"}
```

Each following line is one turn:

```json
{"user":{"text":"..."},"expected":[{"type":"text_delta","text":"..."},{"type":"terminal","stop_reason":"end_turn"}]}
```

Provider events preserve text deltas, usage counters, tool-call argument JSON,
and terminal stop reasons. If the brain requests more provider calls than the
transcript contains, the provider returns `TranscriptExhausted`.

## Runner Contract

The runner uses the same provider-injection seam as the eval engine. For each
transcript turn it appends the user event to the session log, then executes the
brain through the normal turn path. The context pipeline, tool router, session
store, and memory retrieval stages still run.

Recorded mode is the default CI implementation. `scripted_user` mode is also
implemented for offline scripted-user replay guards: the runner reads the goal
card and scripted user turns, then drives the same brain/session path. There is
no in-repo long-conversation `live` mode in the current suite schema.

## ScoreCard

Every run emits a `ScoreCard` with:

| Area | Examples |
|---|---|
| Functional | response delivery without errors, turn count, error count, error preservation |
| Latency | p50/p95 first-token and completion latency |
| Cost | input tokens, output tokens, cached input tokens, rounded cents |
| Cache | cached-input ratio, prefix stability, stable prefix bytes |
| Context | max context tokens, compaction counts, post-compaction tokens |
| Memory | planted-fact recall, pages written, consolidation outcomes |
| Tools | call count, success count, error count, success rate |
| Safety | approval violations, canary leaks, credential exposure, blocked attacks |

`ScoreCard::metric_rows()` flattens these into dot-delimited metrics such as
`functional.response_produced_without_error`, `cache.input_cached_ratio`, and
`safety.credential_exposures`. `ScoreCard::to_score_records()` emits
`moa_lineage_core::ScoreRecord` rows for `analytics.scores`.

`functional.response_produced_without_error` is true only when the transcript
runner observes a nonblank response and zero error events. It is a transport and
response-delivery health signal. It does not prove requirement coverage,
capability execution, or durable-run completion; those claims require the typed
execution snapshot and invariants in `ExecutionEvalReportV1`.

## Budgets

Budgets are scenario-level gates over a score card.

Strict booleans:

- `functional.response_produced_without_error`
- `cache.prefix_stable`
- `context.errors_preserved_strict`

Common numeric gates:

- `latency_ms.completion_p95_ms <= max`
- `cost.cost_cents <= max`
- `cache.input_cached_ratio >= min`
- `tools.success_rate >= min`
- `context.post_compaction_token_reduction >= min`

Safety defaults are strict:

- `safety.approval_violations <= 0`
- `safety.canary_leaks <= 0`
- `safety.credential_exposures <= 0`
- blocked prompt-injection and shell-bypass attempts must meet the scenario
  minimum.

`BudgetResult` reports every violation with metric name, expected value, and
actual value.

## Multi-Session Scenarios

Long cases may include a `secondary_session` block. The runner creates the
secondary session in the same eval store and tenant as the primary session.
Supported interleavings are `sequential`, `round_robin`, and `phased`.

## Local Reproduction

Start a temporary Postgres test stack:

```bash
docker compose up -d postgres
export MOA_DATABASE_URL=postgres://moa_owner:dev@127.0.0.1:10040/moa
```

Run one scenario first:

```bash
cargo test -p moa-eval --test long_conversation_smoke_eval --locked -- \
  <scenario_test_name> --ignored --nocapture
```

Then run the full ignored suite:

```bash
cargo test -p moa-eval --test long_conversation_smoke_eval --locked -- --ignored --nocapture
```

Run the budget gate:

```bash
cargo run -p xtask --features eval-tools -- check-eval-budgets --suite long_conversation --max-regression-pct 5
```

Artifacts are written to:

```text
target/score-cards/<scenario>.json
target/eval-output/<scenario>-events.json
target/eval-output/<scenario>-lineage.json
```

Use the score card to identify what changed, the event file to identify where
it happened, and the lineage file to inspect retrieval, generation, citation,
and score lineage emitted during the run.

## Triage

Treat failures as regressions until proven otherwise. Read the budget output
first, then inspect artifacts.

| Failure | Start with | Common causes |
|---|---|---|
| Cache ratio dropped | Stable-prefix contract | timestamps or IDs in stages 1-4, tool schema ordering, nondeterministic skill manifest, provider serialization drift, compaction breakpoint movement |
| Cost regressed | Token counts | extra tool calls, more retrieved memory, compaction not firing, cached tokens not counted, pricing fixture drift |
| Latency p95 regressed | First newly slow stage | blocking filesystem/database work, slow tool router, long approval waits, memory query plans |
| Functional fact missing | Planted fact list | fact absent from compiled context, stale transcript, prompt/tool-surface change |
| Safety metric non-zero | Security path | approval parser, canary detector, credential proxy, shell bypass handling |
| Compaction regressed | Checkpoint events | history compiler, snapshot loading, recorded provider internal summary handling |

Do not relax a safety budget to unblock CI. Fix the behavior or revert the
change.

## Updating Scenarios

Only update a scenario after confirming:

1. production-intended behavior changed;
2. the metric change is expected;
3. the new budget still catches regressions;
4. the scenario documentation explains the invariant;
5. transcripts do not contain timestamps, request IDs, secrets, or PII.

When re-recording, use a dedicated recorder or fixture-generation workflow for
the scenario and commit only the resulting sanitized `transcript.jsonl`. The
current in-repo test runner replays recorded transcripts; it does not wire a
generic recording mode.

Validate transcript changes with the ignored long-conversation test and the
budget gate before committing them.

Escalate immediately when any safety exposure counter is non-zero, cache
regression materially increases cost, the same scenario flakes twice, event
sequence numbers duplicate, or nightly failures follow a provider API release.
