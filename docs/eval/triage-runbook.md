# Long-Conversation Eval Triage Runbook

This runbook is for PR or nightly failures in the long-conversation eval suite.
The suite is designed to fail before production behavior degrades, so treat a
failure as a regression until proven otherwise.

## First Response

1. Open the failing GitHub Actions run.
2. Find the `long-conv recorded` or `long-conv nightly` job.
3. Read the `check-eval-budgets` output first.
4. Download the failure artifact.
5. Identify the scenario and metric from the violation.
6. Reproduce the same scenario locally.
7. Compare the score card against the last passing run.
8. Inspect the event output for the failing turn.
9. Only update a transcript after confirming the behavior change is intended.

The budget output should look like this:

```text
Budget violations:
  scenario: code_task_30_turns_with_str_replace_and_recovery
    cache.input_cached_ratio: expected >= 0.65, actual 0.58
    cost.cost_cents: expected <= 150, actual 167

Total: 1 scenario failed, 2 metric violations.
```

The scenario name is the repro target. The metric name tells you which failure
category to start with.

## Local Reproduction

Start the local Postgres test stack or use an existing temporary Postgres URL.

```bash
docker compose up -d postgres
export MOA_TEST_POSTGRES_URL=postgres://moa_owner:dev@127.0.0.1:25432/moa
```

Run one failing scenario first:

```bash
cargo test -p moa-eval --test long_conversation_smoke --locked -- \
  code_task_30_turns_with_str_replace_and_recovery_meets_budgets \
  --ignored --nocapture
```

Then run the full suite:

```bash
cargo test -p moa-eval --test long_conversation_smoke --locked -- --ignored --nocapture
```

Run the budget gate over the emitted score cards:

```bash
cargo run -p xtask -- check-eval-budgets --suite long_conversation --max-regression-pct 5
```

The smoke test writes artifacts to:

```text
target/score-cards/<scenario>.json
target/eval-output/<scenario>-events.json
```

Use those files before reading the entire trace. The score card tells you what
changed. The event file tells you where it happened.

## Analytics Comparison

The dashboard and nightly trend collector use `analytics.scores`. To inspect a
scenario directly:

```sql
SELECT
  replace(model_or_evaluator, 'long_conversation:', '') AS scenario,
  name AS metric,
  COALESCE(value_numeric::text, value_boolean::text, value_categorical) AS value,
  ts AS recorded_at,
  run_id
FROM analytics.scores
WHERE model_or_evaluator = 'long_conversation:<failing_scenario>'
ORDER BY ts DESC
LIMIT 100;
```

To compare one metric over time:

```sql
SELECT
  ts,
  value_numeric
FROM analytics.scores
WHERE model_or_evaluator = 'long_conversation:<failing_scenario>'
  AND name = 'cache.input_cached_ratio'
ORDER BY ts DESC
LIMIT 20;
```

If the CI artifact has a score card but `analytics.scores` does not, the
scenario ran but the nightly persistence path did not publish. Triage that as
observability or CI plumbing, not as brain behavior.

## Failure Categories

### Cache Ratio Dropped

Start with the stable-prefix contract.

Common signs:

- `cache.input_cached_ratio` is below budget.
- `cache.prefix_stable` is false.
- Cost increased at the same time.

Check these code paths:

- `crates/moa-brain/src/pipeline/identity.rs`
- `crates/moa-brain/src/pipeline/instructions.rs`
- `crates/moa-brain/src/pipeline/tools.rs`
- `crates/moa-brain/src/pipeline/cache.rs`
- provider snapshot tests in `crates/moa-providers/tests/`

Likely causes:

- A timestamp or run ID entered stages 1-4.
- Tool schemas changed order.
- Skill manifest ordering became nondeterministic.
- Provider JSON serialization order changed.
- Compaction moved cache breakpoints.

Example:

```text
cache.input_cached_ratio: expected >= 0.70, actual 0.42
```

Re-run the provider wire-byte snapshots and compare the cache-control markers.
If the stable prefix changed intentionally, update the scenario budget only
after the new cost profile is accepted.

### Cost Regressed

Start with token counts, not prices.

Common signs:

- `cost.cost_cents` exceeds budget.
- `cost.input_tokens` increased.
- `cache.input_cached_ratio` dropped.

Check:

- `target/score-cards/<scenario>.json`
- `target/eval-output/<scenario>-events.json`
- provider pricing fixtures in `moa-test-support`
- provider request snapshots

Likely causes:

- Extra tool calls.
- More retrieved memory than expected.
- Compaction not firing.
- Cached input tokens no longer counted.
- Pricing fixture drift.

Example:

```text
cost.cost_cents: expected <= 150, actual 167
```

If token counts are stable but cost changed, inspect pricing. If token counts
changed, find the first turn where the input token count diverges.

### Latency P95 Regressed

Start with unexpected blocking work.

Common signs:

- `latency_ms.completion_p95_ms` exceeds budget.
- The event log has the same functional outcome.
- Cost is unchanged.

Check:

- OTel spans for the failing run.
- Any new synchronous filesystem or database work.
- Tool router execution latency.
- Long-running approval waits.

Example:

```text
latency_ms.completion_p95_ms: expected <= 4200, actual 6800
```

If `MOA_EVAL_TRACE=1` was enabled, open the per-turn `pipeline_trace.json`.
Find the first turn with a new slow stage. If the slow stage is memory
retrieval, inspect graph/vector query plans.

### Functional Fact Missing

Start with the planted fact list in `expectations.toml`.

Common signs:

- The test fails with `planted fact not recalled`.
- The budget gate may still pass.
- The final response omits a required marker.

Check:

- The scenario README for the invariant.
- The relevant transcript turn.
- The compiled context at the turn where the fact should be recalled.
- Recent prompt or tool-surface changes.

Example:

```text
planted fact not recalled for research_task_with_web_fetch_and_memory_writes:
cross_turn_fact_recall_in_turn_18
```

This usually means either the brain no longer saw the prior fact, or the
transcript no longer matches intended behavior. Do not patch the marker into
the transcript unless the underlying behavior is still correct.

### Safety Metric Non-Zero

Stop normal triage and treat this as a security regression.

Common signs:

- `safety.approval_violations > 0`
- `safety.canary_leaks > 0`
- `safety.credential_exposures > 0`
- A blocked-attempt metric is lower than required.

Check:

- `prompt_injection_in_tool_results`
- `shell_chaining_bypass_attempt_in_long_conversation`
- `canary_token_must_not_leak_through_tool_chain`
- approval rule parser and matcher
- canary detector and credential proxy

Examples:

```text
safety.approval_violations: expected <= 0, actual 1
safety.canary_leaks: expected <= 0, actual 1
safety.shell_bypass_attempts_blocked: expected 1, actual 0
```

Do not relax a safety budget to unblock CI. Fix the behavior or revert the
change that caused it.

### Compaction Regressed

Start with checkpoint events.

Common signs:

- `context.compaction_events` is zero for the compaction scenario.
- `context.errors_preserved_strict` is false.
- `context.post_compaction_token_reduction` misses budget.
- Cache prefix stability fails across a compaction boundary.

Check:

- `Event::Checkpoint` in the event artifact.
- `HistoryCompiler` snapshot and checkpoint logic.
- `RecordedScriptedProvider` handling of internal summarization requests.
- Recent changes to context snapshot loading.

Example:

```text
context.post_compaction_token_reduction: expected >= 0.55, actual 0.20
```

If a current incremental snapshot bypasses checkpoint eligibility, the scenario
will fail even though the transcript is valid. Fix the history compiler rather
than lowering the threshold.

## Updating Scenarios

Only update a scenario after answering these questions:

1. Did production-intended behavior change?
2. Did the score-card metric change for a good reason?
3. Is the new budget still strict enough to catch regressions?
4. Does the scenario README explain the invariant?
5. Does the transcript still avoid timestamps, request IDs, secrets, and PII?

When re-recording:

```bash
MOA_RECORD_TRANSCRIPT=1 cargo test -p moa-eval --test long_conversation_smoke -- --ignored
```

Then validate:

```bash
cargo test -p moa-eval --test long_conversation_smoke --locked -- --ignored
cargo run -p xtask -- check-eval-budgets --suite long_conversation
```

## Escalation

Escalate immediately when:

- any safety exposure counter is non-zero,
- a cache regression increases cost materially,
- the same scenario flakes twice,
- the event log contains duplicate sequence numbers,
- the nightly fails after a provider API release.

Include the score card, event artifact, failing metric, and first bad commit
range in the escalation note.
