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

`cargo-nextest` does not run doctests, so `make test-fast` runs
`cargo test --locked --doc` after the nextest pass. The CI-equivalent local
target keeps running after failures and writes nextest's JUnit report under
`target/nextest/ci/`:

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
| DB memory | `make test-db-memory` | Postgres with AGE/pgvector; currently serial until physical DB isolation lands |
| Authz pentest | `make test-authz-pentest` | Postgres with graph/vector state; writes the pentest report |
| Service E2E | `make test-service-e2e` | clean Postgres/OpenFGA/Restate/PII harness with deterministic providers |
| Provider E2E | `make test-provider-e2e` | service E2E harness plus live/billed provider credentials |

The nextest profiles in `.config/nextest.toml` are mostly suffix-based. Keep
new out-of-line test targets on one of these suffixes so the filters stay short:
`*_unit.rs`, `*_offline.rs`, `*_component.rs`, `*_db.rs`, `*_db_memory.rs`,
`*_service_e2e.rs`, `*_provider_e2e.rs`, `*_eval.rs`, `*_live.rs`, and
`*_docker.rs`. Mixed E2E binaries can use exact module selectors when one file
contains service and provider lanes.

If a crate-private inline unit test needs a slow resource and cannot move to an
integration test without exposing internals, put the lane marker in the test
function name, for example `*_db_*`. Keep these exceptions rare; file suffixes
are the preferred boundary.

Before changing build profiles, linker settings, or crate structure for compile
speed, capture a Cargo timings report:

```bash
make build-timings
```

The report is written under `target/cargo-timings/`.

## Architecture Boundary Check

Run the boundary scanner after touching Restate handlers, workflows, runtime
dependency wiring, or domain repository seams:

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
