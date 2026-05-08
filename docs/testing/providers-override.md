# Provider Overrides For Tests

`MOA_PROVIDERS_OVERRIDE` lets a dev or CI orchestrator replace normal LLM
providers at process startup.

Supported values:

- unset: use providers configured from normal API keys.
- `scripted:<path>`: load deterministic responses from a JSON fixture.
- `mock:<seed>`: use a built-in deterministic mock response.

The override is blocked when the orchestrator detects a production environment
(`prod` or `production` via `observability.environment`, `DEPLOY_ENV`,
`MOA_ENVIRONMENT`, or `MOA__ENVIRONMENT`).

## Script Format

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
