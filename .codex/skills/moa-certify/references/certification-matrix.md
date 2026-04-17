# Certification Matrix

This file is the command map for `moa-certify`. Run the smallest section that still covers the changed surface.

## Prerequisites

- Prefer `PROTOC=/opt/homebrew/bin/protoc` when running `cargo` commands that need protobuf tooling on this machine.
- Temporal tests require the `temporal` CLI in `PATH`.
- Live provider checks require the relevant API keys in the environment.
- Set `MOA_RUN_LIVE_PROVIDER_TESTS=1` for the live orchestrator matrices.

## Baseline Hygiene

For any Rust change:

```bash
cargo fmt --all
PROTOC=/opt/homebrew/bin/protoc cargo clippy -p <touched-crate> --all-targets --all-features -- -D warnings
```

For a pre-release gate or wide cross-crate change:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## Orchestrator, Approval, Lifecycle, Replay

Deterministic:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --test local_orchestrator -- --test-threads=1
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --features temporal --test temporal_orchestrator -- --test-threads=1
```

Manual or ignored Temporal flows when durability or signal behavior changed:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --features temporal --test temporal_orchestrator temporal_orchestrator_runs_workflow_and_unblocks_on_approval -- --ignored --exact --nocapture
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --features temporal --test temporal_orchestrator temporal_orchestrator_soft_cancel_stops_after_current_tool_call -- --ignored --exact --nocapture
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --features temporal --test temporal_orchestrator temporal_orchestrator_recovers_after_worker_process_restart -- --ignored --exact --nocapture
```

Live orchestrator matrix when provider or approval behavior changed:

```bash
MOA_RUN_LIVE_PROVIDER_TESTS=1 PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --test live_provider_roundtrip live_providers_complete_tool_approval_roundtrip_when_available -- --ignored --exact --nocapture
MOA_RUN_LIVE_PROVIDER_TESTS=1 PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --features temporal --test temporal_orchestrator temporal_live_providers_complete_tool_approval_roundtrip_when_available -- --ignored --exact --nocapture
```

Observability audit when traces, cache metrics, or session timing changed:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --test live_observability live_observability_audit_tracks_cache_replay_and_latency -- --ignored --exact --nocapture
```

## Providers, Models, Pricing, Tool Parsing, Web Search

Deterministic:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-providers --lib
```

Live provider matrix:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-providers --test live_provider_matrix live_providers_answer_simple_prompt_across_available_keys -- --ignored --exact --nocapture
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-providers --test live_provider_matrix live_providers_emit_tool_calls_across_available_keys -- --ignored --exact --nocapture
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-providers --test live_provider_matrix live_providers_can_use_native_web_search_across_available_keys -- --ignored --exact --nocapture
```

Provider-specific live smoke when narrowing a failure:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-providers --test anthropic_live -- --ignored --nocapture
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-providers --test openai_live -- --ignored --nocapture
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-providers --test gemini_live -- --ignored --nocapture
```

## Session Store, Event Schema, Analytics, Persistence

Deterministic:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-session --test postgres_store -- --test-threads=1
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-brain --test integration_steps_72_77 -- --test-threads=1
```

If the change affects session-derived analytics or event accounting, also rerun the orchestrator deterministic suites because they exercise persisted session state through real flows.

## Memory And Context Pipeline

Deterministic:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-brain --test brain_turn -- --test-threads=1
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-brain --test stable_prefix -- --test-threads=1
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-memory --test memory_store -- --test-threads=1
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-memory --test maintenance -- --test-threads=1
```

Live cache or live harness verification when prompt layout or cache planning changed:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-brain --test live_harness -- --ignored --nocapture
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-brain --test live_cache_audit -- --ignored --nocapture
```

## Skills And Eval Infrastructure

Deterministic:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-skills --test skills -- --test-threads=1
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-eval -- --test-threads=1
```

If a workspace skill or skill regression suite changed:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo run -p moa-cli -- eval skill <skill-name> --ci
```

## Suggested Release Gate

Use this when the change spans orchestrators, providers, or persistence:

1. `cargo fmt --all`
2. workspace `clippy`
3. `moa-providers --lib`
4. `moa-session --test postgres_store`
5. `moa-orchestrator --test local_orchestrator`
6. `moa-orchestrator --features temporal --test temporal_orchestrator -- --test-threads=1`
7. live provider matrix if provider/envs are available
8. live local + live Temporal orchestrator matrices if orchestrator/provider approval flow changed

