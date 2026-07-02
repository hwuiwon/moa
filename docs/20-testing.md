# 20 - Testing

_Provider overrides and snapshot testing conventions._

## Provider Overrides

`MOA_PROVIDERS_OVERRIDE` lets a dev or CI orchestrator replace normal LLM
providers at process startup. Build the orchestrator with
`--features provider-overrides` before setting this variable; default production
builds do not compile the scripted provider.

Supported values:

- unset: use providers configured from normal API keys;
- `scripted:<path>`: load deterministic responses from a JSON fixture;
- `mock:<seed>`: use a built-in deterministic mock response.

The override is blocked when the orchestrator detects a production environment
(`prod` or `production` via `MOA_OBSERVABILITY_ENVIRONMENT`).

### Script Format

The fixture supports a fallback response and an optional queue of one-shot
responses:

```json
{
  "default": {
    "completion": {
      "content": "OK",
      "duration_ms": 1,
      "input_tokens": 64,
      "cached_input_tokens": 0,
      "cache_write_input_tokens": 0,
      "tool_calls": []
    }
  },
  "responses": [
    {
      "completion": {
        "content": "first response",
        "tool_calls": []
      }
    }
  ]
}
```

`responses` are consumed in order. After they are exhausted, `default` is used
for every later request. `tool_calls` entries have `name`, `input`, and optional
`id` fields.

The load-test smoke fixture is checked in at
`crates/moa-loadtest/scripts/perf-gate.json`.

## Fast Local Test Runs

Use `cargo-nextest` for the default local suite. It schedules individual tests
across test binaries. The default developer target uses the `fast-pr` nextest
profile, which excludes tests that need Postgres, Docker, Restate, OpenFGA, the
PII sidecar, cloud auth, or live/billed providers.

```bash
cargo install cargo-nextest --locked
make test-fast
```

For the inner loop, `make test-affected` narrows the run further: it maps the
change set (versus the merge base with `main`, plus uncommitted files) to
workspace crates via `cargo metadata`, expands to reverse dependents, and runs
only those crates' tests. Workspace-level files such as `Cargo.lock` fall back
to the full `fast-pr` lane.

`cargo-nextest` does not run doctests. `make test-fast` intentionally skips
them (the workspace currently has no runnable doc examples and the rustdoc
pass costs ~90s); `make test-ci` keeps the doc-test pass as the safety net.
The CI-equivalent local target also keeps running after failures and writes
nextest's JUnit report under `target/nextest/ci/`:

```bash
make test-ci
```

### Test Lanes

The suite is split by runtime requirements rather than by crate. Keep the fast
lane free of hidden service dependencies; move mixed test files into clearer
suffixes as they are touched.

| Lane | Command | Runtime requirements |
| --- | --- | --- |
| Fast PR | `make test-fast` | none beyond local mock servers and tempdirs |
| DB session | `make test-db-session` | Postgres only; schema isolation |
| DB memory | `make test-db-memory` | Postgres with relational graph/vector state; per-test template-cloned databases, runs 4-wide |
| Authz pentest | `make test-authz-pentest` | Postgres with graph/vector state; writes the pentest report |
| Service E2E | `make test-service-e2e` | clean Postgres/OpenFGA/Restate/PII harness with deterministic providers |
| Provider E2E | `make test-provider-e2e` | service E2E harness plus live/billed provider credentials |

The nextest profiles in `.config/nextest.toml` are mostly suffix-based. Keep
new out-of-line test targets on one of these suffixes so the filters stay short:
`*_unit.rs`, `*_offline.rs`, `*_component.rs`, `*_db.rs`, `*_db_memory.rs`,
`*_service_e2e.rs`, `*_provider_e2e.rs`, `*_eval.rs`, `*_live.rs`, and
`*_docker.rs`. When a file starts mixing runtime requirements, split it into
lane-specific binaries before adding profile selectors.

Offline, `_db`, and `_db_memory` behavior files are consolidated into one
harness binary per crate per lane (for example
`crates/moa-orchestrator/tests/orchestrator_db.rs` declaring
`#[path = "orchestrator_db/session_store_db.rs"] mod session_store_db;`).
Each file under `tests/` otherwise links as its own binary, and the binary
count dominates link and nextest-listing time. When adding a behavior file to
one of these lanes, place it under the harness directory and add a `mod` line
to the harness; run it with
`cargo test -p <crate> --test <harness> <module_name>`. Binaries that nextest
profiles, scripts, or workflows reference by name (the `_service_e2e`,
`_provider_e2e`, `_live`, `_eval` lanes and pinned names like
`cross_tenant_pentest_db_memory`) stay standalone. For example, memory
eval corpus and metric tests belong in `_offline` or `_eval` targets, graph
gold-resolution and tenant knowledge graph/vector tests belong in
`_db_memory`, local hand-tool filesystem tests belong in `_offline`,
session-search tests belong in `_db`, Docker hardening belongs in `_docker`,
and Restate service/provider E2E coverage should use surface-specific
`*_service_e2e.rs` or `*_provider_e2e.rs` binaries. If an existing mixed
integration-test binary cannot be split immediately, give the resource-backed
test function the same lane suffix so nextest can keep it out of `fast-pr`.

If a crate-private inline unit test needs a slow resource and cannot move to an
integration test without exposing internals, put the lane marker in the test
function name, for example `*_db_*`. Keep these exceptions rare; file suffixes
are the preferred boundary.

DB-backed lineage and auth recovery checks are explicit integration lanes. Run
them directly when touching those surfaces:

```bash
cargo test -p moa-auth-providers-auth0 --test auth_providers_auth0_db ciba_db --locked -- --test-threads=1
cargo test -p moa-authz --test authz_db authz_poller_db --locked -- --test-threads=1
cargo test -p moa-lineage-audit --test merkle_publisher_db --locked -- --test-threads=1
```

These commands require `MOA_DATABASE_URL` or the local compose Postgres default.
When the database is absent, the lane fails with a Postgres reachability error
instead of silently skipping inside a library unit test.

Before changing build profiles, linker settings, or crate structure for compile
speed, capture a Cargo timings report:

```bash
make build-timings
```

The report is written under `target/cargo-timings/`.

## Architecture Boundary Check

Run the boundary scanner after touching Restate handlers, workflows, runtime
dependency wiring, domain repository seams, workspace crate dependencies,
`moa-core` top-level re-exports, or central hotspot files such as
`crates/moa-edge/src/routes.rs`,
`crates/moa-core/src/config/env_overlay.rs`, and
`crates/moa-orchestrator/src/workflows/turn_execution.rs`:

```bash
cargo run -p xtask -- check-architecture-boundaries
```

The check fails on new direct SQL in
`crates/moa-orchestrator/src/services/**` or
`crates/moa-orchestrator/src/workflows/**`, and on new raw
`OrchestratorCtx` dependency access such as `current_graph_pool()` or
`current_session_store()`. If a handler truly needs a temporary exception,
record it in the scanner allowlist with a concrete reason and exact expected
count. Prefer moving SQL to a repository or domain crate and passing concrete
dependencies from the composition root instead of expanding the allowlist.

The same command also reports and enforces architecture budgets from Cargo
metadata and current source files: workspace package/default-member counts,
`moa-core` direct and transitive reverse dependencies, configured LOC budgets,
forbidden dependency directions from `docs/15-architecture-policy.md`, and the
`moa-core` top-level re-export budget. If one of these numbers grows
intentionally, update the scanner budget in the same change with the measured
count and the reason for accepting the growth.

## Clean E2E Runner

Use the clean runner for certification instead of the persistent compose
Restate and `moa` database. It creates a temporary Postgres database, starts an
ephemeral `restate-server` with random ports, bootstraps OpenFGA into a temp env
file, and cleans those resources up on exit.

```bash
make e2e-clean
```

Ignored/live Restate E2E requires an explicit opt-in:

```bash
MOA_RUN_LIVE_E2E=1 make e2e-clean-live
```

The live Restate lane keeps provider-backed cases out of the default `--live`
path. It uses `moa-orchestrator/provider-overrides` and
`moa-orchestrator/skill-learning` with
`MOA_PROVIDERS_OVERRIDE=mock:<run-id>` for deterministic lifecycle and
skill-learning smoke tests. The clean runner also executes the focused
`skill_learning` orchestrator tests with the feature enabled before the ignored
live profiles, and it unsets provider API-key environment variables around
provider-override smoke tests. Billed provider coverage remains in the separate
provider lane below.

Optional provider and long-eval lanes remain explicit because they can be
billed or slow:

```bash
MOA_RUN_LIVE_E2E=1 MOA_RUN_LIVE_PROVIDER_TESTS=1 make test-provider-e2e

MOA_RUN_LIVE_E2E=1 ./scripts/run-clean-e2e.sh --live --long-eval
```

Postmark messaging e2e coverage is ignored by default and reads local `.env`
values directly. Use `POSTMARK_SERVER_API_TOKEN=POSTMARK_API_TEST` for
non-delivery validation, or provide `POSTMARK_TEST_FROM` and `POSTMARK_TEST_TO`
with a real server token:

```bash
MOA_RUN_LIVE_POSTMARK_TESTS=1 \
cargo test -p moa-messaging --test postmark_provider_e2e --all-features -- --ignored --nocapture
```

The Postmark offline suite covers payload shape, provider status errors,
bounded HTTP 429 retries, exhausted rate-limit failures, and nonzero
`ErrorCode` classification. Contact OTP delivery additionally requires
`MOA_MESSAGING_EMAIL_FROM` to contain a verified sender address. Live Postmark
coverage should remain a single happy-path acceptance check because reproducing
account, suppression, or rate limit failures against the real service is
brittle.

Twilio SMS e2e coverage is also ignored by default and reads local `.env`
values directly. It requires `TWILIO_ACCOUNT_SID`, either `TWILIO_AUTH_TOKEN`
or `TWILIO_API_KEY_SID` plus `TWILIO_API_KEY_SECRET`, and either
`TWILIO_FROM_NUMBER` or `TWILIO_MESSAGING_SERVICE_SID`. Set
`TWILIO_TEST_TO` to the recipient number for a live send; the test skips when
that value is absent:

```bash
MOA_RUN_LIVE_TWILIO_TESTS=1 \
cargo test -p moa-messaging --test twilio_provider_e2e --all-features -- --ignored --nocapture
```

The Twilio live test polls the accepted Message SID until the message reaches
`sent`, `delivered`, or a terminal failure state. Terminal failures include the
Twilio status and error code in the assertion so delivery regressions do not
look like successful provider acceptance.

Slack messaging tests stay offline by default. Unit and integration tests cover
Events API normalization, approval controls, edit fallbacks, per-channel send
pacing, exhausted rate limits, and Slack API error classification; live Slack
coverage should use a separate ignored provider lane once a test workspace and
channel are configured.

Remote loadtest checks are also ignored by default. The step-latency check
requires a running orchestrator with Prometheus metrics enabled:

```bash
MOA_RUN_LOADTEST_REMOTE_SMOKE=1 \
MOA_LOADTEST_METRICS_ENDPOINT=http://localhost:9090/metrics \
cargo test -p moa-loadtest --test mock_loadtest_service_e2e mock_short_profile_reports_runtime_step_latency -- --ignored
```

The runner may start `postgres`, `openfga`, and `moa-pii-service` if compose is
not already running. If it starts compose itself, it stops compose at the end
with volumes preserved.

## Snapshot Testing

Use snapshots when exact rendered output is the contract:

- provider wire-format request bodies;
- byte-stable prompt prefixes;
- rendered UI strings;
- public error message formatting.

Do not snapshot internal struct layouts, incidental debug output, or data that
is easier to assert with targeted semantic checks.

### Adding A Snapshot

Keep fixture inputs fixed, small, and named with constants. Route through the
same formatter or provider request builder used in production, then snapshot
with a stable name in the form `<area>__<scenario>`.

```rust
const SYSTEM_PROMPT: &str = "You are MOA.";

#[test]
fn provider_request_body__minimal_request_serializes_with_stable_byte_layout() {
    let body = build_provider_request_body(SYSTEM_PROMPT)
        .expect("request body should build");

    insta::assert_json_snapshot!("provider_request_body__minimal_request", body, {
        ".metadata.request_id" => "[redacted]",
        ".timestamp" => "[redacted]"
    });
}
```

Use redactions only for genuinely nondeterministic fields such as IDs,
timestamps, or provider-generated cache names. If a test can avoid
nondeterminism with fixed inputs, prefer that over redacting.

### Reviewing Snapshot Diffs

Run the failing test locally, then inspect pending updates with:

```bash
cargo insta review
```

A good diff has an intentional code change next to an expected wire-format or
rendering change. A suspicious diff is only key reordering, whitespace churn, a
changed cache marker location, or an unexpected new ID/timestamp; fix the
determinism issue instead of accepting that snapshot.
