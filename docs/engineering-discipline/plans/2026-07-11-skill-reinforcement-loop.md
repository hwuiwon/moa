# Skill Self-Reinforcement Loop — Audit and Improvement Plan (2026-07-11)

Audit of MOA's skill learning loop against the "procedural-memory induction"
framing (trace capture → pattern mining → two-tier synthesis → dedup + eval
gate → shadow/canary → runtime reweighting), and the plan that follows from
it. Breaking changes are allowed; no backwards compatibility is required.

## Verdict on the proposed framing

The framing is correct about the shape of the loop — it is
mining-and-memory, not policy-gradient RL — and MOA already implements more
of its trust layer than the proposal assumes. Three of its prescriptions are
wrong for MOA and are rejected below (offline PrefixSpan-style motif mining,
auto-promotion through shadow→canary, a full contextual-bandit selector).
Five of its observations name real gaps that the audit confirmed with code
evidence (semantic similarity, generalization across recurrences, procedure
synthesis, staleness, post-promotion safety), plus three gaps the proposal
missed (segment-boundary fragility, credit misassignment, a weak gate
oracle).

## What exists today (audited 2026-07-11)

The loop: session events → `task_segments` → 5-signal heuristic assessment →
`experience_records` + `experience_attributions` → dispatch gate (outcome
resolved w/ confidence ≥ 0.7, or partial ≥ 0.85 + helpful verification
attribution; ≥ 8 tool calls) → detached `SkillLearning` workflow →
improve-vs-create routing → draft artifact + `Proposed` learning candidate →
human operator review → fail-closed regression gate (held-in compare,
held-out pool of previous suite + ≤ 3 sibling suites, procedure smoke,
$0.50 budget) → publish → outcome-weighted ranking via materialized views
refreshed every 15 min. Weakness mining files failure-pattern candidates
deterministically. Everything is tenant-scoped under RLS; there is no
cross-tenant path.

Strengths worth preserving: fail-closed gate semantics with honest
`AcceptanceChecks` derivation, three-layer exact dedup with sibling-suite
accumulation as held-out material, review-required promotion (operator
ruling), always-on event-driven learning (no offline batch), failure-driven
mining alongside success-driven distillation.

## Confirmed gaps (with evidence)

| # | Gap | Evidence |
|---|---|---|
| G1 | Segment boundary hangs on one LLM flag. `is_new_task` only comes from the query-rewrite success path; every skip/disable path hardcodes false, so after the first segment a session never splits again — one mis-scoped learning unit per session. | `moa-brain/src/pipeline/segments.rs:33-37`, `moa-core/src/types/query_rewrite.rs:29-41`, `moa-brain/src/pipeline/query_rewrite/gate.rs:60-111` |
| G2 | Credit misassignment: `skills_activated` = every skill the injector put in the manifest, not skills the model used. Outcomes credit/blame all injected skills, coupling reinforcement to injection. | `moa-orchestrator/src/workflows/turn_execution/mod.rs:773-826` |
| G3 | No explicit user feedback signal. Outcome is purely heuristic; the closest thing is string-matching the next user message ("thanks"/"wrong"). No accepted/edited/rejected capture. | `moa-brain/src/segment_assessment/continuation_signal.rs:43-77` |
| G4 | No semantic similarity anywhere in the skill loop. Routing is token Jaccard (threshold 0.5), ranking is keyword overlap, dedup is exact name/fingerprint locks. Memory already has pgvector; skills use none of it. | `moa-skills/src/distiller.rs:406-466`, `moa-brain/src/pipeline/skills/tier1_metadata.rs:249-270`, `moa-skills/src/proposals.rs:186-278` |
| G5 | No generalization across recurrences. Distillation is strictly one experience → one proposal; recurring sessions only accumulate held-out *test suites* onto the open proposal, never re-synthesize a broader skill. No trajectory normalization or arg parameterization exists. | `moa-skills/src/distiller.rs:107-123`, `moa-skills/src/proposals.rs:389-476`, `moa-brain/src/learning/experience.rs:144-153,660-688` |
| G6 | Learned skills are never procedures. The distiller emits `SKILL.md` only; `has_procedure` is hardcoded false for document-derived skills; procedures exist only via human-authored `skill.moa.yaml`. | `moa-skills/src/format.rs:125-127`, `moa-skills/src/artifact.rs:136-153` |
| G7 | Weak gate oracle. The regression suite is auto-derived (input = first message, expected = 5 keywords of the final response + tool-name sequence); evaluators are substring/LCS/tool-success. A candidate passes by reproducing keywords and tool order. No LLM-as-judge groundedness check exists. | `moa-skills/src/regression.rs:84-109`, `moa-eval/core/src/evaluators/{output_match,trajectory_match,tool_success}.rs` |
| G8 | No post-promotion safety: no shadow run, no canary, no auto-rollback. Promotion is all-or-nothing; a rotting skill's only consequence is a slowly declining resolution rate. | `moa-orchestrator/src/services/learning_review.rs`, `moa-artifacts/src/document.rs:70-93` |
| G9 | No skill staleness lifecycle. Memory has decay/expiry (`decay_half_life_days: 180` etc. on the compaction cron); published skills have no re-validation, expiry, or tool-drift detection. | memory: `moa-memory/lifecycle/src/consolidate.rs:53-98`; skills: absent |
| G10 | No exploration. Ranking is deterministic greedy top-K; unexposed skills sit at the 0.5 prior forever while injected skills accumulate all the outcome rows (G2 amplifies this). The computed Helpful/Harmful attribution effects are written but never consumed by ranking. | `tier1_metadata.rs:75-105,272-284`, `V000001__session_baseline.sql:2138-2154` |
| G11 | Sparse loop observability: no counters for proposals filed, promotion rate, time-in-review, or post-promotion usage; only ClickHouse candidate facts and an experiments-only counter. | `moa-observability/src/runtime_metrics.rs:872-884` |
| G12 | Failure mining covers only durable tool errors and denied approvals; procedure-run failures (separate workflow) and skill-attributed failures are not mining signals. Lessons (`learn_lesson`) still have no production caller. | `moa-skills/src/mining.rs:135-176`, `moa-skills/src/lessons.rs:47` |

## Rejected prescriptions from the proposal

- **Offline PrefixSpan/motif mining over a trace store.** Per-tenant volumes
  make frequent-sequence mining worse than what MOA already has: the
  fingerprint dedup-bump already detects "this exact task recurred N times"
  event-natively, with zero batch infrastructure. The missing piece is not
  detection of recurrence, it is *generalization at recurrence time* (P3
  below). LLM re-synthesis over N concrete sibling instances produces the
  parameterized abstraction (`lookup(X) → get(X, range)`) that symbolic
  sequence mining approximates, with far less machinery.
- **Auto-promotion through shadow→canary.** Conflicts with the standing
  operator ruling that every learned change requires tenant operator/admin
  review. Canary belongs *after* human accept, as a staged rollout with
  auto-demotion (P5), not as a replacement for review.
- **A full contextual bandit for skill selection.** The prerequisite is
  honest reward attribution, which is broken today (G2). Fix credit
  assignment and add a bounded exposure bonus first (P6); revisit a real
  bandit only if the simple version measurably under-explores.

## Plan

Ordered by dependency: P1 fixes the learning unit everything else consumes;
P2 gives the loop a semantic backbone; P3–P4 improve what gets learned and
how it is verified; P5–P6 close the loop after promotion.

### P1 — Fix the foundation: learning units and reward signal (G1, G2, G3)

1. **Boundary fallback heuristic.** In `SegmentTracker`, when the rewrite
   gate skipped (no LLM `is_new_task`), decide boundaries deterministically:
   long idle gap since last event, explicit new-request markers in the user
   message, and a disjoint tool-cluster shift. Keep the LLM flag as the
   primary signal when present. Pin with tests that a rewrite-disabled
   session still segments per task.
2. **Track used skills, not injected skills.** Record which skills the model
   actually engaged (SKILL.md materialized/read in the hand, skill action or
   `run_procedure` invoked) as `skills_used` on the segment, distinct from
   `skills_activated`. Attribute outcomes to used skills; injected-but-unused
   is its own (weak, negative-relevance) signal for ranking. Breaking change
   to `experience_attributions` semantics and both materialized views.
3. **Explicit feedback event.** Add a `UserFeedback` session event
   (accepted/rejected/corrected + optional text), exposed through the edge
   API and Slack reactions. Feed it into segment assessment as a
   high-weight signal (above the continuation heuristic) and into
   `experience_records` as an outcome override. This is the single highest
   -leverage reward-quality improvement available.

### P2 — Semantic backbone: embeddings for routing, dedup, ranking (G4)

Reuse the existing pgvector + embedding-provider infrastructure from memory.

1. Embed published skills (name + description + tags + trigger summary) into
   a `skill_embedding` column on artifact revisions; refresh on publish.
2. Embed the task (normalized summary + facets) at experience time; store on
   the experience record.
3. **Routing:** improve-vs-create becomes hybrid — embedding cosine as the
   primary score, keyword Jaccard as a tie-breaker; threshold calibrated
   against the existing routing-evidence payloads already stored on
   candidates.
4. **Proposal dedup:** after the exact name/fingerprint locks, add an
   embedding NN check against open proposals and existing skills; high
   similarity routes to improvement/sibling-accumulation instead of a new
   proposal. Kills the differently-worded-duplicate class the exact locks miss.
5. **Ranking:** add an embedding-similarity term to `rank_skills` alongside
   keyword overlap (weights re-tuned; keep the four-branch outcome logic).
   Guard cache stability: compute per-turn similarity from the same query
   keywords source already used, deterministically.

### P3 — Generalize at recurrence; synthesize procedures when stable (G5, G6)

1. **Re-synthesis trigger.** When a `Proposed` candidate accumulates its
   Nth sibling experience (N = 2), instead of only pooling the suite,
   dispatch a generalization pass: feed all sibling segments to the
   distiller with a prompt that must produce a *parameterized* skill —
   explicit inputs, invariant steps, variable slots. Replace the draft
   revision (it is still unpublished; no compat concerns).
2. **Trajectory stability measurement.** Across siblings, compute pairwise
   tool-sequence similarity with the existing trajectory-LCS logic. Store the
   stability score in the candidate evidence.
3. **Procedure emission.** When stability is high (all siblings share the
   same tool sequence modulo args) and every step is a pure tool call,
   have the generalization pass additionally emit a draft
   `skill.moa.yaml` with a `ProcedureDefinition` (nodes = tool calls with
   input templates, args lifted to procedure inputs). It rides the same
   draft artifact through the same review + procedure smoke gate. This is
   the two-tier design: playbook (`SKILL.md`) by default, procedure when
   the evidence says the sequence is deterministic. No auto-run change:
   procedures remain agent-invoked via `run_procedure`.

### P4 — Strengthen the gate oracle (G7)

1. Add an LLM-as-judge groundedness evaluator to the skill gate's evaluator
   set (judge model = configured non-main-loop task model; rubric: does the
   response actually accomplish the case's task, is it grounded in tool
   results). Keep the deterministic evaluators; the judge adds a semantic
   floor the keyword oracle can't provide. Raise the per-gate budget
   accordingly and keep fail-closed semantics (judge unavailable = operational
   error, not a waiver).
2. Improve suite derivation: expected-output extraction should capture
   verifiable facts from tool results (numbers, identifiers) rather than the
   5 longest keywords of the final response.

### P5 — Staged promotion, staleness, and auto-demotion (G8, G9)

1. **Canary state after accept.** Add a `Canary` artifact revision status
   between accept and fully published: the skill serves normally but is
   flagged, and a post-promotion monitor (extend the existing 15-min matview
   cron) compares its resolution rate over the first K uses against the
   tenant baseline / previous revision. Pass → `Published`; regress → the
   monitor auto-files a rollback `LearningCandidate` and reverts serving to
   the previous revision. Review stays human; demotion is automatic and
   evidence-preserving.
2. **Skill lifecycle job.** Mirror the memory-lifecycle cron for skills:
   (a) re-run each published skill's own regression suite on a slow cadence
   (staleness re-validation, budgeted); (b) flag skills whose
   `allowed-tools`/procedure references no longer resolve in the
   capabilities catalog (tool drift); (c) file demotion candidates for
   skills with sustained low resolution rate and sufficient sample count.
   Rot becomes a reviewed proposal, not a silent decline.

### P6 — Exploration and loop observability (G10, G11, G12)

1. **Exposure bonus.** In `rank_skills`, add a small bounded bonus for
   skills with low use-count under the current task fingerprint (optimistic
   prior), so alternatives can earn evidence; consume the
   Helpful/Harmful attribution effects (currently written and ignored) as a
   modifier on the smoothed task rate. Deterministic (seeded by fingerprint),
   preserving cache-stable manifests.
2. **Metrics.** Prometheus counters/histograms: proposals filed by source
   (distilled/mined/experiment), promotion/rejection rate, time-in-review,
   gate outcomes by failure class, canary pass/demote, post-promotion skill
   use and resolution delta.
3. **Widen mining signals.** Add procedure-run failures and
   canary demotions as mining inputs; wire `learn_lesson` through the same
   `learning_candidates` review boundary (a lesson is a small skill-scoped
   proposal), unblocking the built-but-dead lesson pipeline.

## Sequencing and verification

- P1 → P2 → P3 are sequential (each consumes the previous layer's output).
  P4 is independent after P1; P5 and P6 depend on P1–P2 only.
- Every phase: unit + `_db`/`_db_memory` lane tests per AGENTS.md, plus the
  hermetic gate e2e (`moa-orchestrator/tests/skill_learning_gate_e2e.rs`)
  extended for new gate behavior; P3 adds a recorded procedure-synthesis
  scenario; P5's monitor gets a db-lane test with seeded outcome rows.
  Live validation for the full loop rides the 100-session sweep
  (`moa-100-session-sweep`) with skill-activation metrics compared before/after.
- Migrations may be edited in place (pre-prod); live compose DBs need
  dev-wipe after schema changes to `experience_attributions`/matviews.
