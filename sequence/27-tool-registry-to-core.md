# Step 27: Tool Registry Trait Promotion to Core

## What this step is about
The `ToolRegistry`, `ToolDefinition`, `BuiltInTool` trait, and related types currently live in `moa-hands/src/router.rs`. This means:
- `moa-brain` depends on the concrete `ToolRouter` from `moa-hands` to get tool schemas for pipeline stage 3
- The brain harness depends on `moa-hands` for execution, which is correct, but also for *type definitions*, which is a layering violation
- Future MCP discovery, cloud hand providers, and policy engines will all need to reference tool definitions without pulling in `moa-hands`

This step promotes the tool registry *interface* (trait + shared types) into `moa-core`, while the concrete `ToolRegistry` implementation and `ToolRouter` remain in `moa-hands`.

## Files to read
- `moa-hands/src/router.rs` — `BuiltInTool`, `ToolContext`, `ToolDefinition`, `ToolExecution`, `ToolRegistry`, `ToolRouter`, `ToolPolicySpec`, `ToolInputShape`, `ToolDiffStrategy`
- `moa-core/src/traits.rs` — all existing traits
- `moa-core/src/types.rs` — all existing shared types
- `moa-brain/src/harness.rs` — how the brain uses tools
- `moa-brain/src/pipeline/tools.rs` — stage 3 uses tool schemas
- `moa-hands/src/lib.rs` — current public re-exports
- `moa-security/src/policies.rs` — policy checking uses tool definitions

## Goal
`moa-core` defines what a tool *is* (trait + metadata types). `moa-hands` defines how tools are *organized and executed* (registry impl + router). Any crate can depend on `moa-core` to reference tool definitions without depending on `moa-hands`.

## Rules
- Move only the **interface types** to `moa-core`. The concrete `ToolRegistry` (HashMap-backed), `ToolRouter`, and execution logic stay in `moa-hands`.
- Do NOT over-abstract. The goal is to lift the types that other crates already need, not to create a new abstraction layer.
- Maintain backward compatibility: `moa-hands` should re-export anything that moved so existing `use moa_hands::...` paths still work (with a deprecation notice if desired).
- This is a refactor — no new functionality, no behavior changes.

## Tasks

### 1. Identify what moves to `moa-core`

**Move to `moa-core/src/types.rs`** (these are pure data types):
- `ToolDefinition` — tool name, description, schema, risk level, approval requirement
- `ToolInputShape` — enum for policy matching (Path, Command, Query, Custom)
- `ToolDiffStrategy` — enum for diff display (None, FileDiff, ContentDiff)
- `ToolPolicySpec` — struct combining risk level, input shape, diff strategy
- `read_tool_policy()` and `write_tool_policy()` — helper constructors for common policy specs

**Move to `moa-core/src/traits.rs`** (these are trait interfaces):
- `BuiltInTool` — the trait that individual tools implement (`name`, `description`, `input_schema`, `execute`, `policy_spec`)
- `ToolContext` — the context struct passed to tool `execute()` (holds references to memory store, session meta, etc.)

**Stay in `moa-hands/src/router.rs`**:
- `ToolExecution` enum (BuiltIn/Hand/Mcp — this is routing logic)
- `ToolRegistry` struct (the concrete HashMap-backed registry)
- `ToolRouter` struct (the execution router)
- All registration methods (`register_builtin`, `register_hand_tools`, etc.)

### 2. Move types to `moa-core/src/types.rs`
Add `ToolDefinition`, `ToolInputShape`, `ToolDiffStrategy`, `ToolPolicySpec` and the helper constructors. Make sure they derive `Debug, Clone, Serialize, Deserialize` as appropriate.

### 3. Move `BuiltInTool` trait and `ToolContext` to `moa-core/src/traits.rs`
`BuiltInTool` should use `#[async_trait]` (consistent with other traits in core). `ToolContext` needs to reference `MemoryStore` and `SessionMeta` which are already in core — verify no circular dependencies.

### 4. Update `moa-core/src/lib.rs` exports
Add the new types and trait to the public exports.

### 5. Update `moa-hands/src/router.rs`
- Remove the moved types and import them from `moa_core` instead.
- Keep `ToolExecution`, `ToolRegistry`, `ToolRouter` in place.
- Re-export the moved types from `moa-hands/src/lib.rs` for backward compatibility:
  ```rust
  // Backward compat re-exports (from moa-core)
  pub use moa_core::{BuiltInTool, ToolContext, ToolDefinition, ToolInputShape, ToolDiffStrategy, ToolPolicySpec};
  ```

### 6. Update `moa-hands/src/tools/*.rs`
These tools implement `BuiltInTool`. Update their imports from `crate::router::BuiltInTool` to `moa_core::BuiltInTool` (or keep using the re-export — either works).

### 7. Update `moa-brain` imports
If `moa-brain` currently imports tool types from `moa-hands`, switch those to `moa-core`. Check:
- `moa-brain/src/harness.rs`
- `moa-brain/src/pipeline/tools.rs`
- `moa-brain/Cargo.toml` — verify it already depends on `moa-core` (it should)

### 8. Update `moa-security` imports
If `moa-security/src/policies.rs` references `ToolPolicySpec`, `ToolInputShape`, or `RiskLevel` from `moa-hands`, switch to `moa-core`.

### 9. Update tests
Any test that constructs `ToolDefinition`, `ToolPolicySpec`, or implements `BuiltInTool` — update imports. No logic changes needed.

## Deliverables
```
moa-core/src/types.rs       # + ToolDefinition, ToolInputShape, ToolDiffStrategy, ToolPolicySpec
moa-core/src/traits.rs      # + BuiltInTool trait, ToolContext struct
moa-core/src/lib.rs          # Updated exports
moa-hands/src/router.rs     # Imports from moa-core, keeps ToolExecution/Registry/Router
moa-hands/src/lib.rs         # Re-exports for backward compat
moa-hands/src/tools/*.rs    # Updated imports
moa-brain/src/harness.rs    # Updated imports (if applicable)
moa-brain/src/pipeline/tools.rs # Updated imports (if applicable)
moa-security/src/policies.rs # Updated imports (if applicable)
```

## Acceptance criteria
1. `BuiltInTool` trait is defined in `moa-core/src/traits.rs`.
2. `ToolDefinition`, `ToolPolicySpec`, `ToolInputShape`, `ToolDiffStrategy` are defined in `moa-core/src/types.rs`.
3. `ToolContext` is defined in `moa-core` (traits or types — whichever is cleaner).
4. `ToolExecution`, `ToolRegistry`, `ToolRouter` remain in `moa-hands/src/router.rs`.
5. `moa-hands` re-exports the moved types for backward compat.
6. No crate other than `moa-hands` needs to depend on `moa-hands` just to reference tool type definitions.
7. All existing tests pass with no logic changes.
8. `cargo build --workspace` compiles cleanly.

## Tests
This is a pure refactor — no new tests needed. All existing tests must pass:

```bash
cargo test --workspace
```

If any test fails, it's an import or re-export issue, not a logic bug. Fix the imports.

**Verification checklist** (manual):
- `grep -rn "use moa_hands.*ToolDefinition\|use moa_hands.*BuiltInTool\|use moa_hands.*ToolContext\|use moa_hands.*ToolPolicySpec" moa-brain/ moa-security/ moa-orchestrator/ moa-gateway/` → should return zero results (these crates should import from `moa-core`, not `moa-hands`)
- `grep -rn "use moa_core.*ToolDefinition\|use moa_core.*BuiltInTool" moa-hands/src/tools/` → should show the tools importing from core (or via re-export)
