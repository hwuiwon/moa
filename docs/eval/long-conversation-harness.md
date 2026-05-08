# Long-Conversation Eval Harness

The long-conversation harness extends `moa-eval` from single-turn checks to
multi-turn scenario runs. It is intended to catch regressions that only appear
after context, cache, memory, and tool state accumulate over a realistic
conversation.

## Modes

The harness has three planned modes.

`recorded` is the PR-CI mode. A JSONL transcript contains the user message for
each turn and the provider events that should be replayed in response. The
`RecordedScriptedProvider` advances one transcript turn per provider call and,
by default, rejects a request whose latest user message differs from the
transcript. This makes scenario drift explicit instead of silently accepting a
different conversation.

`scripted_user` is reserved for future simulation mode. The user side will be
driven from a goal card and rubric while the provider can be recorded or live.
Prompt 16 only defines the enum value so scenario files can remain stable.

`live` is reserved for nightly or manual runs against real providers. Live mode
must never run in PR CI by default and remains outside this foundation.

## Scenario Layout

Long-conversation scenarios live under:

```text
crates/moa-eval/scenarios/long_conversation/<scenario>/
```

The expected files are:

```text
goal_card.md          # optional, used by future scripted_user mode
transcript.jsonl      # required for recorded mode
expectations.toml     # budgets, planted facts, canaries, and tool expectations
```

Suite TOML uses the same `[[cases]]` list as single-turn evals:

```toml
[[cases]]
kind = "long"
name = "scenario_name"
goal_card = "scenarios/long_conversation/scenario_name/goal_card.md"
transcript = "scenarios/long_conversation/scenario_name/transcript.jsonl"
expectations = "scenarios/long_conversation/scenario_name/expectations.toml"
mode = "recorded"
```

`kind = "single"` remains the default for existing cases, so old suites do not
need to change.

## Transcript Replay

Recorded transcripts use `moa-test-support::transcript::Transcript`.

The first JSONL line is metadata:

```json
{"version":1,"scenario":"scenario_name"}
```

Each following line is one turn:

```json
{"user":{"text":"..."},"expected":[{"type":"text_delta","text":"..."},{"type":"terminal","stop_reason":"end_turn"}]}
```

The provider returns recorded `ProviderEvent` values in order. Text deltas,
usage counters, tool-call argument JSON, and terminal stop reasons are preserved
as transcript data. If the brain asks for one more provider call than the
transcript contains, the provider returns `TranscriptExhausted`. If strict mode
sees a different latest user message, it returns `TranscriptMismatch`.

Strict matching is the default constructor behavior. Loose matching exists only
for early scenario authoring when the user side is still changing.

## Runner Contract

The runner uses the same provider-injection seam as the existing eval engine.
For each transcript turn it appends the user event to the session log and then
executes the brain through `run_streamed_turn`. The first turn is emitted as
`UserMessage`; later turns are emitted as `QueuedMessage`. The normal context
pipeline, tool router, session store, and memory retrieval stages still run.

The foundation runner implements recorded mode only. Scripted-user and live
mode remain explicit future modes so scenario files do not need a schema break
later.

## ScoreCard Schema

Every run emits a `ScoreCard`:

```rust
pub struct ScoreCard {
    pub scenario: String,
    pub run_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub provider: String,
    pub functional: FunctionalScores,
    pub latency_ms: LatencyScores,
    pub cost: CostScores,
    pub cache: CacheScores,
    pub context: ContextScores,
    pub memory: MemoryScores,
    pub tools: ToolScores,
    pub safety: SafetyScores,
}
```

`functional` contains task completion, turn count, error count, and error
preservation.

`latency_ms` contains p50 and p95 first-token and completion latency. The
foundation records aggregate latency until per-turn timing is available.

`cost` contains input tokens, output tokens, cached input tokens, and final
rounded cost in cents.

`cache` contains cached-input ratio, prefix stability, and the longest stable
prefix byte count.

`context` contains maximum context tokens, compaction count, and strict
error-preservation status.

`memory` contains planted-fact recall, memory pages written, and consolidation
success/failure counts.

`tools` contains tool-call count, successful tool-call count, tool-error count,
and success rate.

`safety` contains approval violations, canary leaks, and credential exposures.
Production budgets expect all three safety counters to be zero.

## Analytics Integration

`ScoreCard::metric_rows()` flattens sub-struct fields into dot-delimited metric
rows such as:

```text
functional.task_completed
latency_ms.completion_p95_ms
cost.cost_cents
cache.input_cached_ratio
safety.credential_exposures
```

`ScoreCard::to_score_records()` converts those rows into
`moa_lineage_core::ScoreRecord` values. Each row targets the session, uses
`ScoreSource::OfflineReplay`, carries the scenario in `model_or_evaluator`, and
uses the score card `run_id`. The existing lineage sink routes
`LineageEvent::Eval(ScoreRecord)` into `analytics.scores`, so the harness does
not invent a parallel persistence path.

## Budgets

Budgets are scenario-level gates over a `ScoreCard`.

Required booleans are strict equality checks:

```text
functional.task_completed
cache.prefix_stable
context.errors_preserved_strict
```

Optional numeric budgets are min/max checks:

```text
latency_ms.completion_p95_ms <= configured max
cost.cost_cents <= configured max
cache.input_cached_ratio >= configured min
tools.success_rate >= configured min
```

Safety budgets default to strict zero:

```text
safety.approval_violations <= 0
safety.canary_leaks <= 0
safety.credential_exposures <= 0
```

`BudgetResult` reports every violation with the metric name, expected value,
and actual value. It is designed to be printable in CI logs and ingestible by
future dashboard tooling.

## Authoring Notes

Start new scenarios in recorded mode. Keep the transcript small while building
the rubric, then expand it to the intended long-conversation shape. Run the
foundation tests first, then the scenario-specific eval target.

Do not redact cache-control or usage data from transcripts when the scenario is
about cache behavior. Do redact request IDs, timestamps, credentials, and any
other nondeterministic or sensitive payloads.

When a recorded provider mismatch occurs, treat it as a useful signal. Either
the scenario transcript is stale, or the brain changed the user-facing request
sequence. Update the transcript only after confirming the behavior change is
intentional.
