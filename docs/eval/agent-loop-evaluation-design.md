# Evaluation and Release Control for Agent-Loop Systems

**Status:** Proposed
**Date:** 2026-07-29
**Audience:** Agent-runtime, evaluation, security, data, and release-platform
engineers

## Purpose

This document defines a general architecture for evaluating systems built around an
open-ended agent loop:

```text
receive input
  -> compile context
  -> call a model
  -> propose an action or response
  -> apply policy
  -> execute a tool
  -> persist the observation
  -> decide whether to continue
```

The design covers two related but distinct jobs:

1. platform regression evaluation for changes to the runtime, model routing,
   retrieval, tools, memory, or orchestration; and
2. externally triggered release evaluation for changes to an agent, prompt, skill,
   policy, or enabled capability.

Both jobs use the same assertion, evidence, metric, reliability, and resource
contracts. They do not need to use the same runner, database, authorization surface,
or workflow engine.

The central correctness rule is:

> Evaluation must exercise the real agent loop under an isolated, pinned environment,
> then bind its decision to the exact candidate and every dependency that could have
> changed the observed behavior.

## Goals

- Evaluate the production agent-loop path rather than a parallel mock implementation.
- Prevent an unevaluated or differently evaluated revision from becoming active.
- Distinguish runtime completion, conversation termination, and task success.
- Make tool effects and final state objectively assertable.
- Bound cost, time, turns, calls, and side effects before and during execution.
- Produce paired, statistically defensible candidate-versus-baseline decisions.
- Return `INCONCLUSIVE` when the evidence cannot support either success or regression.
- Make simulator and model-judge limitations explicit and measurable.
- Preserve tenant isolation, privacy, replay safety, and evidence lineage.
- Keep common release gates fast through staging, caching, pairing, and coalescing.

## Non-Goals

- Replacing production monitoring, canary rollout, or rollback systems.
- Proving that a simulated user is equivalent to a human in every domain.
- Treating model output, a trace, or a transcript as authoritative environment state.
- Letting evaluation plans execute arbitrary assertion code.
- Automatically activating a candidate because one scorecard is green.
- Using one universal sample size, confidence method, judge threshold, or simulator
  error threshold for every metric and domain.

## System Boundary

### Shared contracts, separate runners

```text
                        shared pure contracts
                assertions / evidence / metrics / verdicts
                                  |
                +-----------------+-----------------+
                |                                   |
                v                                   v
     platform regression runner          release-evaluation runner
     CI / nightly / explicit live        durable, scoped, user-triggered
     hermetic repo-owned cases            isolated customer-owned candidates
                |                                   |
                v                                   v
     platform release decision          activation attestation
```

The platform runner may be a CLI, test harness, or scheduled job. The release runner
is usually a durable workflow because it needs idempotency, cancellation, retries,
status, and exact activation evidence.

The shared layer contains no provider clients, tenant stores, workflow state, or
release APIs. It defines pure data contracts and statistical operations.

### Runtime resource contracts

Resource envelopes, reservations, deadlines, and cancellation tokens belong to the
runtime domain, not the evaluation domain. Session, execution, provider, tool, and
sandbox code must not depend on an evaluation package.

Evaluation-specific policies translate a plan into a runtime resource envelope and
adapt reconciled usage into score and audit records.

## Agent-Loop Integration

### Production loop

A conforming agent loop exposes durable boundaries around each externally observable
step:

```text
InputAdmission
  -> ContextCompiled
  -> ModelCallReserved
  -> ModelDecisionObserved
  -> ActionProposed
  -> ActionPolicyDecided
  -> ToolCallReserved
  -> ToolResultObserved
  -> LoopStatePersisted
  -> CompletionEvaluated
```

The names are illustrative. The required property is that each boundary has structured
identity and evidence. An evaluator should not infer a tool decision from prose when
the runtime already knows the tool name, arguments, policy outcome, and result.

### Evaluation controller

The evaluation controller sits outside the agent loop:

```text
TrialController
  -> creates isolated target session/run
  -> delivers the next user or simulator turn through normal ingress
  -> waits for one durable target outcome
  -> reads bounded structured evidence
  -> advances the scenario or terminates the trial
```

It must not:

- inject hidden control instructions into the target's ordinary user transcript;
- call target tools on the target's behalf;
- treat simulator output as a security authority;
- read a production session to save setup cost; or
- bypass normal action policy, usage accounting, persistence, or cancellation.

### Supported target kinds

The design supports two target adapters:

| Target | Description |
|---|---|
| Conversational agent loop | Repeated user/model/tool turns in a session |
| Durable execution | A detached plan/run that reaches typed terminal state |

Both adapters implement equivalent control semantics:

```text
start(isolated_snapshot, pinned_dependencies, resource_envelope)
deliver(input)
status()
request_cancel(reason)
collect_evidence()
```

The adapter is responsible for using the production path. The shared evaluator is not
responsible for knowing how a session or durable run is stored.

### Approvals and asynchronous input

An agent loop may request administrator approval, user clarification, or an external
signal and then continue later. Evaluation drives these through the same typed product
boundary as production:

```text
action requires review
  -> target persists review request
  -> trial observes pending review
  -> scripted scenario supplies approve / deny / timeout
  -> target receives one typed receipt
  -> target continues or terminates
```

The user simulator cannot fabricate administrator approval in conversation text. The
environment/controller owns review decisions, and the evidence records request,
decision, cleared action, actor, and ordering.

A pending review or input request is not automatically success or failure. The
scenario defines whether to supply the input, deny it, let it time out, or assert that
the target should not have requested it. Trial deadlines and resource accounting
continue to apply while the target is parked.

### Delegation and detached work

Worker agents, background tasks, and detached durable runs remain inside the trial's
security and resource tree:

- children inherit owner scope, capability policy, subject digest, and a bounded
  sub-envelope;
- child calls reserve from the shared trial/run ledger;
- evidence carries actor, logical task, and generation identity;
- cancellation fences new child work and propagates through the tree;
- a stale child result cannot update a newer generation; and
- terminal trial evaluation includes unfinished, failed, or abandoned children when
  the scenario contract requires them.

The evaluator does not treat “root response returned” as completion while required
detached work is still running. Conversely, optional background work must be declared
so the trial does not wait indefinitely for irrelevant children.

## Trust and Threat Model

The system assumes:

- candidate authors may unintentionally or deliberately choose favorable cases;
- model, prompt, retrieval, tool schema, and simulator changes can invalidate old
  results;
- retries and workflow replay are normal;
- a target can influence a simulator through conversation text;
- model output and tool output may contain prompt injection;
- evaluation inputs may contain private customer data;
- parallel trials can race shared cost and concurrency limits;
- callers may attempt cross-tenant identifiers or oversized matrices;
- repeated access to a visible test set causes adaptive overfitting.

The system does not assume:

- simulator cooperation is independent of target behavior;
- a temperature or seed makes a remote provider deterministic;
- a sandbox network policy contains host-side tools or connectors;
- a schema annotation enforces an argument limit at runtime; or
- a successful process exit means the task was completed correctly.

## Core Invariants

1. Normal sessions resolve only active serving pointers.
2. A release candidate is immutable after its subject digest is created.
3. Activation consumes one unexpired, unconsumed attestation for the exact subject.
4. A stale, superseded, wrong-scope, or differently pinned result cannot activate.
5. Evaluation-owned sessions and environments cannot access production credentials or
   production side-effecting capabilities.
6. Authorization and ownership checks happen before the first protected read.
7. Every paid or side-effecting call reserves resources before dispatch.
8. Deterministic safety or state failure cannot be overridden by a stochastic score.
9. Missing, duplicate, unversioned, or provenance-mismatched blocking evidence fails
   closed.
10. Runtime completion, simulator termination, and scenario outcome are recorded
    separately.
11. Candidate and baseline comparisons use exact pairing and common randomness.
12. Insufficient independent support produces `INCONCLUSIVE`, never a green result.
13. A replay cannot duplicate a trial, external effect, score, attestation, or
    activation.
14. Evaluation never activates a candidate automatically.

## Release Lifecycle

### State machine

```text
Draft
  -> Candidate
  -> Evaluating
       -> Ready
       -> Rejected
       -> Inconclusive
       -> Superseded
  -> Active
  -> Archived
```

`Ready` is non-serving. It means the current release attempt produced a valid
attestation. An explicit authorized activation request still has to consume it.

`Inconclusive` is a terminal attempt outcome and a retryable, non-serving candidate
state. It releases the active evaluation slot and permits the newest pending candidate
to run.

`Superseded` is terminal. Its eventual workflow result may be persisted for audit but
cannot create an attestation.

### Serving pointers

Activation gates the actual serving mutation, not a generic “publish” event. Examples
include:

- the active revision of an instruction or skill;
- the deployed revision of an agent;
- the active prompt/policy bundle;
- the enabled tool-policy revision; and
- the active connector/catalog snapshot.

An artifact may be visible for review without being resolvable by ordinary agent
sessions.

### Release policy

The release policy is resolved server-side and authorized independently from candidate
submission. Candidate authors may add authoring cases, but cannot remove mandatory
safety assertions, choose an empty gate, select an obsolete calibration, or weaken the
hidden release cohort.

A release policy pins:

- required plan/scenario packs;
- mandatory platform safety assertions;
- primary metrics and practical margins;
- authority rules for evaluators;
- simulator policy and certification requirements;
- resource and capability policies;
- hidden-cohort policy;
- required tool/catalog snapshots; and
- attestation lifetime.

### Exact evaluation subject

An `EvaluationSubject` is canonicalized and hashed before work begins:

```text
EvaluationSubject {
    owner_scope
    activation_target
    candidate_revision_hash
    serving_baseline_hash
    resolved_dependency_lock_hash
    agent_prompt_and_policy_hash
    model_and_provider_policy_hash
    tool_policy_hash
    tool_catalog_schema_hash
    environment_fixture_hash
    evaluation_plan_hash
    scenario_and_dataset_hash
    simulator_policy_hash
    evaluator_registry_hash
    release_policy_hash
    resource_policy_hash
}
```

Changing any field invalidates prior results.

“Current revision,” “latest run,” or “recent score” is not an adequate subject
identity.

### Attestation

An activation attestation contains:

```text
ActivationAttestation {
    attestation_id
    owner_scope
    subject_digest
    release_attempt_id
    run_and_trial_ids
    evidence_root_hash
    evaluator_and_metric_versions
    decision
    created_at
    expires_at
    consumed_at
    decision_provenance
}
```

Only `PASS` can produce an activation attestation. `REGRESSION` and `INCONCLUSIVE`
remain reviewable attempt outcomes.

The activation transaction recomputes the subject, checks scope and policy, verifies
the expected serving pointer, consumes the attestation, records an audit decision, and
moves the serving pointer with one compare-and-swap.

### Change-triggered evaluation

Candidate submission and run dispatch use a transactional outbox or an equivalent
durable message boundary.

Rapid changes are coalesced as:

```text
one active candidate + one pending newest candidate
```

Every result is generation-fenced. When the active attempt terminates, the controller
dispatches the pending newest subject. Intermediate candidates may be preserved as
superseded records without consuming provider budget.

## Scenario and Environment Model

### Scenario definition

A scenario defines:

```text
Scenario {
    initial_world_state
    user_goal
    target_visible_state
    simulator_visible_state
    hidden_oracle_state
    allowed_user_intents
    allowed_target_capabilities
    resource_envelope
    stop_policy
    assertions
}
```

Visible and hidden facts are typed partitions of one world state. They are not
overlapping prose fields spread across personas, profiles, and prompts.

### Isolated environment

Every trial receives a copy-on-write environment namespace:

- deterministic initial state;
- fixture-only credentials;
- typed state transitions;
- idempotent tool effects;
- reset or disposal at trial end;
- final-state query support; and
- no implicit access to production connectors.

The agent sees normal tool descriptors and uses the production tool-policy path. A
run-origin capability policy denies every production or side-effecting capability by
default and allows only the scenario's fixture capabilities.

Generated-code sandboxes use network deny by default. Any approved external read goes
through a run-scoped broker with explicit provenance and budget; it is not enabled by
placing reusable credentials in the sandbox.

### Tool and connector snapshots

A tool-bearing subject pins the exact tool descriptor and result-schema snapshot.

Catalog changes follow:

```text
discover candidate snapshot
  -> structural protocol/schema/policy validation
  -> deterministic fixture invocation
  -> optional model-behavior probe
  -> activate or quarantine
```

Structural and fixture failures may block. A model tool-selection probe is stochastic
and diagnostic unless a separately calibrated policy explicitly grants it authority.
Replica-local refresh never runs an expensive model probe independently.

The last-known-good snapshot remains active until the candidate snapshot passes its
required deterministic contracts.

## Simulated Users

### Structured protocol

The simulator returns a typed decision:

```text
SimulatorDecision =
    Continue { user_message }
  | GoalSatisfied { final_message? }
  | TransferRequested { reason }
  | OutOfScope { reason }
  | ScenarioInvalid { reason }
```

The trial controller maps this to a stop cause and scenario outcome. It does not parse
magic strings such as `DONE` or `###STOP###`.

The target never sees hidden oracle state. The simulator receives only the state
partition required to act as the user.

### Dual control

Some tasks require a user to take an action, such as restarting a device or confirming
an account change. The simulator must not receive general target tools.

Instead:

1. the target emits a structured request for a user action;
2. a mediator validates that the request is permitted by the scenario;
3. the simulator decides whether to comply;
4. the mediator applies one typed fixture transition; and
5. the resulting human-readable observation is delivered through normal ingress.

The mediator, not the simulator model, owns state mutation and authorization.

### Simulator fidelity

Simulator fidelity is certified per domain and policy version. Certification uses
separate selection and untouched validation cohorts and predeclares:

- the independent human unit;
- minimum support derived from a power analysis;
- lower bounds for critical-class sensitivity and specificity;
- slice-level disagreement tolerances;
- an equivalence margin for simulated-versus-human candidate/baseline treatment
  effects; and
- an expiration rule.

Insufficient validation support produces `INCONCLUSIVE`.

Normal release runs use one certified primary simulator policy per domain. Alternate
simulator models run as periodic or sampled canaries, not as a mandatory multiplier on
every candidate.

Simulator stop decisions are evaluation signals, not containment. The target can
influence the simulator through the transcript; only external counters, reservations,
policy, and deadlines are containment authorities.

## Assertions and Evidence

### Assertion categories

The current typed seam is `ExperimentScorecard` with
`ScorecardRequirement`; future assertion categories extend or migrate that
contract rather than introducing a parallel scorecard model.

```text
AssertionSpec {
    assertion_id
    category
    evaluator_ref
    parameters
    gate_effect
}
```

Supported categories:

| Category | Examples |
|---|---|
| Environment | final record state, file checksum, resource existence |
| Communication | required disclosure, user-facing confirmation |
| Semantic/history | bounded condition over structured history evidence |
| Action | required, prohibited, or ordered tool/policy action |

Assertion implementations are server-registered and versioned. Plans select an
evaluator and typed parameters; they cannot upload executable assertion functions.

### Evidence envelope

The current runtime emits `TrialTerminalEvidence`. The envelope below is a
proposed evolution of that type, not a second evidence model:

```text
EvidenceEnvelope {
    schema_version
    subject_digest
    trial_identity
    target_identity
    world_state_snapshot_hash
    action_and_policy_facts
    completion_facts
    resource_usage
    stop_cause
    provenance
}
```

Raw prompts, model reasoning, credentials, full tool payloads, and raw customer
transcripts are excluded by default.

The evidence root hash covers every row used by a blocking score.

### Outcome separation

The following are never aliases:

| Record | Meaning |
|---|---|
| `TargetTerminalState` | The target stopped, failed, or completed operationally |
| `TrialStopCause` | The controller stopped because of goal, budget, deadline, cancellation, or error |
| `ScenarioOutcome` | Deterministic or statistically evaluated task result |

A clean target terminal state can coexist with a failed scenario outcome.

### Evaluator authority

Evaluators declare one authority class:

| Authority | Allowed use |
|---|---|
| Deterministic | Blocking safety/state/action assertion |
| Calibrated statistical | Comparative quality metric when policy and current calibration permit |
| Heuristic | Diagnostic and triage only |

Plan admission rejects an invalid combination such as a blocking assertion backed by a
heuristic evaluator.

A calibrated statistical evaluator never overrides deterministic failure. Its exact
model, prompt, rubric, parser, calibration cohort, and validity interval are part of
the subject.

## Judge Calibration

Calibration separates three questions:

1. Do human labelers agree enough to define usable gold?
2. Does the judge agree with adjudicated gold by class and slice?
3. How does judge error change the aggregate metric and uncertainty?

A calibration artifact pins:

- judge model, prompt, rubric, parser, and domain;
- blinded labels and adjudication;
- class prevalence and strata;
- selection and untouched validation splits;
- confusion matrix;
- raw agreement and chance-corrected agreement;
- class-specific precision, recall, sensitivity, and specificity;
- abstention coverage and selective accuracy when applicable;
- position-swap and test-retest results for pairwise judges; and
- expiration conditions.

Deterministic facts should remain deterministic. Do not replace environment-state,
temporal, redaction, budget, or abstention oracles with model calls merely to reuse a
judge framework.

If one structured multi-label call is adequate, it is preferred to multiple isolated
calls. Split dimensions only when held-out evidence shows a validity gain worth the
additional cost and latency.

## Metric and Decision Contract

### Metric definition

Every sampled metric declares:

```text
MetricDefinition {
    metric_id
    direction
    estimand
    unit
    independent_unit
    cluster_key
    paired_key
    estimator
    practical_margin
    alpha
    confidence_method
    acceptable_alternative
    unacceptable_alternative
    gate_kind
    hypothesis_family
}
```

Metrics without these fields may be reported descriptively but cannot produce a
sampled release decision.

### Direction-normalized paired decision

Define an oriented delta where larger is always better:

```text
utility_delta = direction_sign * (candidate - baseline)
direction_sign = +1 for higher-is-better
direction_sign = -1 for lower-is-better
```

For tolerated regression `margin`:

```text
PASS         when lower_bound(utility_delta) >= -margin
REGRESSION   when upper_bound(utility_delta) <  -margin
INCONCLUSIVE otherwise
```

Baseline and candidate use the same cases, environment states, simulator policy, and
paired seeds. A missing pair is an evidence error, not a zero.

### Method by metric class

| Metric class | Method |
|---|---|
| Fixed-corpus invariant or safety count | Exact assertion or exact upper failure-rate bound |
| Paired binary | Matched effect interval/test for the declared margin |
| Paired numeric | Cluster-aware interval on the paired utility delta |
| Stochastic live outcome | Hierarchical cases with repetitions nested within cases |
| Latency quantile | Quantile-appropriate paired/bootstrap method |

Standard McNemar is a zero-difference diagnostic; it does not by itself implement a
nonzero non-inferiority margin. Clustered binary data requires a cluster-aware method.

### Operating characteristics

Before a sampled metric blocks, simulate or estimate the exact production gate over
representative clustered observations:

- control false PASS at the non-inferiority boundary
  `utility_delta = -margin`;
- measure pass power at a separately declared acceptable alternative, commonly
  `utility_delta = 0`;
- measure regression-detection power at a separately declared unacceptable
  alternative below `-margin`;
- report interval coverage and effective independent support; and
- return `INCONCLUSIVE` when support is insufficient.

Adding observations inside the same few users, tasks, sessions, or tenants does not
create new independent clusters.

### Multiple metrics

For an all-required release, use intersection-union non-inferiority: every required
metric must establish non-inferiority.

If the system separately declares overall regression when any metric regresses, define
the reverse one-sided regression hypotheses and apply a family-wise procedure such as
Holm to those p-values.

False-discovery-rate procedures are for exploratory diagnostics, not primary release
authority.

## Repeat Reliability

For a case with `c` successes among `n` independent repetitions:

```text
pass_any_at_k = 1 - C(n-c, k) / C(n, k)
pass_all_at_k = C(c, k) / C(n, k)
```

Compute these values per logical case and then aggregate across cases. Never pool all
successes across heterogeneous cases.

Persist a logical trial identity:

```text
TrialIdentity {
    case_id
    scenario_id
    persona_or_user_profile_id
    environment_fixture_id
    variant_id
    simulator_policy_id
    repetition
    paired_seed
}
```

Required validation includes:

- `k = 1`;
- `k = n`;
- `c = 0`;
- `c = n`;
- monotonicity in `k`;
- `n < k`;
- missing or duplicate repetitions; and
- rejection of pooled-case estimation.

Branched rollouts that share a prefix are correlated. They are useful for failure
discovery and debugging but do not enter independent-trial pass-any/pass-all
estimators.

## Resource Containment

### Resource envelope

```text
ResourceEnvelope {
    version
    limits: ResourceAmounts {
        cost_micro_usd
        tokens
        turns
        model_calls
        tool_calls
    }
    deadline
}
```

This is the implemented `moa_core::types::resource::ResourceEnvelope` contract.
Future dimensions such as retrieved bytes or parallel children extend that
shared contract. Reserved work must consume at least one dimension; zero never
means unlimited. Cost, token, call, byte, and time accounting uses integer base
units; floating-point currency is not a reservation or reconciliation
authority.

The admission layer also bounds:

- plan and field bytes;
- number of cases, variants, personas, profiles, and repetitions;
- checked total matrix cardinality before allocation;
- active and queued runs per subject and owner;
- tenant/workspace and fleet concurrency;
- provider QPS; and
- daily or release-window spend.

### Reservation and reconciliation

Before each provider, tool, simulator, judge, or side-effecting call:

1. estimate the worst-case authorized usage;
2. atomically reserve it from the trial and run;
3. refuse dispatch when the reservation cannot be made;
4. reconcile actual integer usage after completion; and
5. release unused reservation.

Parallel trials share an atomic run ledger. Post-hoc score thresholds remain useful for
reporting but are not containment.

### Deadlines and cancellation

Persist absolute run and trial deadlines. Every external call receives the remaining
duration, and the durable trial races its complete child tree against the deadline.

Cancellation:

- fences new reservations;
- propagates to target, tools, providers, sandboxes, and child trials;
- records one typed terminal stop cause;
- preserves completed evidence; and
- is replay-idempotent.

Dropping an outer future without cancelling child work is not sufficient.

### Loop and timeout defenses

- Repeated-action detection uses progress-aware state/output fingerprints, not only
  repeated tool names and arguments.
- A legitimate repeated action that changes state is not a loop.
- Tool-specific timeouts are runtime-validated and bounded by the remaining trial and
  sandbox lifetime.
- Matrix multiplication uses checked arithmetic before allocation.
- Queues are bounded and apply owner- and fleet-level admission.

## Evaluation Lanes

| Lane | Purpose | Provider | Release authority |
|---|---|---|---|
| Unit/property | Contracts, state machines, statistics, bounds | None | Hard |
| Deterministic replay | Agent loop with recorded/scripted dependencies | Replay/scripted | Hard |
| Service integration | Production orchestration, persistence, replay, authz | Scripted | Hard |
| Mutation | Prove guards and assertions catch intentional faults | None/scripted | Hard |
| Simulated-user | Multi-turn behavioral comparison | Pinned simulator | Policy-dependent |
| Live provider | Trend, calibration, provider compatibility | Real, billed | Explicit and sampled |
| Human fidelity | Simulator certification and treatment-effect validation | Real humans | Certification only |
| Production monitoring | Canary, drift, rollback signals | Production | Separate rollout policy |

Paid lanes are ignored by default and require:

- an explicit run flag;
- credentials;
- a positive budget;
- a forecast before dispatch;
- reservation/reconciliation;
- privacy authorization where human data is used; and
- clear failure when an opt-in requirement is missing.

A deterministic service lane should use the same orchestration and persistence path as
production with a scripted provider seam.

## Suite Validity

Every capability or quality suite has:

- a negative/null control that ignores or permutes the relevant input;
- a positive/oracle control proving the harness can score known-good behavior;
- per-slice control results;
- exact provenance and package-leakage checks; and
- targeted mutation evidence for the scorer/gate.

Resource metrics use exact boundary fixtures. Safety invariants use adversarial and
mutation fixtures rather than artificial null models.

Null ceilings are derived from repeated null seeds and an upper uncertainty bound. A
null below its ceiling is necessary but does not by itself prove construct validity.

Generated tasks use an independent validity oracle when one exists. Checked-in tasks
use an authoring validator; the system does not invent a meaningless “solution
function” for data that has no world-state solution.

## Dataset Governance

Maintain three distinct cohorts:

| Cohort | Purpose |
|---|---|
| Authoring | Visible failures and local iteration |
| Anchor | Immutable longitudinal paired regression |
| Rolling hidden | Freshness, leakage, and adaptive-overfit surveillance |

Candidate authors may iterate freely against authoring cases. Hidden release attempts
are rate-limited and versioned. Repeatedly inspected hidden cases become validation
data and must rotate.

Do not replace the anchor with the rolling cohort; that destroys longitudinal
comparability.

For closed-corpus retrieval:

- deny network;
- pin allowed corpus hashes;
- reject question, answer, label, and evaluation-metadata artifacts;
- detect exact and near-duplicate leakage; and
- retain source-object provenance.

For open-web evaluation:

- log retrieved URL, content hash, and timestamp;
- distinguish legitimate sources from answer-key or benchmark-distribution pages;
- report contaminated and clean strata; and
- fail closed when provenance is missing.

Public benchmark provenance does not automatically imply search-time contamination in
a closed-corpus system.

## Privacy and Multi-Tenancy

- Owner scope is a typed field in subject, run, trial, evidence, attestation, and
  serving-pointer records.
- Every protected read checks authorization before loading the resource.
- Wrong-scope IDs cause zero provider calls and zero writes.
- Evaluation sessions are newly created; production sessions are never continued.
- Raw customer transcripts are not copied into reusable personas or fixtures.
- Human-derived cases require consent, de-identification, contribution provenance,
  retention, and erasure closure.
- Aggregated persona traits use minimum cohort sizes and exclude identifying voice
  snippets.
- Evidence stores bounded hashes and typed facts instead of raw prompts and outputs
  unless a separately authorized debugging workflow requires them.

## Idempotency and Replay

Every command has a caller-visible or server-derived idempotency key:

| Operation | Stable identity |
|---|---|
| Candidate submission | owner + activation target + candidate hash |
| Release attempt | subject digest + attempt generation |
| Trial | run + logical trial identity |
| Provider/tool call | trial + loop generation + call ordinal |
| Score | trial + evaluator ref + evidence hash |
| Attestation | subject digest + release attempt |
| Activation | serving pointer + expected version + attestation |

External side effects happen through durable service/workflow calls or journaled
blocks. Time, randomness, and generated IDs are either derived from stable coordinates
or journaled once.

Terminal state transitions are conditional and idempotent. A late success cannot
overwrite `Rejected`, `Inconclusive`, `Superseded`, `Cancelled`, or another terminal
state.

## Fast-Path Design

Correct evaluation does not require running the largest live matrix for every change.

Order work by cost and information value:

```text
1. subject/policy/schema validation
2. impacted deterministic assertions and safety invariants
3. null/oracle and fixture-contract checks
4. small paired candidate/baseline sample
5. additional independent samples only when needed to resolve the margin
6. optional live or alternate-simulator canaries
```

Additional optimizations:

- cache deterministic results by the complete subject/evaluator/evidence hash;
- invalidate cache entries on any pinned dependency change;
- run candidate and baseline concurrently on paired cases;
- coalesce superseded candidates before provider dispatch;
- use one certified primary simulator per domain;
- schedule alternate simulators on a sampled fraction;
- select impacted quality assertions while always retaining mandatory safety checks;
- lazily materialize or page large trial matrices; and
- use a valid group-sequential or confidence-sequence design if early stopping and
  repeated peeking are required.

Without a sequentially valid design, precommit the sample size and do not stop because
an intermediate point estimate looks favorable.

## APIs and Commands

The concrete transport is implementation-specific. The domain needs these commands:

```text
submit_candidate(candidate_revision, activation_target)
get_release_attempt(release_attempt_id)
cancel_release_attempt(release_attempt_id, reason)
retry_inconclusive(release_attempt_id)
activate(activation_target, expected_serving_version, attestation_id)
```

Read APIs expose:

- candidate and attempt state;
- exact subject digest and pinned dependency summary;
- run/trial progress;
- resource forecast, reservations, and actual usage;
- assertion and metric results;
- missing/inconclusive evidence;
- attestation status and expiry; and
- activation audit history.

Callers never submit a verdict or an attestation body. The server derives them from
persisted evidence.

## Failure Semantics

| Failure | Release outcome |
|---|---|
| Structural/schema/policy invalid | Rejected before dispatch |
| Wrong scope or authorization | Rejected before read/dispatch |
| Missing blocking evidence | Inconclusive or fail-closed rejection by policy |
| Deterministic assertion failure | Regression/rejected |
| Statistical support insufficient | Inconclusive |
| Candidate superseded | Superseded |
| Budget or deadline exhausted | Inconclusive unless policy defines deterministic failure |
| Provider/tool transient failure | Retry within envelope, then inconclusive |
| Simulator policy stale | No attestation |
| Tool/catalog snapshot stale | No attestation |
| Attestation stale/consumed | Activation rejected |
| Serving pointer changed | Activation CAS rejected; reevaluate against new baseline |

An operational error does not become a behavioral failure unless the release policy
explicitly defines that behavior as part of the product contract.

## Observability and Audit

Every log, trace, score, and audit event carries:

- owner scope;
- subject digest;
- release attempt, run, and trial IDs;
- target and baseline identities;
- simulator/evaluator/model/tool-catalog versions;
- resource reservation and actual usage;
- stop cause and outcome;
- evidence root hash; and
- activation decision ID where applicable.

The product should answer:

- What exact candidate was evaluated?
- Against which serving baseline?
- Which environment, tools, model, simulator, and evaluator versions were used?
- Which assertions blocked or were missing?
- Was the result paired and adequately supported?
- Why was the attempt rejected or inconclusive?
- Which attestation authorized the current serving pointer?

Audit records do not need raw model content to answer these questions.

## Validation Strategy

### Deterministic contract tests

- subject canonicalization and hash stability;
- candidate/release state-machine transitions;
- serving-pointer backfill and resolver parity;
- stale/superseded/consumed attestation rejection;
- release-policy separation and mandatory assertion enforcement;
- matrix overflow and every exact limit boundary;
- reservation/reconciliation under parallel trials;
- deadline and cancellation propagation;
- typed stop-cause/outcome mapping;
- assertion evidence completeness and versioning;
- statistical formula and grouping properties; and
- cache invalidation on every subject field.

### Security and service tests

- wrong-owner session, candidate, run, evidence, and attestation IDs;
- proof of zero protected reads, provider calls, and writes on rejection;
- evaluation-origin tool policy denies production capabilities;
- replay creates no duplicate dispatch, score, charge, or activation;
- concurrent activation yields one CAS winner;
- simulator cannot invoke an unrequested user action;
- target cannot access hidden oracle state; and
- connector last-known-good survives candidate refresh failure.

### Statistical tests

- false-PASS behavior at the non-inferiority boundary;
- pass power at the declared acceptable alternative;
- regression-detection power at the declared unacceptable alternative;
- interval coverage under the declared cluster structure;
- paired seed and case completeness;
- pass-any/pass-all edge cases;
- no pooled-case estimation; and
- `INCONCLUSIVE` under insufficient support.

### Mutation tests

Remove or invert:

- activation-attestation checks;
- authorization-before-read;
- reservation comparisons;
- deadline branches;
- evidence-version checks;
- mandatory safety assertions;
- action ordering/prohibition checks; and
- candidate-generation fences.

The owning focused test must fail for each mutation.

### Live tests

Live and human-data lanes are never default CI. They require explicit authorization,
budget, credentials, and privacy approval. A small canary runs before a full sweep, and
the full result is published only when every planned case was attempted or the report
is explicitly marked incomplete.

## Migration Strategy

This is a hard architectural migration, not a long-lived compatibility design.

1. Inventory every path that can mutate serving behavior.
2. Add type-owned serving pointers.
3. Backfill pointers from the exact currently serving revisions.
4. Switch all normal resolvers to the new pointers and verify byte-for-byte serving
   parity.
5. Add immutable candidates, subject digests, release attempts, and isolated execution.
6. Add assertions, evidence, resource enforcement, and release decisions in shadow
   mode.
7. Validate null/oracle controls, mutations, statistics, and simulator/tool policies.
8. Enable attestation-required activation for subject classes whose prerequisites are
   complete.
9. Remove direct publish/import/deploy bypasses and old status semantics.
10. Delete compatibility readers and migration-only parity code.

Existing serving revisions may remain active during migration. Every new serving
transition after cutover requires the new path.

## Recommended Delivery Order

```text
P0 containment and security
├── authorization-before-read
├── nonzero limits, checked matrices, queue quotas
├── runtime resource reservation and durable deadlines
└── isolated evaluation capability policy

P1 shared truth
├── subject/evidence/evaluator/metric contracts
├── typed assertions and mock environment
├── paired comparison and reliability
├── null/oracle controls
└── serving-pointer backfill and resolver cutover

P2 non-serving release workflow
├── candidate state/store
├── isolated target adapter
├── durable run/trial orchestration
├── attestation schema and verifier
└── coalescing, cancellation, replay

P3 validity
├── gate operating-characteristic validation
├── judge calibration where actually used
├── minimum simulator certification
├── connector snapshot pinning
└── hidden cohort governance

P4 activation
├── single activation repository
├── exact serving-pointer CAS
├── remove bypasses
└── enable fail-closed policy by eligible subject class

P5 expansion
├── dual-control environments
├── broader simulator fidelity coverage
├── live provider canaries
└── production drift and rollback integration
```

P0 and most of P1 can proceed in parallel. P2 can run in non-serving shadow mode while
P3 validates the evidence. P4 must not start for a subject class until all of its
load-bearing P3 prerequisites pass.

## Design Consequences

This architecture intentionally adds explicit candidate, evidence, and activation
objects. That complexity replaces less visible and more dangerous complexity:

- post-publish evaluation that is too late to gate;
- results that do not identify what actually ran;
- model transcripts mistaken for state;
- cost thresholds checked after money was spent;
- simulator output treated as containment;
- unpaired point estimates treated as regression evidence; and
- multiple publish/import/deploy paths with inconsistent protection.

The resulting system is faster in the common case because cheap deterministic work
runs first, exact caching is safe, superseded changes are coalesced, baseline and
candidate are paired, and expensive simulator or live-provider work is reserved for
cases that need it.

## Research Context

These sources motivate parts of the design but are not normative specifications:

- [Mind the Sim2Real Gap in User Simulation for Agentic Tasks](https://arxiv.org/abs/2603.11245)
- [Tau-squared Bench: Evaluating Conversational Agents in a Dual-Control Environment](https://arxiv.org/abs/2506.07982)
- [RealUserSim: Grounded User Simulation](https://arxiv.org/abs/2605.20204)
- [A Statistical Framework for Evaluating Large Language Models](https://arxiv.org/abs/2411.00640)
- [Tau-bench and the combinatorial pass-at-k estimator](https://arxiv.org/abs/2406.12045)
