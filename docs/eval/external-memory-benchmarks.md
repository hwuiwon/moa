# External Memory Benchmarks

This lane measures MOA's production memory formation, retrieval, and answer
path against pinned PersonaMem 32k and LongMemEval-S Cleaned packages. It is a
manual, billed evidence lane. It does not replace the hermetic retrieval gate,
and its outputs are informational until the authority checks below are
completed in a separate reviewed change.

## Manual protected lane

The dedicated GitHub workflow (`memory-benchmarks.yml`) has been removed; the
lane now runs manually via the xtask commands below, from a `main` checkout.
The controls that workflow enforced remain the contract for this lane — and
for any reinstated CI workflow: `workflow_dispatch`-only from
`refs/heads/main`, `contents: read`, a protected `memory-benchmarks`
environment with deployment branches restricted to `main`, required reviewers,
prevent-self-review, and only the minimum benchmark secrets described below.

The lane supports only `formation_mode=live`; recorded formation is a local
workflow because there is no recorded-manifest artifact input here.

The lane runs against the repository's
`moa-postgres:pg17-pgvector0.8.2-pgaudit` image with database `moa`,
password `ci`, and a `pg_isready` health check. It exports:

```text
MOA_DATABASE_URL=postgres://postgres:ci@localhost:5432/moa
```

Every benchmark command passes `--migrate-database`. The command runs
`moa_migrations::run` before backend or provider construction and fails closed
if migration fails.

### Inputs and secret boundary

The lane requires the dataset, live formation mode, extractor model, merge
verifier model, embedding selector, reader model, LongMemEval judge model,
reader context window, reader output-token reserve, controls, evidence budget,
and cumulative USD ceiling. Evidence budget is exactly `512`, `1024`, or
`2048`; controls are exactly
`no-memory,full-context,oracle-evidence`; reader limits and the USD ceiling must
be positive.

The currently protected family matrix is OpenAI extraction/merge/reader,
Google or Gemini embedding, and Anthropic LongMemEval judging. This matches the
secret surface instead of silently selecting a provider for which no explicit
credential was approved.

Package download and paid execution are independently authorized:

- fetch steps receive only `MOA_RUN_NETWORK_MEMORY_BENCHMARKS=1`;
- PersonaMem execution receives `MOA_RUN_LIVE_MEMORY_BENCHMARKS=1`,
  `MOA_OPENAI_API_KEY`, and `MOA_GOOGLE_API_KEY`;
- LongMemEval execution additionally receives `MOA_ANTHROPIC_API_KEY`.

Anthropic credentials are not present in the PersonaMem step. Credentials are
never written to fixtures, reports, commands, or artifacts.

The fetcher writes a strict `package.json` and `VerifiedFetchSummary`. The run
command requires `--fetch-summary` for both external datasets and verifies the
dataset, repository, revision, package hash, and release counts against the
loaded package before migration or provider construction. Network authorization
does not imply billed-run authorization.

Fetch and run share the same strict summary enum. Its PersonaMem variant has
exact fields
`{schema_version,dataset,repository,revision,package_sha256,question_count,persona_count,context_count,verified}`;
its LongMemEval variant has exact fields
`{schema_version,dataset,repository,revision,package_sha256,question_count,abstention_count,retrieval_count,verified}`.
Both require schema version 1, the pinned package identity/counts, and
`verified:true`; unknown fields or a summary/package mismatch fail closed.

The lane produces package manifests, verified fetch summaries, and reports
for review. It has no repository write permission and no baseline write,
commit, or push step.

## Report V2 contract

External runs produce the hard-break `ExternalMemoryReportV2`. Readers reject
unknown fields. Serialization and all mode/case ordering are deterministic.
The top-level wire is exactly:

```text
{
  schema_version: 2,
  generated_at,
  dataset_package,
  formation,
  formation_hash,
  reader_contract,
  budget,
  stage_metrics,
  modes,
  authority
}
```

`reader_contract` is:

```text
{
  model,
  prompt_version,
  context_window: u64,
  output_token_reserve: u64,
  token_estimator: "chars_div_4_v1"
}
```

`budget` is:

```text
{
  ceiling_usd,
  estimated_committed_usd,
  actual_or_estimated_committed_usd
}
```

`modes` is ordered `primary`, `no_memory`, `full_context`,
`oracle_evidence`. Every `ModeReportV2` has exactly:

```text
{ mode, cases, denominators, category_slices, dataset_metrics }
```

Every `CaseReportV2` has exactly:

```text
{
  isolation_key,
  category,
  mode,
  mode_support,
  rendered_evidence,
  rendered_evidence_tokens,
  reader,
  answer_score_support,
  answer_score,
  absolute_judge,
  failure
}
```

`ModeDenominatorsV2` is exactly:

```text
{
  total_cases,
  completed_cases,
  failed_cases,
  unsupported_cases,
  reader_attempts,
  judge_attempts
}
```

The invariant is
`total_cases = completed_cases + failed_cases + unsupported_cases`. Unsupported
modes make no provider call. A terminal budget exhaustion performs no later
backend or paid work, but emits budget failures for every remaining supported
`(mode, case)` and still records every precomputable unsupported case.

`StageCostRecord` and `StageObservation` carry mode
`primary|no_memory|full_context|oracle_evidence|null`. Formation and embedding
use `null`; retrieval, reader, and judge use their actual mode. Stage metrics
are an ordered vector keyed by `(stage, mode)`, not a JSON object with a
stringified composite key. All four modes consume one cumulative ledger.

Formation configuration must be fully resolved. Reports retain formation
implementation/model/prompt/version/hash plus stage usage, latency, and cost.
The reader model, provider, prompt, temperature, and output reserve are the
same across modes.

## Evidence modes and fit

Primary uses production retrieval and its exact token-budgeted rendered
evidence. No-memory supplies an empty string and zero evidence tokens.

Full-context is never truncated. Its exact evidence is the header:

```text
FULL_CONTEXT_V1
```

followed immediately by compact JSON with this wire:

```text
{
  "schema_version": 1,
  "mode": "full_context",
  "sessions": [
    {
      "source_id": "...",
      "occurred_at": "...",
      "turns": [
        {
          "source_id": "...",
          "occurred_at": "...",
          "role": "...",
          "text": "..."
        }
      ]
    }
  ]
}
```

Sessions and turns retain validated dataset order. Oracle evidence uses the
same envelope with `mode:"oracle_evidence"`, includes only independently
labeled gold turns grouped in source order, and never infers evidence from
answer text or session labels. It is supported only for LongMemEval. PersonaMem
oracle is unsupported for every case with reason
`oracle-evidence-requires-longmemeval-labels`; missing or invalid LongMemEval
turn references are rejected rather than guessed.

Fit is computed from the exact provider request text emitted by the shared
reader-prompt renderer: system instructions, prompt version, question, ordered
options, and the exact mode evidence. `chars_div_4_v1` counts Unicode scalar
values and estimates `ceil(count / 4)`. A mode is supported only when:

```text
estimated_input_tokens + reader_output_token_reserve <= reader_context_window
```

Overflow is unsupported with reason `reader-context-limit`; it is never
truncated or recorded as a provider failure. The same fit check applies to
primary and no-memory.

PersonaMem retains label-only accuracy and clustered slices per mode, while
retrieval recall is explicitly unsupported. LongMemEval retains the
500-answer, 30-abstention, and six official type denominators in every mode.
Only primary has the 470-case retrieval metrics; controls carry
`retrieval-metrics-apply-to-primary-only`.

Every supported LongMemEval mode uses the same category-specific absolute
judge contract. Every supported PersonaMem mode uses its deterministic label
scorer and makes no judge call.

## Authority

The report-level authority object separates retrieval from answer authority:

```text
{
  retrieval: {
    authoritative: false,
    reason,
    calibration_manifest_sha256: null,
    calibration_results_sha256: null
  },
  answer: {
    authoritative: false,
    reason,
    calibration_manifest_sha256: null,
    calibration_results_sha256: null
  }
}
```

The lane and runner cannot set either value to true or attach calibration
links. Results cannot enable a projection, ranking change, or production
default automatically.

## LongMemEval judge calibration

Calibration is a later human-coordination step; Task 11 validation makes no
provider call and requires no human label. All calibration JSON is schema
version 1, rejects unknown fields, and uses lowercase 64-hex hashes.

### Deterministic sample

Take ten non-`_abs` cases from each ordered stratum:

1. `knowledge-update`
2. `multi-session`
3. `single-session-assistant`
4. `single-session-preference`
5. `single-session-user`
6. `temporal-reasoning`
7. ten `_abs` cases in `abstention`

Within each stratum sort by lowercase
`SHA256(b"moa-longmemeval-calibration-v1\0" || UTF8(question_id))`, then by
question ID as the collision tie-breaker. Concatenate strata in the order
above. The resulting sample has exactly 70 unique items.

Two independent blinded labelers assign exact strings `correct|incorrect`,
mapped to `1|0`, then a third person adjudicates. Labeler A, labeler B, and
adjudicator identity hashes are required and pairwise distinct. The identity
hash is:

```text
SHA256(b"moa.external-memory.calibration.identity.v1\0" || UTF8(NFC(trim(identity))))
```

Raw identities are never stored. Missing items, duplicates, changed item
content/order, incomplete labels, or repeated identities invalidate the run.
Labeler templates contain no judge output.

### Exact wires and hashes

`CalibrationManifest` is exactly:

```text
{
  schema_version,
  dataset: "longmemeval-s-cleaned",
  dataset_revision,
  package_sha256,
  report_sha256,
  selection_seed: "moa-longmemeval-calibration-v1",
  sample: [{ question_id, stratum }],
  manifest_sha256
}
```

A label artifact is exactly:

```text
{
  schema_version,
  manifest_sha256,
  role: "labeler_a"|"labeler_b",
  status: "template"|"completed",
  identity_sha256: string|null,
  items: [{
    question_id,
    stratum,
    question,
    reference_answer,
    candidate_answer: string|null,
    reader_failure_kind: string|null,
    label: "correct"|"incorrect"|null
  }]
}
```

`prepare` emits null identity/labels and no judge output. `score` requires
completed status, exact sample/content equality, all 70 labels, and non-null
distinct identity hashes.

`CalibrationAdjudication` is exactly:

```text
{
  schema_version,
  manifest_sha256,
  role: "adjudicator",
  identity_sha256,
  labels: [{ question_id, label: "correct"|"incorrect" }]
}
```

Labels remain in manifest order. `CalibrationResults` is exactly:

```text
{
  schema_version,
  manifest_sha256,
  report_sha256,
  labeler_a_sha256,
  labeler_b_sha256,
  adjudication_sha256,
  n00,
  n01,
  n10,
  n11,
  pair_denominator: 70,
  agreement,
  kappa_status: "defined"|"undefined_zero_denominator",
  kappa: number|null,
  judge_correct_count,
  judge_denominator: 70,
  judge_accuracy,
  agreement_pass,
  kappa_pass,
  accuracy_pass,
  verdict: "pass"|"fail",
  results_sha256
}
```

`nXY` means labeler A assigned `X` and labeler B assigned `Y`. Manifest and
results self-hashes use the benchmark calibration wire's explicit
`canonical_json(value_without_its_own_hash_field)`; only the artifact's own
self-hash field is excluded:

```text
SHA256(b"moa.external-memory.calibration.manifest.v1\0" || canonical_json)
SHA256(b"moa.external-memory.calibration.results.v1\0" || canonical_json)
```

The canonical encoder recursively sorts object keys by Unicode scalar-value
order, preserves array order, emits compact JSON with `serde_json` string
escaping, and renders every finite number using `serde_json::Number`'s
deterministic shortest representation. Non-finite values are rejected before
hashing. This explicit numeric branch is required because the workspace
`serde_canonical_json` formatter rejects floating-point numbers while the
results wire intentionally contains numeric agreement, kappa, and accuracy.

All other fields remain included. Package, report, labeler, and adjudication
hashes are SHA-256 of exact file bytes, avoiding circular hashes. `score`
requires an explicit report, verifies its exact bytes against the manifest,
and reads only the primary-mode LongMemEval judge outcome for every sample.

### Decision math

Over exactly 70 pre-adjudication pairs:

```text
p_o  = (n00 + n11) / 70
p_A1 = (n10 + n11) / 70
p_B1 = (n01 + n11) / 70
p_e  = p_A1*p_B1 + (1-p_A1)*(1-p_B1)
kappa = (p_o-p_e) / (1-p_e)
```

If `1-p_e` is zero, calibration is failed with
`kappa_status=undefined_zero_denominator`, `kappa=null`; it is never coerced to
zero or NaN. Judge accuracy is exact agreement with all 70 adjudicated labels.
A missing case, reader/judge failure, timeout, parse failure, budget failure,
or missing primary-mode judge artifact counts as judge-incorrect.

The verdict passes only when all three conditions hold:

```text
agreement >= 0.90 && kappa >= 0.80 && judge_accuracy >= 0.85
```

## Maintainer protocol

The following operations require separate approval for public downloads,
provider spend, or human coordination. They are not part of hermetic plan
completion.

Fetch both pinned packages:

```bash
MOA_RUN_NETWORK_MEMORY_BENCHMARKS=1 cargo run -p xtask --quiet --features eval-tools -- fetch-memory-benchmark \
  --dataset personamem-32k \
  --revision 73dfd752d477d0c466cd441f1669397f5726d7ab \
  --output target/memory-benchmarks/personamem-32k \
  --summary-output target/memory-benchmarks/personamem-32k-fetch-summary.json

MOA_RUN_NETWORK_MEMORY_BENCHMARKS=1 cargo run -p xtask --quiet --features eval-tools -- fetch-memory-benchmark \
  --dataset longmemeval-s-cleaned \
  --revision 98d7416c24c778c2fee6e6f3006e7a073259d48f \
  --output target/memory-benchmarks/longmemeval-s-cleaned \
  --summary-output target/memory-benchmarks/longmemeval-s-cleaned-fetch-summary.json
```

PersonaMem must verify `589 questions / 20 personas / 37 contexts`;
LongMemEval must verify `500 total / 30 abstention / 470 retrieval`.

Run the two live evaluations against a migrated database:

```bash
export MOA_DATABASE_URL=postgres://postgres:ci@localhost:5432/moa
export MOA_OPENAI_API_KEY='<approved OpenAI credential>'
export MOA_GOOGLE_API_KEY='<approved Google credential>'
```

Do not copy real credential values into a report, fixture, shell transcript, or
review comment.

```bash
MOA_RUN_LIVE_MEMORY_BENCHMARKS=1 cargo run -p xtask --features eval-tools -- run-external-memory-eval \
  --dataset personamem-32k \
  --data target/memory-benchmarks/personamem-32k \
  --package-manifest target/memory-benchmarks/personamem-32k/package.json \
  --fetch-summary target/memory-benchmarks/personamem-32k-fetch-summary.json \
  --migrate-database \
  --formation-mode live \
  --extractor-model openai:gpt-5.4-mini \
  --merge-verifier-model openai:gpt-5.4-mini \
  --embedding-selector gemini:gemini-embedding-2 \
  --reader-model openai:gpt-5.4-mini \
  --reader-context-window 400000 \
  --reader-output-token-reserve 4096 \
  --controls no-memory,full-context,oracle-evidence \
  --evidence-token-budget 1024 \
  --budget-usd 25 \
  --output target/memory-benchmarks/personamem-32k-report.json

export MOA_ANTHROPIC_API_KEY='<approved Anthropic credential>'
MOA_RUN_LIVE_MEMORY_BENCHMARKS=1 cargo run -p xtask --features eval-tools -- run-external-memory-eval \
  --dataset longmemeval-s-cleaned \
  --data target/memory-benchmarks/longmemeval-s-cleaned \
  --package-manifest target/memory-benchmarks/longmemeval-s-cleaned/package.json \
  --fetch-summary target/memory-benchmarks/longmemeval-s-cleaned-fetch-summary.json \
  --migrate-database \
  --formation-mode live \
  --extractor-model openai:gpt-5.4-mini \
  --merge-verifier-model openai:gpt-5.4-mini \
  --embedding-selector gemini:gemini-embedding-2 \
  --reader-model openai:gpt-5.4-mini \
  --judge-model anthropic:claude-sonnet-4-6 \
  --reader-context-window 400000 \
  --reader-output-token-reserve 4096 \
  --controls no-memory,full-context,oracle-evidence \
  --evidence-token-budget 1024 \
  --budget-usd 50 \
  --output target/memory-benchmarks/longmemeval-s-cleaned-report.json
unset MOA_ANTHROPIC_API_KEY
```

Prepare and score the blinded calibration:

```bash
cargo run -p xtask --features eval-tools -- calibrate-external-memory-judge prepare \
  --dataset target/memory-benchmarks/longmemeval-s-cleaned \
  --report target/memory-benchmarks/longmemeval-s-cleaned-report.json \
  --output-manifest target/memory-benchmarks/calibration/manifest.json \
  --labeler-a-template target/memory-benchmarks/calibration/labeler-a.json \
  --labeler-b-template target/memory-benchmarks/calibration/labeler-b.json

# After two independent blinded labels and adjudication:
cargo run -p xtask --features eval-tools -- calibrate-external-memory-judge score \
  --manifest target/memory-benchmarks/calibration/manifest.json \
  --report target/memory-benchmarks/longmemeval-s-cleaned-report.json \
  --labeler-a target/memory-benchmarks/calibration/labeler-a.json \
  --labeler-b target/memory-benchmarks/calibration/labeler-b.json \
  --adjudication target/memory-benchmarks/calibration/adjudication.json \
  --output target/memory-benchmarks/calibration/results.json
```

## Promotion checklist

Retrieval and answer authority are separate reviewed decisions. The lane
never promotes either.

Retrieval promotion requires:

- exact fetch-summary, package, formation, and report hashes;
- primary retrieval metrics with complete dataset denominators;
- every failure retained and reviewed; and
- all control artifacts present with the same reader contract.

Answer promotion additionally requires:

- the pinned reader/judge models and prompt/rubric versions;
- a calibration manifest and results file linked by exact hashes;
- all 70 adjudicated labels and all judge failures counted incorrect; and
- passing agreement, kappa, and accuracy thresholds conjunctively.

Future reviewed baselines may live at:

```text
docs/eval/baselines/personamem-32k-moa-baseline.json
docs/eval/baselines/longmemeval-s-cleaned-moa-baseline.json
```

Promotion is a separate human-reviewed repository change. A benchmark workflow
run never writes these paths.
