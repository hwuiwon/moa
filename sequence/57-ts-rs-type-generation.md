# Step 57 — ts-rs Type Generation

_Add `ts-rs` to all DTO structs and the `StreamEvent` enum. Auto-generate TypeScript bindings from Rust. Delete hand-written `src/lib/types.ts`. Update all frontend imports to use generated bindings._

---

## 1. What this step is about

The hand-written `src/lib/types.ts` has been a source of bugs — field name mismatches, missing `Option` → `null` mappings, and stale types after Rust DTO changes. This step replaces it with **auto-generated TypeScript** from Rust using `ts-rs`. After this step, Rust is the single source of truth for every type that crosses the IPC boundary.

---

## 2. Files/directories to read

Rust side (types to annotate):
- **`src-tauri/src/dto.rs`** — All DTO structs: `RuntimeInfoDto`, `SessionSummaryDto`, `SessionPreviewDto`, `SessionMetaDto`, `EventRecordDto`, `MemorySearchResultDto`, `MemoryRecordSummaryDto`, `MemoryRecordDto`, `MoaConfigDto`, `ModelOptionDto`. Each needs `#[derive(TS)]`.
- **`src-tauri/src/stream.rs`** — `StreamEvent` tagged enum. Needs `#[derive(TS)]`.
- **`src-tauri/src/error.rs`** — `MoaAppError` if it crosses IPC.
- **`src-tauri/Cargo.toml`** — Add `ts-rs` dependency.

Frontend side (files to update/delete):
- **`src/lib/types.ts`** — **DELETE this file entirely.** It's the hand-written copy that drifts.
- **`src/types/chat.ts`** — `StreamEvent` type is hand-written here. Replace with import from generated bindings. Keep the `ContentBlock`, `ChatMessage`, and helper functions (they're frontend-only types).
- **`src/lib/tauri.ts`** — Imports from `@/lib/types`. Update to import from `@/lib/bindings`.
- **`src/hooks/useSessionList.ts`** — May import DTO types.
- **`src/hooks/use-session-meta.ts`** — May import DTO types.
- **`src/hooks/use-memory-pages.ts`** — May import DTO types.
- **`src/components/layout/session-sidebar.tsx`** — Uses `SessionPreviewDto`.
- **`src/components/layout/detail-panel.tsx`** — Uses `SessionMetaDto`.
- **`src/components/layout/top-bar.tsx`** — Uses `ModelOptionDto`, `RuntimeInfoDto`.
- **`src/components/layout/app-layout.tsx`** — Uses DTO types.
- **`src/stores/session.ts`** — May reference types.
- **`src/views/chat-view.tsx`** — Uses `SessionMetaDto`.

Any `.tsx` or `.ts` file that imports from `@/lib/types` must be updated.

---

## 3. Goal

After this step:
1. Running `cargo test -p moa-app` (or the ts-rs export test) generates `.ts` files in `src/lib/bindings/`.
2. `src/lib/types.ts` no longer exists.
3. All frontend code imports DTO types from `@/lib/bindings/` (the generated directory).
4. Adding or removing a field in a Rust DTO is automatically reflected in TypeScript after re-running the export.
5. The `StreamEvent` type in `src/types/chat.ts` is imported from bindings, not hand-written.
6. `cargo tauri dev` works with the generated types.

---

## 4. Rules

- **Use `ts-rs` crate version 10+** (latest stable). Add to `[workspace.dependencies]` in the root `Cargo.toml` and to `src-tauri/Cargo.toml`.
- **Export to `src/lib/bindings/`** via `#[ts(export, export_to = "../../src/lib/bindings/")]` on each struct/enum. This puts generated `.ts` files directly in the frontend source tree.
- **Respect serde attributes.** `ts-rs` reads `#[serde(rename_all = "camelCase")]` and `#[serde(tag = "event", content = "data")]` automatically. The generated TS will use `camelCase` field names and tagged union discriminants.
- **Create a barrel `src/lib/bindings/index.ts`** that re-exports all generated types for clean imports: `import { SessionMetaDto, StreamEvent } from '@/lib/bindings'`.
- **Do NOT generate types for internal Rust types** (e.g., `SessionMeta` or raw graph records). Only DTOs that cross the IPC boundary need `#[derive(TS)]`.
- **Run the export as part of the build.** Add an npm script or Makefile target: `cargo test -p moa-app export_bindings -- --nocapture` that regenerates bindings. Optionally add a `build.rs` or test that auto-exports.
- **Keep `ContentBlock`, `ChatMessage`, and `eventsToMessages` in `src/types/chat.ts`.** These are frontend-only types that don't exist in Rust. They compose the generated DTOs, not replace them. But `StreamEvent` should be imported from bindings.
- **All files remain kebab-case.** Generated files may use PascalCase names (e.g., `SessionMetaDto.ts`) — that's fine for auto-generated code, but the barrel `index.ts` normalizes the import path.

---

## 5. Tasks

### 5a. Add `ts-rs` to workspace and src-tauri

In root `Cargo.toml` workspace dependencies:
```toml
ts-rs = { version = "10", features = ["serde-compat", "chrono-impl", "uuid-impl"] }
```

In `src-tauri/Cargo.toml`:
```toml
ts-rs = { workspace = true }
```

### 5b. Annotate all DTOs in `src-tauri/src/dto.rs`

Add `#[derive(TS)]` and `#[ts(export, export_to = "../../src/lib/bindings/")]` to every struct:

```rust
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub struct SessionMetaDto {
    // ... fields unchanged
}

// Repeat for: RuntimeInfoDto, SessionSummaryDto, SessionPreviewDto,
// EventRecordDto, MemorySearchResultDto, MemoryRecordSummaryDto, MemoryRecordDto,
// MoaConfigDto, ModelOptionDto
```

For `serde_json::Value` fields (like `payload` in `EventRecordDto` and `properties` in `MemoryRecordDto`), ts-rs maps these to `any` or you can override with `#[ts(type = "unknown")]`.

For `HashMap<String, Value>` in `MemoryRecordDto.properties`, use `#[ts(type = "Record<string, unknown>")]`.

### 5c. Annotate `StreamEvent` in `src-tauri/src/stream.rs`

```rust
use ts_rs::TS;

#[derive(Clone, Debug, Serialize, TS)]
#[serde(rename_all = "camelCase", tag = "event", content = "data")]
#[ts(export, export_to = "../../src/lib/bindings/")]
pub enum StreamEvent {
    AssistantStarted,
    AssistantDelta { text: String },
    // ... all variants
}
```

ts-rs respects the `tag`/`content` serde attributes, so the generated TS will be a proper discriminated union.

### 5d. Create a test that exports bindings

In `src-tauri/src/lib.rs` or a dedicated test file:

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn export_bindings() {
        // ts-rs exports happen automatically via #[ts(export)] during test runs.
        // This test exists to give a named target for the export step.
    }
}
```

Run with: `cargo test -p moa-app export_bindings`

### 5e. Create barrel file `src/lib/bindings/index.ts`

After generation, create (or auto-generate) an `index.ts` that re-exports everything:

```typescript
// Auto-generated barrel — re-export all ts-rs bindings
export type { RuntimeInfoDto } from './RuntimeInfoDto';
export type { SessionSummaryDto } from './SessionSummaryDto';
export type { SessionPreviewDto } from './SessionPreviewDto';
export type { SessionMetaDto } from './SessionMetaDto';
export type { EventRecordDto } from './EventRecordDto';
export type { MemorySearchResultDto } from './MemorySearchResultDto';
export type { MemoryRecordSummaryDto } from './MemoryRecordSummaryDto';
export type { MemoryRecordDto } from './MemoryRecordDto';
export type { MoaConfigDto } from './MoaConfigDto';
export type { ModelOptionDto } from './ModelOptionDto';
export type { StreamEvent } from './StreamEvent';
```

### 5f. Delete `src/lib/types.ts`

Remove the file entirely.

### 5g. Update all frontend imports

Find every file that imports from `@/lib/types` and change to `@/lib/bindings`:

```bash
# Find all affected files
grep -r "from.*@/lib/types" src/ --include="*.ts" --include="*.tsx" -l
```

For each file, replace:
```typescript
// Before
import type { SessionMetaDto } from '@/lib/types';

// After
import type { SessionMetaDto } from '@/lib/bindings';
```

### 5h. Update `src/types/chat.ts`

Remove the hand-written `StreamEvent` type. Import from bindings:

```typescript
// Before: hand-written StreamEvent type definition (50+ lines)
// After:
export type { StreamEvent } from '@/lib/bindings';
```

Keep `ContentBlock`, `ChatMessage`, `eventsToMessages`, `normalizeChatMessage`, etc. — these are frontend composition types that don't exist in Rust.

But update `eventsToMessages` to use `EventRecordDto` from bindings instead of the old types import.

### 5i. Add npm script for regeneration

In `package.json`:
```json
{
  "scripts": {
    "generate:types": "cargo test -p moa-app export_bindings -- --nocapture && echo 'Types generated'"
  }
}
```

### 5j. Add `src/lib/bindings/` to `.gitignore` (optional — recommended against)

**Don't gitignore generated bindings.** Commit them so that:
- Frontend devs can work without Rust installed
- CI doesn't need a Rust build step before TS type-checking
- The diff shows when types change

Add a comment at the top of each generated file (ts-rs does this automatically): `// This file was generated by [ts-rs]. Do not edit this file manually.`

---

## 6. How it should be implemented

The key insight: `ts-rs` reads your `#[serde(...)]` attributes and generates TypeScript that matches what `serde_json` actually produces. Since Tauri uses serde for IPC serialization, the generated TS types are **guaranteed to match the runtime JSON shape**. This eliminates the entire class of bugs you experienced.

For the `StreamEvent` tagged enum specifically, `#[serde(tag = "event", content = "data")]` produces:
```typescript
export type StreamEvent =
  | { event: "assistantStarted" }
  | { event: "assistantDelta"; data: { text: string } }
  | { event: "assistantFinished"; data: { text: string } }
  // ... etc
```

Which is exactly the discriminated union the frontend needs for `switch (event.event)`.

---

## 7. Deliverables

- [ ] `ts-rs` added to `Cargo.toml` workspace dependencies
- [ ] `ts-rs` added to `src-tauri/Cargo.toml`
- [ ] `#[derive(TS)]` + `#[ts(export)]` on all 10 DTO structs in `dto.rs`
- [ ] `#[derive(TS)]` + `#[ts(export)]` on `StreamEvent` in `stream.rs`
- [ ] `src/lib/bindings/*.ts` — Generated TypeScript files (committed)
- [ ] `src/lib/bindings/index.ts` — Barrel re-export
- [ ] `src/lib/types.ts` — **DELETED**
- [ ] All `@/lib/types` imports updated to `@/lib/bindings`
- [ ] `src/types/chat.ts` — `StreamEvent` imported from bindings, not hand-written
- [ ] `package.json` — `generate:types` script added
- [ ] Export test in `src-tauri/src/lib.rs`

---

## 8. Acceptance criteria

1. `cargo test -p moa-app` generates `.ts` files in `src/lib/bindings/`.
2. `src/lib/types.ts` does not exist.
3. `npm run build` (TypeScript compilation) succeeds with generated types.
4. `cargo tauri dev` launches and works correctly — no type errors at runtime.
5. Adding a new field to `SessionMetaDto` in Rust → running `cargo test` → the field appears in `src/lib/bindings/SessionMetaDto.ts` → TypeScript sees it.
6. Removing a field from a DTO → running `cargo test` → TypeScript compilation fails if the field was used (caught at compile time, not runtime).
7. `StreamEvent` in `src/types/chat.ts` is a re-export from bindings, not a duplicate definition.
8. No file in `src/` imports from `@/lib/types` (the deleted file).

---

## 9. Testing

**Test 1:** Run `cargo test -p moa-app` → verify `.ts` files appear in `src/lib/bindings/`.
**Test 2:** Run `npm run build` → TypeScript compiles without errors.
**Test 3:** Add a field `pub foo: String` to `SessionMetaDto` in Rust → run `cargo test` → verify `foo: string` appears in the generated TS.
**Test 4:** Remove a field from `SessionMetaDto` that the frontend uses → run `npm run build` → verify TypeScript compilation fails (the whole point).
**Test 5:** Run `cargo tauri dev` → app works correctly, sessions load, streaming works.
**Test 6:** `grep -r "from.*@/lib/types" src/` → returns zero results.

---

## 10. Additional notes

- **Why ts-rs over tauri-specta?** `tauri-specta` generates typed invoke wrappers too, but it's more complex to set up, has a smaller community, and couples you to Tauri-specific tooling. `ts-rs` is simpler (just `#[derive(TS)]`), has 2.5k GitHub stars, and solves the exact problem — type drift. The `tauriClient` wrapper in `src/lib/tauri.ts` remains hand-written but now its type annotations come from generated code.
- **Why not gRPC/protobuf?** Tauri IPC is in-process (no network). gRPC would add HTTP/2 transport, protobuf serialization, and two code generators for data that never leaves the process. The problem is codegen, not transport. ts-rs solves codegen.
- **Regeneration workflow:** When you change a Rust DTO, run `cargo test -p moa-app` and commit the updated bindings. Consider adding a CI check that verifies generated files are up-to-date: run the export, then `git diff --exit-code src/lib/bindings/`.
- **serde_json::Value fields:** ts-rs maps `Value` to `any` by default. Override with `#[ts(type = "unknown")]` for stricter typing, or `#[ts(type = "Record<string, unknown>")]` for object-shaped values.
