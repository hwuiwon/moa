# Step 47 — Skill Regression Testing

_Auto-run eval suites when skills are updated. Roll back if scores regress. Close the skill improvement feedback loop._

---

## 1. What this step is about

MOA's skills self-improve during use (Step 12, `maybe_improve_skill()`). But there's no way to know if the improvement actually helped. This step closes that gap by:

1. Associating test cases with skills — each skill can have a `tests/` directory with eval cases
2. Running those tests automatically when a skill is updated
3. Comparing scores against the previous version
4. Rolling back the skill if the new version scores worse
5. Logging the diff in `_log.md` for auditability

This is where MOA's learning loop becomes *validated* — not just accumulating knowledge, but proving accumulated knowledge is correct.

---

## 2. Files/directories to read

- **`moa-skills/src/`** — Skill format, `SkillMetadata`, distillation, improvement.
- **`moa-eval/src/`** — `TestSuite`, `TestCase`, `EvalEngine`, `EvalResult` (Steps 42-44).
- **`crates/moa-memory/`** — graph memory fixtures and learning records used by skill evals.
- **`docs/09-skills-and-learning.md`** — Skill lifecycle.

---

## 3. Goal

A skill directory can optionally contain test cases:

```
skills/
└── deploy-to-fly/
    ├── SKILL.md
    ├── scripts/
    │   └── deploy.sh
    └── tests/
        └── suite.toml
```

When `maybe_improve_skill()` generates an updated SKILL.md:

1. Old SKILL.md preserved as `SKILL.md.prev`
2. New version written
3. `tests/suite.toml` run against both old and new versions
4. New version equal or better → keep, log success
5. New version worse → roll back, log regression

---

## 4. Rules

- **Tests are optional.** Skills without `tests/` update without validation (current behavior).
- **Auto-generated skills get auto-generated tests.** `maybe_distill_skill()` creates a minimal test suite from the session events.
- **Rollback is clean.** `SKILL.md.prev` is the safety net. On regression, restore and increment `regression_count` in frontmatter.
- **Tests run with `auto_approve_all = true`.** No approval blocking.
- **Test results logged.** Every improvement attempt logged in `_log.md` with before/after scores.
- **Budget-limited.** Default $0.50 per test run. If exceeded, skip tests and keep new version (optimistic).

---

## 5. Tasks

### 5a. Define skill test suite format

Same TOML format as `moa-eval` suites:

```toml
# skills/deploy-to-fly/tests/suite.toml
[suite]
name = "deploy-to-fly-regression"

[[cases]]
name = "basic-staging-deploy"
input = "Deploy the app to staging"
[cases.expected_output]
contains = ["staging", "deployed"]
expected_trajectory = ["bash", "bash", "bash"]
timeout_seconds = 120
```

### 5b. Modify `maybe_improve_skill()` to run regression tests

Preserve old version → write new → run tests on both → compare → accept or roll back.

### 5c. Auto-generate test suite during skill distillation

Extract from session events: user input, tool sequence, response keywords. Create minimal `tests/suite.toml`.

### 5d. Add `moa eval skill` CLI subcommand

```bash
moa eval skill deploy-to-fly
# Runs tests/suite.toml for the named skill
```

### 5e. Log improvements in `_log.md`

```markdown
## [2026-04-11T10:30:00Z] skill_improvement | deploy-to-fly v1.2 → v1.3
- Test suite: 3 cases
- Old scores: trajectory=1.0, output=0.8, avg=0.9
- New scores: trajectory=1.0, output=1.0, avg=1.0
- Decision: ACCEPTED (+11% improvement)
```

---

## 6. How it should be implemented

```
moa-skills/src/regression.rs    — New: run_skill_tests(), compare_scores(), generate_skill_tests()
moa-skills/src/distiller.rs     — Modified: generate tests alongside skill
moa-skills/src/improver.rs      — Modified: run regression before accepting
moa-cli/src/main.rs             — Add `eval skill` subcommand
```

The skill regression runner reuses `moa-eval`'s engine with a fixed config: the skill under test is the only variable.

---

## 7. Deliverables

- [ ] `moa-skills/src/regression.rs` — Skill regression runner, score comparison, test generation
- [ ] `moa-skills/src/distiller.rs` — Auto-generate `tests/suite.toml` during distillation
- [ ] `moa-skills/src/improver.rs` — Run regression before accepting improvements
- [ ] `moa-cli/src/main.rs` — `moa eval skill <name>` subcommand
- [ ] Example skill test suite in `moa-eval/examples/`

---

## 8. Acceptance criteria

1. Skill with `tests/suite.toml` runs regression tests on improvement attempts.
2. Improvements that score worse are rolled back.
3. Improvements that score equal or better are accepted.
4. `_log.md` records every attempt with scores.
5. `maybe_distill_skill()` creates `tests/suite.toml` for new skills.
6. `moa eval skill deploy-to-fly` runs manually.
7. Skills without tests still improve normally.
8. Test budget respected.

---

## 9. Testing

**Test 1:** `improvement_accepted_when_scores_better` — Old 0.7, new 0.9, verify accepted.
**Test 2:** `improvement_rejected_on_regression` — Old 0.9, new 0.5, verify rolled back.
**Test 3:** `no_tests_means_unconditional_accept` — Skill without tests/, verify accepted.
**Test 4:** `distillation_generates_test_suite` — Distill, verify tests/suite.toml exists and valid.
**Test 5:** `log_entry_written` — Improve, verify _log.md has entry with scores.
**Test 6:** `budget_limit_skips_expensive_tests` — Set budget $0.01, verify skipped.

---

## 10. Additional notes

- **This closes the learning loop.** Without regression testing, skill improvement is optimistic. With it, you verify. Difference between a learning system and a guessing system.
- **Compound effect.** Over months, skills accumulate validated improvements. Each version provably at least as good as previous. Genuine compounding intelligence.
- **Auto-generated tests are a starting point.** Users can edit to add cases, tighten expectations, cover edge cases.
- **Future: community skill registries.** Skills with passing test suites can be shared with confidence. The test suite is the quality certificate.
