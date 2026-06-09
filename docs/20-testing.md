# 20 - Testing

_Provider overrides and snapshot testing conventions._

## Provider Overrides

`MOA_PROVIDERS_OVERRIDE` lets a dev or CI orchestrator replace normal LLM
providers at process startup.

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
