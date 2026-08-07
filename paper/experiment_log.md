# Experiment Log

## Contribution (one sentence)
A production learning loop in which LLM agents distill reusable skills from their own
successful task executions and every candidate must pass (a) a deterministically generated
regression suite whose oracle requires the response to carry facts corroborated
word-boundary-exactly by successful tool outputs (no labels, no LLM judge) and (b) held-out
"sibling suites" from other sessions of the same recurring task, before human-reviewed
promotion with append-only provenance and an automatic rollback monitor.

## Harness
`paper/experiments/oracle_study/` — standalone crate, path-deps on the real
`moa-skills` / `moa-eval-core` / `moa-memory-pii` crates (workspace Cargo.lock pinned).
Everything is deterministic (SplitMix64 seeds 7/11/23/31); no LLM or network calls.
Code paths exercised are the production ones: `sanitize_segment_evidence` (HeuristicPiiClassifier),
`generate_skill_test_suite_source_for_name`, `evaluate_assertions` + `builtin_registry`.
Results: `paper/results/oracle_study_results.json` (regenerate with `cargo run`).

## Experiments

### E1: Oracle characterization (n=300; tool calls t∈2..8, carried facts k∈0..5)
- Claim tested: the oracle selects grounded facts whenever the response carries ≥1
  tool-corroborated fact, and falls back to keywords only otherwise.
- Result: 250/300 grounded (exactly the k≥1 cases), 50/300 keywords (exactly k=0).
  Suite generation byte-identical across repeated runs: 300/300.
- Facts per grounded case: 1:50, 2:55, 3:15, 4:48, 5:82 (cap MAX_ORACLE_FACTS=5).
- Note: bare digits 3..9 are groundable (trivial-number floor excludes only 0–2 and years);
  enumeration digits ("Finding 3") grounded against step indices in tool output in some cases.

### E1b: Adversarial substring grounding (n=200)
- Claim: word-boundary grounding refuses facts appearing only inside longer tokens
  (e.g., response "412ms" vs tool output "1412msx").
- Result: 0/200 false groundings.

### E1c: Punctuation-extension adversarial grounding (n=300, seed 47) — added after external review
- Claim tested: reviewer's counterexamples — word boundaries do NOT protect against
  extensions beginning with non-word chars ('.', '-', '/').
- Result: **300/300 false groundings** across six families (currency cents, ref-token
  suffix, path prefix, URL prefix, bare-number decimal, hyphenated unit).
- Consequence: "zero substring false-groundings" claim retracted; paper now reports the
  precision boundary exactly and motivates typed per-class canonicalization + per-fact
  provenance as the fix.

### E2: Perturbation detection, grounded-facts oracle (n=200; k∈2..5)
- exact reproduction: 0/200 gate failures (100% pass)
- paraphrase keeping facts verbatim: 15/200 failures (92.5% pass). All 15 traced to
  incidental grounded enumeration digits ("3","4") the paraphrase dropped (verified by
  inspecting expected sets; see DBG mode).
- fabricated facts (every digit mutated): 200/200 detected
- omitted facts: 200/200 detected

### E3: Same perturbations, keyword-fallback oracle (ablation twin segments)
- exact: 0/200 failures; paraphrase: 200/200 failures (0% pass — keywords are brittle
  near-verbatim matching); fabricate: 200/200; omit: 200/200.
- Story: detection of fabrication is equal; the oracles differ in *selectivity* —
  grounded facts accept semantically equivalent variation, keywords reject it wholesale.

### E4: Action assertions (n=200)
- dropped distinct tool: 200/200 detected (required_actions, blocking)
- reversed order + duplicated call, same distinct set: 0/200 failures
  (order assertion is diagnostic-only by design).

## Figures
| Artifact | Where |
|---|---|
| Pipeline diagram (TikZ, in-tex) | Fig. 1, main.tex |
| Perturbation × oracle table | Table 1 |
| Action assertion table | Table 2 |

## Failed / notable
- First build failed from dependency drift; fixed by pinning the workspace Cargo.lock.
- Keyword-twin segments initially grounded the bare digit "3" ("Finding 3" vs "step 3 ok");
  documented as real oracle behavior, generator adjusted for a clean ablation.

## Open questions
- Live end-to-end numbers (resolution-rate deltas from promoted skills) require production
  traffic; targeted for v2.
