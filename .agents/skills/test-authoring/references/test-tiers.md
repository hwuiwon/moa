# Test Tiers

Decision tree for which kind of test to write, with examples grounded in the MOA workspace.

## The Five Tiers

| Tier | Lives in | When | Cost |
|---|---|---|---|
| Unit | inline `#[cfg(test)] mod tests` | Pure function, small algorithm, crate-internal helper | Milliseconds |
| Integration | `crates/<name>/tests/<topic>.rs` | Spans modules, exercises public API, needs fixtures | Hundreds of ms to seconds |
| Snapshot | `crates/<name>/tests/<topic>.rs` with `insta` | Large structured output meant to be byte-stable | Milliseconds, but reviews cost human time |
| Live | `crates/<name>/tests/<provider>_live.rs` with `#[ignore]` + env flag | Behavior depends on paid external API or running infra | Seconds, plus dollar cost |
| Eval scenario | `crates/moa-eval/scenarios/` | End-to-end multi-turn conversation behavior | Tens of seconds; nightly runs |

## When To Use Each

### Unit

Default for any function that takes inputs and returns outputs deterministically. Examples:

- `parse_and_match_bash` from `moa-security` — pure parser; unit test with a corpus of strings.
- A new `truncate_head_tail_lines` helper in `moa-core` — unit test with boundary inputs.
- A small state-machine transition function — unit test the transition table.

If the SUT depends on Postgres, a real LLM, or the filesystem, this is not the right tier.

### Integration

Use when the test must:

- exercise a public API of the crate
- coordinate multiple modules
- use fixtures from `moa-test-support` or `wiremock`
- talk to a real Postgres instance (with `MOA_DATABASE_URL` set, `#[ignore]` if unset)

Examples already in the repo:

- `crates/moa-brain/tests/brain_turn_artifacts_db.rs` for turn lifecycle with artifact-backed outputs.
- `crates/moa-orchestrator/tests/orchestrator_offline/session_vo.rs` for Restate virtual-object behavior.
- `crates/moa-session/tests/postgres_store_db.rs` for session store behavior.
- `crates/moa-providers/tests/providers_offline/anthropic_offline.rs` for wiremock-backed provider behavior.

One file per topic. A 2,000-line `tests.rs` is not an integration test file; it is a placeholder for someone to split it up. Offline/`_db`/`_db_memory` behavior files live as modules inside one per-lane harness binary per crate (for example `tests/orchestrator_offline.rs` declaring `#[path = "orchestrator_offline/session_vo.rs"] mod session_vo;`) so each new file does not add a new link target; add the `mod` line to the matching harness when creating a file.

### Snapshot

Use when the assertion is "this big structured output looks right" and the output is large enough that hand-coding the equality is impractical. Examples:

- The exact bytes of a provider request body (Anthropic with cache_control markers, OpenAI Responses envelope).
- A rendered Slack Block Kit JSON for an approval prompt.
- The compiled context that the brain harness sends to the LLM.

Do not use snapshots for:

- Tests of internal struct layouts (couples to implementation; refactors break it for no reason).
- Outputs that contain timestamps, UUIDs, or other non-deterministic fields without using `insta` redactions to neutralize them.

### Live

Use when:

- the SUT calls a real external API (Anthropic, OpenAI, Cohere, Gemini, Turbopuffer)
- behavior depends on running infra (PII sidecar, Postgres extensions)
- offline counterparts (wiremock-based) cannot reproduce a specific upstream behavior

The double-gate pattern (`#[ignore]` + env flag) is mandatory. The flag conventions are in `certify`'s test matrix; do not invent new flags.

Always pair a live test with an offline counterpart when possible. The offline test runs in PR CI; the live test runs nightly or before release.

### Eval Scenario

Use when:

- the behavior only emerges in multi-turn conversations
- you want to assert end-to-end functional correctness, latency, cost, cache stability, or memory recall together
- the test is expected to be authoritative for production behavior

Eval scenarios live in `crates/moa-eval/scenarios/long_conversation/<name>/` if that directory exists in the repo, with their own 5-file pattern (scenario.toml, transcript.jsonl, goal_card.md, expectations.toml, README.md). If the eval-scenarios pattern does not yet exist for the change you are making, file the scenario as a follow-up rather than authoring an ad-hoc multi-turn test in another tier.

## Anti-Pattern: Mixing Tiers

A common mistake is writing an integration test that should be a unit test, or a unit test that should be an integration test.

- If you are mocking the LLM, a real Postgres, AND the filesystem, you are probably testing the wrong layer; either drop down to a unit test of the pure logic or up to an integration test that uses the real components.
- If your unit test needs `tokio::test` and a multi-second timeout, it is probably an integration test in the wrong location.
- If your integration test asserts on the internal field order of a private struct, it is probably a unit test that should be inline next to the struct.
