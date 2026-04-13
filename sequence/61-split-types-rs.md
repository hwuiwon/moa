# Step 61 — Split `types.rs` into Module Directory + Identifier Macros

_Break the 83KB god file into ~12 focused modules. Eliminate 175 lines of identifier newtype boilerplate with macros._

---

## 1. What this step is about

`moa-core/src/types.rs` is 83KB containing ~100 types with no internal organization. Every contributor touches this file. It causes merge conflicts, is impossible to navigate, and mixes unrelated concerns (identifiers next to completion streams next to platform types).

This step converts it from a single file to a `types/` module directory. Every downstream `use moa_core::SessionId` import continues to work unchanged — we're restructuring internals, not the public API.

---

## 2. Files to read

- **`moa-core/src/types.rs`** — The entire 83KB file. Read it end to end to understand every type it contains.
- **`moa-core/src/lib.rs`** — The massive `pub use types::{...}` re-export list. This is what preserves the public API.
- **`moa-core/src/traits.rs`** — References types from `types.rs` (e.g., `SessionId`, `ToolOutput`). Verify these still resolve.
- **`moa-core/src/events.rs`** — References types from `types.rs`. Verify these still resolve.

---

## 3. Goal

After this step:
1. `moa-core/src/types.rs` no longer exists — replaced by `moa-core/src/types/` directory
2. Each new module file is 100-400 lines
3. `pub use types::*;` in `lib.rs` (or explicit re-exports) preserves the public API
4. `cargo build` passes with zero changes to any other crate
5. Identifier newtypes use `string_id!` / `uuid_id!` macros, eliminating ~175 lines of boilerplate

---

## 4. Rules

- **Public API does not change.** Every `use moa_core::SessionId` in every other crate must continue compiling without modification. This is a purely internal restructuring.
- **No new dependencies.** The macros are `macro_rules!`, not proc macros.
- **Move types, don't rewrite them.** Copy each struct/enum/impl block verbatim into its new module. Do not change field names, types, derives, or method signatures.
- **Tests move with their types.** Any `#[cfg(test)]` block in types.rs that tests a specific type moves to that type's new module.

---

## 5. Tasks

### 5a. Create identifier macros at `moa-core/src/types/macros.rs`

```rust
/// Generates a string-backed newtype identifier with Display, From, Serialize, Deserialize.
macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self { Self(value.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.0.fmt(f) }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self { Self::new(value) }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self { Self::new(value) }
        }
    };
}

/// Generates a UUID-backed newtype identifier with Display, Default, Serialize, Deserialize.
macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub uuid::Uuid);

        impl $name {
            pub fn new() -> Self { Self(uuid::Uuid::now_v7()) }
        }

        impl Default for $name {
            fn default() -> Self { Self::new() }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.0.fmt(f) }
        }
    };
}
```

### 5b. Create `moa-core/src/types/` directory with modules

Delete `moa-core/src/types.rs` and create these files:

```
moa-core/src/types/
├── mod.rs              # pub(crate) macro import, pub mod declarations, pub use re-exports
├── macros.rs           # string_id! and uuid_id! macros (from 5a)
├── identifiers.rs      # SessionId, UserId, WorkspaceId, BrainId, PendingSignalId, MessageId — using macros
├── session.rs          # SessionMeta, SessionSummary, SessionFilter, SessionStatus, SessionSignal, SessionHandle, StartSessionRequest, WakeContext, PendingSignal, PendingSignalId, PendingSignalType, BufferedUserMessage, CheckpointHandle, CheckpointInfo
├── completion.rs       # CompletionRequest, CompletionResponse, CompletionStream, CompletionContent, StopReason, ToolInvocation
├── context.rs          # ContextMessage, WorkingContext, ProcessorOutput, MessageRole, estimate_text_tokens
├── tools.rs            # ToolDefinition, ToolOutput, ToolContent, ToolInputShape, ToolDiffStrategy, ToolPolicySpec, ToolPolicyInput, read_tool_policy, write_tool_policy
├── approval.rs         # ApprovalRequest, ApprovalPrompt, ApprovalDecision, ApprovalRule, ApprovalField, ApprovalFileDiff, PolicyAction, PolicyScope, RiskLevel
├── platform.rs         # Platform, InboundMessage, OutboundMessage, PlatformUser, PlatformCapabilities, ChannelRef, Attachment, ActionButton, ButtonStyle, MessageContent, MessageId, DiffHunk, ToolStatus
├── memory.rs           # MemoryScope, MemoryPath, WikiPage, PageSummary, PageType, ConfidenceLevel, MemorySearchResult, IngestReport, SkillMetadata
├── hands.rs            # HandSpec, HandHandle, HandResources, HandStatus, SandboxTier
├── runtime_events.rs   # RuntimeEvent, ToolUpdate, ToolCardStatus
├── events_stream.rs    # EventRecord, EventStream, EventRange, EventFilter, EventType, ClaimCheck, MaybeBlob, SequenceNum
├── model.rs            # ModelCapabilities, TokenPricing, ProviderNativeTool, ToolCallFormat
├── observability.rs    # TraceContext, generate_trace_tags, trace_name_from_message, truncate_with_ellipsis, sanitize_langfuse_id, normalize_environment
└── scheduling.rs       # CronSpec, CronHandle
```

### 5c. Write `mod.rs` with re-exports

```rust
#[macro_use]
mod macros;

mod approval;
mod completion;
mod context;
mod events_stream;
mod hands;
mod identifiers;
mod memory;
mod model;
mod observability;
mod platform;
mod runtime_events;
mod scheduling;
mod session;
mod tools;

// Re-export everything to preserve the public API
pub use approval::*;
pub use completion::*;
pub use context::*;
pub use events_stream::*;
pub use hands::*;
pub use identifiers::*;
pub use memory::*;
pub use model::*;
pub use observability::*;
pub use platform::*;
pub use runtime_events::*;
pub use scheduling::*;
pub use session::*;
pub use tools::*;
```

### 5d. Update `moa-core/src/lib.rs`

Replace the massive explicit re-export list with:

```rust
pub mod types;
pub use types::*;
```

This preserves `use moa_core::SessionId` for all downstream crates.

### 5e. Move `identifiers.rs` to use macros

```rust
use crate::error::{MoaError, Result};

// String-backed identifiers
string_id!(/// Identifier for a MOA user.
UserId);
string_id!(/// Identifier for a workspace.
WorkspaceId);
string_id!(/// Logical memory wiki path.
MemoryPath);

// UUID-backed identifiers
uuid_id!(/// Identifier for a MOA session.
SessionId);
uuid_id!(/// Identifier for a brain execution instance.
BrainId);
uuid_id!(/// Identifier for a persisted pending session signal.
PendingSignalId);
```

`MessageId` stays as a `string_id!` call too.

### 5f. Move tests

Each `#[cfg(test)] mod tests` block from the original `types.rs` moves into the module that contains the type being tested. For example, `session_id_roundtrip` moves to `identifiers.rs`, `tool_output_text_creates_single_text_block` moves to `tools.rs`, etc.

---

## 6. Deliverables

- [ ] `moa-core/src/types.rs` — **DELETED**
- [ ] `moa-core/src/types/mod.rs` — Module declarations + glob re-exports
- [ ] `moa-core/src/types/macros.rs` — `string_id!` and `uuid_id!` macros
- [ ] `moa-core/src/types/identifiers.rs` — All ID newtypes using macros
- [ ] `moa-core/src/types/session.rs` — Session types
- [ ] `moa-core/src/types/completion.rs` — LLM completion types
- [ ] `moa-core/src/types/context.rs` — Context compilation types
- [ ] `moa-core/src/types/tools.rs` — Tool definition and output types
- [ ] `moa-core/src/types/approval.rs` — Approval flow types
- [ ] `moa-core/src/types/platform.rs` — Platform messaging types
- [ ] `moa-core/src/types/memory.rs` — Memory/wiki types
- [ ] `moa-core/src/types/hands.rs` — Hand provisioning types
- [ ] `moa-core/src/types/runtime_events.rs` — UI runtime event types
- [ ] `moa-core/src/types/events_stream.rs` — Event log types
- [ ] `moa-core/src/types/model.rs` — Model capability types
- [ ] `moa-core/src/types/observability.rs` — Trace context types
- [ ] `moa-core/src/types/scheduling.rs` — Cron types
- [ ] `moa-core/src/lib.rs` — Updated to use `pub use types::*`

---

## 7. Acceptance criteria

1. `cargo build --workspace` compiles with zero errors.
2. `cargo test --workspace` passes — all existing tests still work.
3. `moa-core/src/types.rs` does not exist.
4. No file in `moa-core/src/types/` exceeds 400 lines.
5. No other crate's source files were modified (the public API is identical).
6. `SessionId`, `UserId`, `WorkspaceId`, `BrainId`, `PendingSignalId` use the `uuid_id!`/`string_id!` macros.
7. `grep -r "pub struct SessionId" moa-core/src/types/` returns exactly one result (in `identifiers.rs`, generated by the macro).