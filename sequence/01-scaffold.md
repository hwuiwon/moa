# Step 01: Repository Scaffold + Core Types

## What this step is about

Setting up the Rust workspace, creating all crate directories, defining the core types and trait interfaces that every other crate depends on, and establishing the configuration system.

## Files to read

- `docs/01-architecture-overview.md` — All trait definitions and workspace layout
- `docs/10-technology-stack.md` — Crate dependencies
- `docs/02-brain-orchestration.md` — Configuration format (the `config.toml` section at the bottom)
- `AGENTS.md` — Coding rules and conventions

## Goal

A compilable Rust workspace with 12 crates. `moa-core` contains all shared types, trait definitions, error types, and configuration structures. Running `cargo build` succeeds. Running `cargo test` passes.

## Rules

- Every type that crosses crate boundaries lives in `moa-core`
- All trait definitions from `docs/01-architecture-overview.md` must be implemented exactly as specified
- Use newtypes for IDs: `SessionId(Uuid)`, `UserId(String)`, `WorkspaceId(String)`, `BrainId(Uuid)`
- All newtypes must derive: `Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize`
- Enums must derive: `Debug, Clone, Serialize, Deserialize`
- Use `#[serde(tag = "type", content = "data")]` for the `Event` enum
- Feature flags in workspace root: `default = ["tui"]`, `telegram`, `slack`, `discord`, `cloud`, `temporal`
- Use Rust 2021 edition, resolver = "2"

## Tasks

1. **Create `Cargo.toml` workspace root** with all 12 member crates listed
2. **Create each crate directory** with its own `Cargo.toml` and a minimal `src/lib.rs` (or `src/main.rs` for `moa-cli` and `moa-tui`)
3. **Implement `moa-core/src/types.rs`**: All newtype IDs, `Platform` enum, `SessionStatus` enum, `SandboxTier` enum, `RiskLevel` enum, `ApprovalDecision` enum, `ObserveLevel` enum, `SessionSignal` enum, `ToolCallFormat` enum
4. **Implement `moa-core/src/traits.rs`**: All trait definitions from the architecture doc — `BrainOrchestrator`, `SessionStore`, `HandProvider`, `LLMProvider`, `PlatformAdapter`, `MemoryStore`, `ContextProcessor`, `CredentialVault`
5. **Implement `moa-core/src/events.rs`**: The full `Event` enum with all variants and their payload structs (from `docs/05-session-event-log.md`)
6. **Implement `moa-core/src/config.rs`**: Configuration structures matching the TOML format in `docs/02-brain-orchestration.md`. Use the `config` crate to load from `~/.moa/config.toml` with env var overrides.
7. **Implement `moa-core/src/error.rs`**: A base `MoaError` enum with variants for common failures (SessionNotFound, ProviderError, ConfigError, StorageError, ToolError, etc.)
8. **Wire `moa-core/src/lib.rs`** to re-export all public types

## How to implement

Start with `Cargo.toml` at the workspace root. Define all 12 members. Each crate's `Cargo.toml` should list `moa-core` as a dependency (except `moa-core` itself). Add external dependencies only where needed at this stage:

- `moa-core`: `serde`, `serde_json`, `uuid` (v4), `chrono`, `thiserror`, `async-trait`, `tokio` (features: full), `tracing`, `config`
- All other crates: just `moa-core` as a path dependency for now

For the trait definitions, copy them exactly from `docs/01-architecture-overview.md`. These are the stable interfaces — they should not change after this step.

For the `Event` enum, follow the definition in `docs/05-session-event-log.md`. Each variant carries its data inline. Use `serde_json::Value` for the `ToolCall.input` field.

For config, define a `MoaConfig` struct that deserializes from TOML. It should have sections: `general`, `providers` (anthropic, openai, openrouter), `local`, `cloud`, `gateway`, `tui`, `permissions`. Use `Option<T>` for cloud-only fields. Provide `MoaConfig::load()` that reads from `~/.moa/config.toml` and merges with environment variables.

## Deliverables

```
moa/
├── Cargo.toml                        # workspace root
├── AGENTS.md
├── docs/                             # spec files (00-10)
├── moa-core/
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                    # re-exports
│       ├── types.rs                  # all newtypes and enums
│       ├── traits.rs                 # all trait definitions
│       ├── events.rs                 # Event enum + payloads
│       ├── config.rs                 # MoaConfig + loading
│       └── error.rs                  # MoaError enum
├── moa-brain/
│   ├── Cargo.toml
│   └── src/lib.rs                    # empty, compiles
├── moa-session/
│   ├── Cargo.toml
│   └── src/lib.rs
├── moa-memory/
│   ├── Cargo.toml
│   └── src/lib.rs
├── moa-hands/
│   ├── Cargo.toml
│   └── src/lib.rs
├── moa-providers/
│   ├── Cargo.toml
│   └── src/lib.rs
├── moa-orchestrator/
│   ├── Cargo.toml
│   └── src/lib.rs
├── moa-gateway/
│   ├── Cargo.toml
│   └── src/lib.rs
├── moa-tui/
│   ├── Cargo.toml
│   └── src/main.rs                   # fn main() {} placeholder
├── moa-cli/
│   ├── Cargo.toml
│   └── src/main.rs                   # fn main() {} placeholder
├── moa-security/
│   ├── Cargo.toml
│   └── src/lib.rs
└── moa-skills/
    ├── Cargo.toml
    └── src/lib.rs
```

## Acceptance criteria

1. `cargo build` succeeds with zero errors
2. `cargo clippy` passes with zero warnings
3. `cargo fmt --check` passes
4. `cargo test` passes (at least the unit tests below)
5. All trait definitions from `docs/01-architecture-overview.md` are present and match
6. All `Event` variants from `docs/05-session-event-log.md` are present
7. `MoaConfig::load()` can parse a sample config.toml
8. All types are serializable/deserializable with serde_json

## Tests

### Unit tests in `moa-core/src/types.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_roundtrip() {
        let id = SessionId::new();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn session_status_serialization() {
        let status = SessionStatus::WaitingApproval;
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("WaitingApproval") || json.contains("waiting_approval"));
    }

    #[test]
    fn all_sandbox_tiers_exist() {
        let _ = SandboxTier::None;
        let _ = SandboxTier::Container;
        let _ = SandboxTier::MicroVM;
        let _ = SandboxTier::Local;
    }
}
```

### Unit tests in `moa-core/src/events.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serialization_roundtrip() {
        let event = Event::UserMessage {
            text: "Hello".to_string(),
            attachments: vec![],
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: Event = serde_json::from_str(&json).unwrap();
        // Verify the tag is present
        assert!(json.contains("UserMessage"));
    }

    #[test]
    fn brain_response_event_has_cost_fields() {
        let event = Event::BrainResponse {
            text: "Hi there".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            input_tokens: 100,
            output_tokens: 50,
            cost_cents: 2,
            duration_ms: 1500,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("cost_cents"));
        assert!(json.contains("input_tokens"));
    }

    #[test]
    fn all_event_types_serialize() {
        // Create one of each variant and verify it serializes
        let events = vec![
            Event::SessionCreated { workspace_id: "ws1".into(), user_id: "u1".into(), model: "test".into() },
            Event::UserMessage { text: "hi".into(), attachments: vec![] },
            Event::ToolCall { tool_id: Uuid::new_v4(), tool_name: "bash".into(), input: json!({}), hand_id: None },
            Event::ApprovalRequested { request_id: Uuid::new_v4(), tool_name: "bash".into(), input_summary: "ls".into(), risk_level: RiskLevel::Low },
            Event::Checkpoint { summary: "test".into(), events_summarized: 10, token_count: 500 },
            Event::Error { message: "oops".into(), recoverable: true },
        ];
        for event in events {
            let json = serde_json::to_string(&event);
            assert!(json.is_ok(), "Failed to serialize: {:?}", event);
        }
    }
}
```

### Unit tests in `moa-core/src/config.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn default_config_is_valid() {
        let config = MoaConfig::default();
        assert_eq!(config.general.default_provider, "anthropic");
    }

    #[test]
    fn config_loads_from_toml_string() {
        let toml = r#"
            [general]
            default_provider = "openai"
            default_model = "gpt-4o"
            reasoning_effort = "high"

            [local]
            docker_enabled = false
        "#;
        let config: MoaConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.general.default_provider, "openai");
        assert_eq!(config.local.docker_enabled, false);
    }
}
```

### Verification commands

```bash
cargo build 2>&1 | tail -5          # should end with "Finished"
cargo clippy 2>&1 | grep -c warning  # should be 0
cargo fmt --check                     # should exit 0
cargo test 2>&1 | tail -10           # should show all tests passing
```

## Notes

- Don't add actual implementations of the traits in this step — just the trait definitions. Implementations come in later steps.
- The `async-trait` crate is needed because Rust doesn't natively support async trait methods in all cases yet. Use `#[async_trait]` on all async traits.
- For `moa-tui/src/main.rs` and `moa-cli/src/main.rs`, just put `fn main() {}` — they'll be implemented in later steps.
- Create a sample `config.toml` at `docs/sample-config.toml` for reference.
