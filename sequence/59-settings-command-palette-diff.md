# Step 59 — Settings Panel + Command Palette + Diff Viewer

_Searchable settings with ArkType validation. Cmd+K palette using shadcn Command + prompt-kit PromptSuggestion. Diff viewer with prompt-kit CodeBlock._

---

## 1. What this step is about

Three features: settings panel, command palette (Cmd+K), and diff viewer.

---

## 2. Files/directories to read

- **`moa-core/src/config.rs`** — `MoaConfig` struct with all sections.
- **`src-tauri/src/commands.rs`** — `get_config`, `update_config`.
- **`src-tauri/src/dto.rs`** — `MoaConfigDto`. If you need to extend it (e.g., add fields for the settings panel to display), add the field in Rust with `#[derive(TS)]`, run `cargo test -p moa-app` to regenerate, then use the updated type in the frontend.
- **`src/lib/bindings/`** — **Generated types from Step 57.** Import `MoaConfigDto`, `ModelOptionDto`, `SessionSummaryDto` from here.
- **`src/components/ui/command.tsx`** — shadcn Command (wraps cmdk, already installed).
- **`src/router.tsx`** — TanStack Router.

Prompt-kit components to use:
- **`src/components/prompt-kit/code-block.tsx`** — For diff syntax highlighting.
- **`src/components/prompt-kit/prompt-suggestion.tsx`** — For empty command palette state.

shadcn/ui: `Command` (cmdk), `Dialog`, `Input`, `Select`, `Switch`, `Slider`, `Card`, `Button`, `ToggleGroup`, `Collapsible`.

---

## 3. Rules

- **All files kebab-case.**
- **No react-resizable-panels.** CSS flex layouts only.
- **Use TanStack Router** for navigation.
- **Use ArkType for validation.** Install `arktype` + `@hookform/resolvers`.
- **Import DTO types from `@/lib/bindings`** (ts-rs generated). If you need a new field on `MoaConfigDto`, add it in Rust first, regenerate, then use it.
- **Use existing shadcn Command component** — wraps cmdk, already installed.
- **Use prompt-kit CodeBlock** for diff content.
- **Use prompt-kit PromptSuggestion** for empty palette state.

---

## 4. Tasks

### Settings Panel

#### 4a. Create `settings-view.tsx`

Two-column flex layout:
```tsx
<div className="flex h-full">
  <nav className="w-[200px] shrink-0 border-r border-border p-4">{/* Category list */}</nav>
  <div className="flex-1 overflow-y-auto p-6">{/* Active category form */}</div>
</div>
```

Categories: General, Providers, Tools & MCP, Approval Rules, Memory, Appearance, Advanced.

#### 4b. Build forms with React Hook Form + ArkType

```typescript
import { type } from 'arktype';
import type { MoaConfigDto } from '@/lib/bindings';

const generalSettingsSchema = type({
  'model': 'string',
  'reasoningEffort?': '"low" | "medium" | "high"',
});
```

Load current config via `invoke<MoaConfigDto>('get_config')`.

### Command Palette

#### 4c. Create `command-palette.tsx`

Replace placeholder in `app-layout.tsx` with shadcn `CommandDialog`:

```tsx
import { CommandDialog, CommandEmpty, CommandGroup, CommandInput,
  CommandItem, CommandList, CommandShortcut } from '@/components/ui/command';
import { PromptSuggestion } from '@/components/prompt-kit/prompt-suggestion';
import type { SessionSummaryDto } from '@/lib/bindings';
```

When input is empty, show `<PromptSuggestion>` chips. Dynamically populate session group from `invoke<SessionSummaryDto[]>('list_sessions')`.

#### 4d. Create `src/lib/command-actions.ts`

### Diff Viewer

#### 4e. Create `diff-viewer.tsx`

Use `react-diff-viewer-continued` + prompt-kit `<CodeBlock>`. Toggle unified/split (shadcn `ToggleGroup`). Per-hunk Accept/Reject.

#### 4f. Integrate diff into tool cards from Step 55

---

## 5. Deliverables

- [ ] `src/views/settings-view.tsx`
- [ ] `src/components/settings/general-settings.tsx` through `advanced-settings.tsx`
- [ ] `src/components/command-palette.tsx` — shadcn Command + prompt-kit PromptSuggestion
- [ ] `src/lib/command-actions.ts`
- [ ] `src/components/chat/diff-viewer.tsx` — prompt-kit CodeBlock
- [ ] `src/hooks/use-config.ts` — uses `MoaConfigDto` from `@/lib/bindings`
- [ ] Dependencies: `react-hook-form`, `@hookform/resolvers`, `arktype`, `react-diff-viewer-continued`

---

## 6. Acceptance criteria

1. Settings panel editable with ArkType validation.
2. Config types come from `@/lib/bindings` (generated), not hand-written.
3. Cmd+K opens command palette.
4. Empty palette shows PromptSuggestion chips.
5. Fuzzy search matches actions and sessions.
6. Diff viewer renders with CodeBlock highlighting.
7. Unified/split toggle works.
