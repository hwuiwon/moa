## Applied Retest Plan

### Objective

Re-run the same Applied task after the fixes and verify that the session follows a narrow, surgical workflow and cleans up properly when interrupted.

### Preconditions

1. Build the CLI:
   `cargo build -p moa-cli`
2. Run targeted tests for the changed crates.
3. Confirm the Applied workspace is clean:
   `git -C ~/github/applied status --short`
4. Confirm the backend container is healthy:
   `docker ps --format '{{.Names}} {{.Status}}' | grep 'applied-backend-1\\|applied-db-1'`
5. Smoke-check the known test path:
   `docker exec applied-backend-1 pytest -vv tests/views/test_call_view.py -k 'twilio_voice_outbound' --no-header -q`

### Live Run

Run from `~/github/applied`:

`~/github/moa/target/debug/moa exec 'Take a look at the CallViewset in the server/ and please simplify and refactor it. make sure everything works'`

### What To Watch For

1. The run should avoid `.venv`.
2. The run should search first, then read a bounded file range instead of reading the full `server/core/views.py`.
3. The run should prefer `str_replace` for edits to existing files.
4. The run should attempt verification after edits.
5. If interrupted with `Ctrl-C`, the session should cleanly move to `cancelled` instead of remaining `running` or `waiting_approval`.

### Post-Run Validation

1. Inspect the diff in `~/github/applied`.
2. Run syntax and lint checks on touched Python files.
3. Run the focused pytest target.
4. Inspect recent session rows:
   `sqlite3 ~/.moa/sessions.db "SELECT id, status, event_count FROM sessions ORDER BY created_at DESC LIMIT 5;"`
5. Inspect approval rules:
   `sqlite3 ~/.moa/sessions.db "SELECT tool, pattern, action FROM approval_rules WHERE workspace_id='applied';"`

### Pass Criteria

1. The agent narrows on the relevant code without a whole-file dump of the large view file.
2. The edit path uses `str_replace` or another surgical write path instead of bash text rewriting.
3. Verification is attempted in-session.
4. No broad approval rule is persisted.
5. If the session is interrupted, it does not remain stuck in `running` or `waiting_approval`.
