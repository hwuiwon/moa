# 16 - Evaluation

_Long-conversation harness, score cards, budgets, and triage._

## Scope

`moa-eval` is a platform-only regression harness: a library, CLI, and `xtask`
surface driven by CI, nightly jobs, explicit live lanes, and the internal
skill-regression gate. It is not a tenant product. There is no hosted `Eval`
Restate service, no tenant eval MCP tool, and no public `/v1/evals/*` route.
Tenant-facing evaluation is Behavior Lab (`moa-experiments`), documented in
[Behavior Lab](product/behavior-lab.md).

Behavior Lab persists score lineage through `analytics.score_run` and
`analytics.scores`. The platform harness produces the same typed `ScoreRecord`
contract for comparison, but leaves persistence to its caller.

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
`ExecutionEvalReport` is the only eval report in this repository that claims
execution success from persisted run/task state.

Execution routing evaluation separates two boundaries. Public-route errors
compare Respond, Execute, and NeedsInput and feed weighted routing cost plus the
catastrophic Respond-on-Execute rate. Internal-strategy errors are scored only
when both expected and observed routes are Execute; they compare Inline and
Durable and feed weighted strategy cost, near-boundary Inline recall, Durable
strategy recall, and one-way-upgrade evidence metrics. A route error is never
relabelled as a strategy error.

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
execution snapshot and invariants in `ExecutionEvalReport`.

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

## Suite Controls And Leakage

A suite that passes proves nothing on its own: the same green score is produced
by a working system and by a scorer that cannot fail. Two `xtask` commands are
the evidence that the graders themselves work, and both are exit-status gates
rather than reports somebody has to read.

```bash
cargo run -p xtask --features eval-tools -- eval-suite-controls [--out <path>]
cargo run -p xtask --features eval-tools -- eval-control-mutants \
  [--out-dir <dir>] [--minimum-score <0..1>] [--list]
```

`eval-suite-controls` runs every control that does not need a live database and
writes one suite-validity report (default
`target/eval-controls/suite-controls.json`). Each suite declares both sides of
the pair:

- a **negative/null control** that should score at chance — a
  query-independent answer, a permuted question, a majority-class predictor
  derived from an authoring split. Its ceiling is *derived* from repeated seeds
  rather than guessed, and a null scoring above its ceiling means the metric is
  measuring something other than the capability it names.
- a **positive/oracle control** that should score near the top — known
  relevant fact IDs, an exact expected UID set, a manifest-provided expected
  route. An
  oracle below its floor means the scorer cannot recognize a correct answer, so
  every passing score above it is unearned.

The command fails on an incomplete registry (a suite declaring only one side of
the pair), when the registry and report control sets differ, when an executed
control has missing or empty slice evidence, on authoring defects in a checked-in
corpus, on a null above its ceiling, and on an oracle below its floor. Every
non-database control runs in this command. Database-lane controls run in the
`db-memory` lane and are the only entries reported as
`skipped_requires_database`, so a missing control cannot look like an intentional
skip. The same report carries one typed package-leakage outcome for every real
corpus this command owns: generated memory transcripts, the checked-in golden
source fixtures, and checked-in external-memory turns. A missing, duplicate, or
vacuous command-owned outcome fails the command. Its deterministic fixed-RAG
workload is explicitly tagged `scanner_fixture`; it tests scanner/control logic
but does not claim WixQA corpus coverage. The report declares this boundary as
`package_leakage_scope=command_owned_corpora_plus_labeled_scanner_fixtures`.
When a suite is invalid the report's `headline_score` is `null` —
deliberately not a null-corrected number, because a broken scorer's arithmetic is
not worth correcting.

`eval-control-mutants` runs a narrow `cargo-mutants` slice over the decision
surface itself: null-ceiling derivation, the validity audit, the fixed-corpus
leakage scanner, and anchor-cohort pairing (`.cargo/mutants-eval-controls.toml`
holds the exact target list so the surface can be audited before the expensive
run). It writes `selected-mutants.txt`, `outcomes.json`, and
`mutation-report.json`, and fails below `--minimum-score` (default 0.90).
A surviving mutant is a scorer edit no test noticed, recorded by name instead of
remembered. `--list` enumerates the selected mutants without running them.
`cargo-mutants` must be installed (`cargo install --locked cargo-mutants`).

The shared `LeakageScanner` is the contamination boundary: a fixed corpus is only
a valid measurement while its cases are absent from what the system under test
can read. `eval-suite-controls` runs it over each required lane's real fixture or
generated source text, exact SHA-256, provenance, and case split. It does not
claim cross-command fulfillment. The live `wixqa-rag-eval` command owns WixQA's
required outcome and scans the selected article bytes, URLs,
deterministic corpus revision, and selected questions before it loads runtime
configuration, connects to Postgres, or calls a provider. Both paths fail closed;
an absent or blank source URL/revision is treated as contaminated rather than
clean. Generated memory probes are split by semantic template family, keeping
the deliberately repeated seed/tenant/user variants on one side of the
leakage-analysis authoring boundary. The memory null controls remain explicitly
all-probe, per-probe-type diagnostics; they are not labeled as a held-out
authoring/validation experiment while several probe types have only one
independent query-template family.
`run-external-memory-eval` also scans the prepared cases after every package
loader branch, including the direct PersonaMem and LongMemEval loaders, before
live authorization, runtime construction, or scoring.
`eval-control-mutants` mutation-checks this scanner and its surrounding decision
surface. Anchor-cohort pairing is in the same slice because a comparison across
two different frozen case sets invents a delta; `moa_eval::kernel::cohorts`
refuses that, and `kernel::compare` refuses two reports whose declared anchor
manifests differ.

Run both after any change to a scorer, a control, a corpus manifest, or a gate
threshold. A change to the graders is exactly the change a suite's own green
result cannot detect.

## Model Judges

Most of this harness does not use a model judge, and that is a design choice
rather than a gap. Memory retrieval is scored deterministically: factual
support, temporal as-of, abstention, and redaction probes have exact
authorities, and the
pairwise LLM judge in `moa_eval::memory_eval::judge` is restricted to the two
open-ended probe types and is not wired into `run-memory-retrieval-eval`. Judges
are not added so that calibration machinery becomes applicable.

Where a judge does decide something, it is treated as a measuring instrument
that has to be calibrated before its output can be quoted. The contract lives in
`moa_eval::external_memory::calibration::judge`:

- **Identity and expiry.** A calibration is bound to one `JudgeIdentity`: exact
  model, prompt version *and* prompt-text hash, rubric hash, output-parser
  version, and domain, reduced to one fingerprint. It expires when any of those
  changes or when positive-class prevalence moves materially — a judge
  calibrated at 20% positives is answering a different question at 60%.
- **Three separate claims.** Human-human reliability (how well two blinded
  labelers agreed), judge-versus-adjudicated-gold validity (whether the judge is
  right, measured on an untouched validation split), and aggregate bias
  correction are distinct reports. Quoting a high human kappa as evidence the
  judge works is the usual error, and the types make it impossible.
- **What gets reported.** Confusion matrix, class prevalence, raw agreement,
  kappa *with its interval*, class-specific precision/recall/sensitivity/
  specificity, worst-stratum weakest-class recall, and selective
  accuracy/coverage wherever abstention exists. Prompt-injection, position-swap,
  rare-class, and cross-domain slices must all carry their own held-out validity
  measurement or authority is refused; caller-asserted slice labels are not
  evidence.
- **Per-task thresholds, not one number.** There is no shared `kappa >= 0.80`
  point threshold. Each task declares a `JudgeAuthorityRequirement`, because the
  same number is unmeetable on a ten-case stratum (a flawless three-for-three
  recall has a Wilson lower bound of 0.44) and trivial on a thousand-case suite.
  Kappa is required of the interval's lower bound; small-stratum recall is
  required of the point estimate with a much weaker interval bar, and the
  reasoning is recorded next to the numbers.
- **Uncertainty propagates from both sides.** `correct_aggregate_rate` applies
  the Rogan-Gladen correction and takes its interval over the calibration set's
  uncertainty about sensitivity and specificity as well as the evaluation set's
  uncertainty about the apparent rate. It refuses outright when the judge's
  interval admits chance performance, where the corrected value is unbounded
  rather than merely wide.
- **Uncalibrated means inconclusive.** `apply_judge_authority` downgrades a
  judge-derived metric decision to `INCONCLUSIVE` when calibration is missing,
  stale, or measured on the selection split. It does not report a weaker pass.
  External-memory runner reports are always emitted informational. There is no
  promotion helper until a reviewed workflow can bind the byte-pinned results,
  exact judge identity, held-out case set, and per-slice measurements in one
  durable artifact.
- **Live calibration is off by default.** `admit_live_calibration` refuses the
  default request. A live run needs an explicit flag, resolved credentials, and
  a positive authorized budget, all three.

Prompt shape is decided the same way: `decide_prompt_shape` refuses to choose on
the split that selected the prompts, and picks per-dimension decomposition over
a single structured call only when the decomposed accuracy interval clears the
holistic point estimate on held-out cases.

## Triage

Treat failures as regressions until proven otherwise. Read the budget output
first, then inspect artifacts.

| Failure | Start with | Common causes |
|---|---|---|
| Null control above its ceiling | Suite controls report | metric measuring a query-independent shortcut, corpus authoring defect, ceiling derived from too few seeds |
| Oracle control below its floor | Suite controls report | scorer cannot recognize a correct answer, corpus/manifest drift, evaluator ID or version mismatch |
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
