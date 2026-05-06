# Step M01 — Add `MemoryScope::Global` to moa-core; ripple to all match sites

_Add the Global tier so every later prompt can rely on a 3-tier scope (Global → Workspace → User) for routing, RLS policies, and graph node properties._

## 1 What this step is about

Today `moa-core` defines `MemoryScope` as a 2-variant enum (`Workspace(WorkspaceId)` and `User(UserId)`). The graph-primary memory architecture requires a third tier — `Global` — for cross-workspace knowledge (organization-wide conventions, shared concepts, derived facts promoted from many workspaces). This is the smallest mechanical prerequisite for the graph work; we land it first so subsequent prompts can write match arms against the new shape from day one.

This is a breaking change to `moa-core`. Every consumer of `MemoryScope` (`moa-brain`, `moa-runtime`, `moa-skills`, `moa-memory`, `moa-orchestrator`, `moa-cli`) must compile after this prompt — that is the cleanup signal.

## 2 Files to read

- `crates/moa-core/src/lib.rs`
- `crates/moa-core/src/memory/scope.rs` (or wherever `MemoryScope` lives — confirm via `rg "enum MemoryScope" crates/`)
- `crates/moa-core/src/memory/mod.rs`
- All match sites returned by `rg "MemoryScope::" crates/ --type rust`

## 3 Goal

`MemoryScope` becomes a 3-variant enum:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MemoryScope {
    Global,
    Workspace { workspace_id: WorkspaceId },
    User { workspace_id: WorkspaceId, user_id: UserId },
}
```

with helper methods:

```rust
impl MemoryScope {
    pub fn ancestors(&self) -> Vec<MemoryScope>;       // Global first → most specific last
    pub fn workspace_id(&self) -> Option<WorkspaceId>;
    pub fn user_id(&self) -> Option<UserId>;
    pub fn is_global(&self) -> bool;
    pub fn tier(&self) -> ScopeTier;                   // {Global, Workspace, User} for fast matching
}
```

Every consumer compiles. Every existing match on `MemoryScope` either handles `Global` explicitly or has a documented `_` fallback with a `// TODO(M02): handle Global tier` comment.

## 4 Rules

- **No backward compatibility.** Old serialized scope JSON will not deserialize after this change. That is acceptable — local dev DBs are wiped between iterations.
- **`User` carries `workspace_id`.** A user is always scoped within a workspace. There is no global user.
- **`scope.ancestors()` returns Global first.** The retrieval layer (M15) walks it in that order and applies layer-priority bias at the end.
- **`#[non_exhaustive]` is NOT used.** We want compile errors at every match site so we can audit them.
- **Do not change `WorkspaceId` or `UserId` newtypes.**
- **Do not introduce a `From`/`TryFrom` shim** that lets old 2-variant payloads parse. Hard break.

## 5 Tasks

### 5a Update the enum

In `crates/moa-core/src/memory/scope.rs`:

```rust
use serde::{Deserialize, Serialize};
use crate::ids::{UserId, WorkspaceId};

/// Three-tier memory scope. Walked Global → Workspace → User during retrieval.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MemoryScope {
    /// Cross-workspace knowledge. Read by all workspaces; written only by promotion path.
    Global,
    /// Workspace-tenant knowledge. Default scope for ingested facts.
    Workspace { workspace_id: WorkspaceId },
    /// User-personal knowledge inside a workspace.
    User { workspace_id: WorkspaceId, user_id: UserId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeTier { Global, Workspace, User }

impl MemoryScope {
    pub fn ancestors(&self) -> Vec<MemoryScope> {
        match self {
            MemoryScope::Global => vec![MemoryScope::Global],
            MemoryScope::Workspace { workspace_id } => vec![
                MemoryScope::Global,
                MemoryScope::Workspace { workspace_id: workspace_id.clone() },
            ],
            MemoryScope::User { workspace_id, user_id } => vec![
                MemoryScope::Global,
                MemoryScope::Workspace { workspace_id: workspace_id.clone() },
                MemoryScope::User {
                    workspace_id: workspace_id.clone(),
                    user_id: user_id.clone(),
                },
            ],
        }
    }

    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        match self {
            MemoryScope::Global => None,
            MemoryScope::Workspace { workspace_id } => Some(workspace_id.clone()),
            MemoryScope::User { workspace_id, .. } => Some(workspace_id.clone()),
        }
    }

    pub fn user_id(&self) -> Option<UserId> {
        match self {
            MemoryScope::User { user_id, .. } => Some(user_id.clone()),
            _ => None,
        }
    }

    pub fn is_global(&self) -> bool { matches!(self, MemoryScope::Global) }

    pub fn tier(&self) -> ScopeTier {
        match self {
            MemoryScope::Global => ScopeTier::Global,
            MemoryScope::Workspace { .. } => ScopeTier::Workspace,
            MemoryScope::User { .. } => ScopeTier::User,
        }
    }
}
```

### 5b Update `Display`/`FromStr` if implemented

If the old `MemoryScope` had `Display` (e.g., for log lines or graph node properties), implement it for all three variants in canonical form: `"global"`, `"workspace:<uuid>"`, `"user:<workspace_uuid>:<user_uuid>"`. Keep `FromStr` symmetric.

### 5c Audit every match site

Run:

```sh
rg "match .* MemoryScope" crates/ --type rust
rg "MemoryScope::Workspace" crates/ --type rust
rg "MemoryScope::User" crates/ --type rust
```

For every hit:

- If the match handles routing logic → add a `MemoryScope::Global => …` arm with the correct behavior. For most call sites the global behavior is "treat as workspace=null in the query filter" or "skip — no global path supported here yet."
- If the match is exhaustive but the global arm has no obvious behavior → add `MemoryScope::Global => unimplemented!("Global scope not yet supported in this path; see M02")` and a `// TODO(M02)` comment.
- If the call site uses a wildcard `_ =>` → leave it but add a comment confirming the wildcard is intentional.

Common files expected to match:
- `crates/moa-memory/graph` and retrieval callers — add Global handling where graph queries fan out by scope
- `crates/moa-brain/src/pipeline/memory_retriever.rs` — must add Global to retrieval fanout
- `crates/moa-runtime/src/context.rs` — wire Global through scope resolution
- `crates/moa-skills/src/registry.rs` — skills can be global (system-provided)
- `crates/moa-orchestrator/src/session.rs` — session scope resolution

### 5d Update tests

In `crates/moa-core/tests/scope.rs` (create if missing) add:

```rust
#[test]
fn ancestors_global_is_just_global() {
    assert_eq!(MemoryScope::Global.ancestors(), vec![MemoryScope::Global]);
}

#[test]
fn ancestors_workspace_includes_global() {
    let w = WorkspaceId::new();
    let s = MemoryScope::Workspace { workspace_id: w.clone() };
    assert_eq!(s.ancestors(), vec![MemoryScope::Global, s.clone()]);
}

#[test]
fn ancestors_user_is_three_tier() {
    let w = WorkspaceId::new();
    let u = UserId::new();
    let s = MemoryScope::User { workspace_id: w.clone(), user_id: u.clone() };
    let anc = s.ancestors();
    assert_eq!(anc.len(), 3);
    assert!(matches!(anc[0], MemoryScope::Global));
    assert_eq!(anc[2], s);
}

#[test]
fn serde_round_trip_all_three() {
    for s in [
        MemoryScope::Global,
        MemoryScope::Workspace { workspace_id: WorkspaceId::new() },
        MemoryScope::User { workspace_id: WorkspaceId::new(), user_id: UserId::new() },
    ] {
        let j = serde_json::to_string(&s).unwrap();
        let r: MemoryScope = serde_json::from_str(&j).unwrap();
        assert_eq!(s, r);
    }
}
```

### 5e Update doc comments

Anywhere `MemoryScope` appears in rustdoc, update to mention three tiers.

## 6 Deliverables

- `crates/moa-core/src/memory/scope.rs` — new enum + helpers (~120 lines).
- `crates/moa-core/tests/scope.rs` — round-trip + ancestor tests.
- Touched match sites across `moa-brain`, `moa-runtime`, `moa-skills`, `moa-memory`, `moa-orchestrator`, `moa-cli`.

## 7 Acceptance criteria

1. `cargo build --workspace` is clean (no warnings beyond existing baseline).
2. `cargo test -p moa-core` passes; all four new tests green.
3. `rg "MemoryScope::Workspace\(" crates/` returns ZERO hits — old tuple-variant syntax is gone.
4. Every existing match on `MemoryScope` either handles `Global` explicitly or has a `TODO(M02)` comment.
5. `serde_json::to_string(&MemoryScope::Global) == r#"{"kind":"global"}"#`.

## 8 Tests

```sh
cargo build --workspace
cargo test -p moa-core
rg "MemoryScope::Workspace\(" crates/   # must be empty
rg "TODO\(M02\)" crates/                # spot-check the deferred sites
```

## 9 Cleanup

- **Delete the old 2-variant enum definition** entirely. There is no `#[deprecated]` shim. The compiler must catch every consumer.
- **Remove any old `MemoryScope::Workspace(uuid)` tuple-variant constructors** in test fixtures or doc examples. Migrate to struct-variant syntax `MemoryScope::Workspace { workspace_id: uuid }`.
- **Delete any `From<WorkspaceId> for MemoryScope` impl** that returned the old `Workspace(_)` variant. Replace with explicit construction at call sites.

## 10 What's next

**M02 — 3-tier RLS + GUC discipline in pgvector schema and `moa-runtime`**. Now that the type system knows about Global, the database needs to enforce it.
