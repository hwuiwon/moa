## Live Retest Summary

Workspace: `~/github/applied`

Prompt:

> Take a look at the CallViewset in the server/ and please simplify and refactor it. make sure everything works

## What Improved

1. Search exclusions worked in practice.
   The run did not walk `server/.venv`.
2. Workspace instructions were discovered and loaded.
   The run read `AGENTS.md` very early.
3. Approval hygiene improved.
   No broad `zsh *` or similar persistent approval rules were created.

## What Still Failed

1. The model still performs too much exploration before editing.
   After the fixes, it now uses bounded `file_read` windows instead of a blind whole-file dump, but it can still spend a long time inspecting neighboring helpers and patterns before committing to an edit.
2. The model still did not reach a surgical edit in the live Applied run.
   `str_replace` was available but not used.
3. The model still did not complete verification in-session.
   It never reached `pytest`, `py_compile`, or `ruff check` from the live MOA run.
4. The original interrupt bug is fixed.
   A later live rerun showed that interrupting `moa exec` now moves the session to `cancelled` and the CLI process exits cleanly.

## Likely Root Causes

1. The lack of scoped reads was a major cause of drift and has now been fixed.
2. The remaining gap is mostly model-side exploration strategy: it still wants more surrounding context than is necessary before editing.
3. Live sessions previously hid long tool chains inside one assistant turn.
   The orchestrator now steps at tool boundaries so turn budgets and loop detection can operate during tool-heavy runs.

## Files Most Likely To Change

1. `moa-hands/src/tools/file_read.rs`
2. `moa-hands/src/router/registration.rs`
3. `moa-brain/src/pipeline/identity.rs`
4. `moa-cli/src/exec.rs`
5. Relevant tests in `moa-hands/tests/`, `moa-brain`, and possibly `moa-cli`
