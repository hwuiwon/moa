## Progress

- [x] Reproduced the Applied live e2e failure on the current branch
- [x] Confirmed the merged fixes that already work (`.venv` skip, `AGENTS.md`, approval hygiene)
- [x] Identified the two main remaining regressions: large-file read drift and broken exec interruption handling
- [x] Wrote the implementation plan and e2e retest plan
- [x] Implemented scoped `file_read` and large-file guardrails
- [x] Tightened prompt and tool descriptions around search -> scoped read -> `str_replace` -> verify
- [x] Added clean `Ctrl-C` handling to `moa exec`
- [x] Switched live orchestrator turns to stepwise tool boundaries so turn counting and loop detection can operate during tool-heavy runs
- [x] Ran targeted tests, full changed-crate tests, `cargo fmt`, and `cargo clippy`
- [x] Reran the Applied live e2e and validated the new file-read and cancel behavior
- [x] Iterated on the exec cancel path after the first validation exposed a missing terminal-event fallback
