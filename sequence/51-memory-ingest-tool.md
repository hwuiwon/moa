# Step 51 — Memory Ingest Tool

_Wire `FileMemoryStore::ingest_source()` as a built-in brain tool and CLI command. Users can say "read this document and add it to the knowledge base."_

---

## 1. What this step is about

MOA has a fully working `ingest_source()` function in `moa-memory/src/ingest.rs` that takes a raw document, creates a summary page in `sources/`, extracts entities/topics/decisions, creates or updates related wiki pages, updates the index, and logs everything. It's tested and working.

But there's no way for a user or the brain to call it. This step wires it as:

1. **A built-in brain tool** — the brain can call `memory_ingest` during a session to incorporate a document into workspace knowledge
2. **A CLI command** — `moa memory ingest <file>` for batch ingestion from the terminal
3. **File attachment handling** — when a user attaches a file to a message (in TUI or messaging), the brain can ingest it

---

## 2. Files/directories to read

- **`moa-memory/src/ingest.rs`** — The existing `ingest_source()` implementation. Already handles summary creation, entity/topic/decision extraction, wiki page upserts, index updates, and logging. This is the backend — fully implemented.
- **`moa-memory/src/lib.rs`** — `FileMemoryStore::ingest_source()` public method (line ~141). Delegates to `ingest::ingest_source()`.
- **`moa-hands/src/tools/memory.rs`** — Existing `memory_read` and `memory_write` built-in tools. The `memory_ingest` tool follows the same pattern.
- **`moa-hands/src/router.rs`** — Where built-in tools are registered. Add `memory_ingest` here.
- **`moa-cli/src/main.rs`** — `MemoryCommand` enum. Add `Ingest` subcommand.
- **`moa-core/src/traits.rs`** — `ToolContext` struct. The ingest tool needs access to the memory store through this context.
- **`moa-core/src/types.rs`** — `ToolOutput`, `Attachment`. The tool needs to handle both inline text content and file paths.

---

## 3. Goal

Three usage patterns work:

**Brain-initiated ingest during a session:**
```
User: Here's our API design doc. Add it to the project knowledge base.
Agent: [calls memory_ingest with the document content]
Agent: Done — I've ingested "API Design Doc" into workspace memory. 
       Created: sources/api-design-doc.md
       Updated: entities/auth-service.md, topics/api-conventions.md
       Extracted 2 entities, 1 topic, 1 decision.
```

**CLI ingest:**
```bash
moa memory ingest docs/rfc-0042.md --name "RFC 0042 Auth Redesign"
# Ingested "RFC 0042 Auth Redesign"
# Created: sources/rfc-0042-auth-redesign.md
# Updated: entities/auth-service.md, topics/token-rotation.md
# Extracted: 1 entity, 1 topic, 1 decision
```

**Batch ingest:**
```bash
moa memory ingest docs/*.md
# Ingested 5 documents into workspace memory
```

---

## 4. Rules

- **Use the existing `ingest_source()` function.** Do not reimplement ingestion logic. The tool and CLI command are thin wrappers.
- **Ingest is a workspace-scoped operation.** Documents are ingested into the current workspace's memory, not user memory.
- **The brain decides when to ingest.** The tool is available like any other — the brain calls it when the user asks to incorporate knowledge. The brain should not auto-ingest every file it reads.
- **Source name is required.** Every ingested document needs a human-readable name for the `sources/` page title. The tool should derive a reasonable name from the file path or first heading if not explicitly provided.
- **Large documents are handled.** The ingest function should work with documents up to ~100KB. For larger documents, truncate to the first 100KB with a note that the document was truncated.
- **The tool returns a structured summary.** Not just "done" — list what was created/updated, what entities/topics/decisions were extracted, and any contradictions detected.
- **`memory_ingest` is in the default tool loadout.** It's always available alongside `memory_read` and `memory_write`.
- **No approval needed.** Ingest writes to memory, which is a low-risk operation (reversible, no side effects outside MOA).

---

## 5. Tasks

### 5a. Create `MemoryIngestTool` in `moa-hands/src/tools/memory.rs`

Add alongside the existing `MemoryReadTool` and `MemoryWriteTool`:

```rust
pub struct MemoryIngestTool;

#[async_trait]
impl BuiltInTool for MemoryIngestTool {
    fn name(&self) -> &'static str { "memory_ingest" }
    
    fn description(&self) -> &'static str {
        "Ingest a source document into workspace memory. Creates a summary page \
         in sources/, extracts entities, topics, and decisions into separate \
         wiki pages, and updates the workspace index. Use this when the user \
         provides a document, RFC, design doc, or reference material that should \
         become part of the project's knowledge base."
    }
    
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source_name": {
                    "type": "string",
                    "description": "Human-readable name for the source (e.g., 'RFC 0042 Auth Redesign'). Used as the wiki page title."
                },
                "content": {
                    "type": "string",
                    "description": "The full text content of the document to ingest."
                }
            },
            "required": ["source_name", "content"]
        })
    }
    
    fn policy_spec(&self) -> ToolPolicySpec {
        ToolPolicySpec {
            risk_level: RiskLevel::Low,
            default_action: PolicyAction::Allow,  // No approval needed
            input_shape: ToolInputShape::Json,
            diff_strategy: ToolDiffStrategy::None,
        }
    }
    
    async fn execute(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let source_name = input["source_name"]
            .as_str()
            .ok_or(MoaError::InvalidInput("source_name is required".into()))?;
        let content = input["content"]
            .as_str()
            .ok_or(MoaError::InvalidInput("content is required".into()))?;
        
        // Truncate very large documents
        let content = if content.len() > 100_000 {
            let truncated = &content[..100_000];
            format!("{}\n\n[Document truncated at 100KB. Original size: {} bytes]", 
                truncated, content.len())
        } else {
            content.to_string()
        };
        
        let scope = MemoryScope::Workspace(ctx.session.workspace_id.clone());
        
        // Call the existing ingest function
        // Note: ingest_source expects a &FileMemoryStore, not &dyn MemoryStore.
        // We need to downcast or restructure. See implementation notes.
        let report = ingest_source_via_trait(
            ctx.memory_store, &scope, source_name, &content
        ).await?;
        
        // Format the result
        let mut output = format!("Ingested \"{}\" into workspace memory.\n\n", source_name);
        output.push_str(&format!("Created: {}\n", report.source_path.as_str()));
        
        if report.affected_pages.len() > 1 {
            output.push_str("\nUpdated pages:\n");
            for page in &report.affected_pages[1..] {
                output.push_str(&format!("  - {}\n", page.as_str()));
            }
        }
        
        if !report.contradictions.is_empty() {
            output.push_str("\n⚠️ Contradictions detected:\n");
            for c in &report.contradictions {
                output.push_str(&format!("  - {}\n", c));
            }
        }
        
        Ok(ToolOutput::text(output))
    }
}
```

### 5b. Register `memory_ingest` in the tool router

In `moa-hands/src/router.rs`, add alongside the existing memory tools:

```rust
registry.register_builtin(Arc::new(memory::MemoryIngestTool));
```

Add to the default loadout:
```rust
registry.default_loadout = vec![
    "memory_read".to_string(),
    "memory_write".to_string(),
    "memory_ingest".to_string(),
    // ... other tools ...
];
```

### 5c. Handle the `MemoryStore` trait vs `FileMemoryStore` concrete type

The existing `ingest_source()` function in `moa-memory` takes a `&FileMemoryStore` (concrete type), but the `ToolContext` only has `&dyn MemoryStore` (trait object). Two approaches:

**Option A: Add `ingest_source()` to the `MemoryStore` trait.** This is the cleanest approach — add a default method that returns `Unsupported` and override it in `FileMemoryStore`.

**Option B: Implement ingest using only `MemoryStore` trait methods.** The ingest function uses `write_page()`, `read_page()`, `list_pages()`, and `get_index()` — all of which are on the trait. Rewrite `ingest_source()` to use trait methods instead of `FileMemoryStore`-specific methods like `refresh_scope_index()` and `append_scope_log()`.

**Recommendation: Option A** — extend the `MemoryStore` trait. Add:

```rust
#[async_trait]
pub trait MemoryStore: Send + Sync {
    // ... existing methods ...
    
    /// Ingest a raw source document into the wiki.
    /// Default implementation returns Unsupported.
    async fn ingest_source(
        &self,
        scope: MemoryScope,
        source_name: &str,
        content: &str,
    ) -> Result<IngestReport> {
        Err(MoaError::Unsupported("ingest_source not supported by this memory store".into()))
    }
}
```

Then `FileMemoryStore` overrides it by delegating to the existing `ingest::ingest_source()`.

### 5d. Add `moa memory ingest` CLI subcommand

In `moa-cli/src/main.rs`, extend `MemoryCommand`:

```rust
#[derive(Debug, Subcommand)]
enum MemoryCommand {
    Search { query: String },
    Show { path: String },
    // New:
    Ingest(IngestArgs),
}

#[derive(Debug, Args)]
struct IngestArgs {
    /// File path(s) to ingest. Supports globs.
    #[arg(required = true)]
    files: Vec<PathBuf>,
    
    /// Source name override. If not set, derived from filename.
    #[arg(long)]
    name: Option<String>,
    
    /// Workspace to ingest into. Defaults to current directory.
    #[arg(long)]
    workspace: Option<String>,
}
```

Implementation:

```rust
async fn handle_memory_ingest(args: IngestArgs, config: MoaConfig) -> Result<()> {
    let memory = create_memory_store(&config).await?;
    let workspace_id = resolve_workspace(&args.workspace, &config)?;
    let scope = MemoryScope::Workspace(workspace_id);
    
    for file_path in &args.files {
        let content = tokio::fs::read_to_string(file_path).await
            .context(format!("Failed to read {}", file_path.display()))?;
        
        let source_name = args.name.as_deref()
            .unwrap_or_else(|| file_path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unnamed"));
        
        let report = memory.ingest_source(scope.clone(), source_name, &content).await?;
        
        println!("✅ Ingested \"{}\"", source_name);
        println!("   Created: {}", report.source_path.as_str());
        println!("   Updated: {} pages", report.affected_pages.len() - 1);
        
        if !report.contradictions.is_empty() {
            println!("   ⚠️  {} contradictions detected", report.contradictions.len());
        }
    }
    
    Ok(())
}
```

### 5e. Handle file attachments in the brain harness

When the user attaches a file (via TUI paste or messaging attachment), the brain should recognize this as potential ingest material. This doesn't require harness changes — the brain already receives attachment content in `UserMessage.attachments`. The system prompt or skills can instruct the brain to offer ingestion when attachments are present.

Add a line to the identity system prompt (pipeline stage 1):

```
When the user provides a document or reference material and asks you to 
remember it or add it to the knowledge base, use the memory_ingest tool 
to incorporate it into workspace memory.
```

### 5f. Emit `MemoryIngest` event

Add a new event variant for tracking ingest operations in the session log:

```rust
Event::MemoryIngest {
    source_name: String,
    source_path: String,
    affected_pages: Vec<String>,
    contradictions: Vec<String>,
}
```

The brain tool emits this after successful ingest so the session log records what was ingested.

---

## 6. How it should be implemented

This is primarily a wiring step — the core logic exists in `moa-memory/src/ingest.rs`. The work is:

1. Expose `ingest_source()` through the `MemoryStore` trait (5 lines of trait code + 5 lines of impl)
2. Create `MemoryIngestTool` (follows the exact pattern of `MemoryReadTool` — 60 lines)
3. Register in router (2 lines)
4. Add CLI subcommand (30 lines of CLI + 20 lines of handler)
5. Add event variant (5 lines)

Total: ~120 lines of new code, mostly boilerplate that follows existing patterns.

---

## 7. Deliverables

- [ ] `moa-core/src/traits.rs` — `ingest_source()` default method on `MemoryStore` trait
- [ ] `moa-memory/src/lib.rs` — `FileMemoryStore::ingest_source()` override
- [ ] `moa-hands/src/tools/memory.rs` — `MemoryIngestTool` implementation
- [ ] `moa-hands/src/router.rs` — Register `memory_ingest` in default tools and loadout
- [ ] `moa-core/src/events.rs` — `Event::MemoryIngest` variant
- [ ] `moa-cli/src/main.rs` — `moa memory ingest` subcommand with file handling

---

## 8. Acceptance criteria

1. **Brain can ingest.** Ask "add this document to the knowledge base" with inline content → brain calls `memory_ingest` → new pages appear in workspace memory.
2. **CLI can ingest.** `moa memory ingest docs/rfc.md` → creates source page + derived pages.
3. **Batch CLI ingest.** `moa memory ingest docs/*.md` → ingests all files.
4. **Ingest result is informative.** Tool output lists created/updated pages, extracted entities, and contradictions.
5. **Event logged.** Session log contains `MemoryIngest` event after tool call.
6. **Large documents handled.** A 200KB document is truncated to 100KB with a note.
7. **Source name derived.** If `--name` not provided, the source name comes from the filename.
8. **Memory index updated.** After ingest, `moa memory search "auth"` finds content from the ingested document.
9. **No approval needed.** The tool auto-approves (low risk, no side effects outside MOA).

---

## 9. Testing

**Test 1:** `ingest_tool_creates_source_page` — Call MemoryIngestTool with sample content, verify source page exists in memory.

**Test 2:** `ingest_tool_extracts_entities` — Provide content with `## Entities\n- Auth Service`, verify `entities/auth-service.md` created.

**Test 3:** `ingest_tool_returns_report` — Verify ToolOutput contains created/updated page paths.

**Test 4:** `ingest_tool_truncates_large_content` — Provide 200KB content, verify truncation note in stored page.

**Test 5:** `cli_ingest_single_file` — Run CLI with one file, verify source page and derived pages.

**Test 6:** `cli_ingest_derives_name_from_filename` — Run without `--name`, verify source name matches filename.

**Test 7:** `ingest_event_emitted` — Run ingest via brain tool in a session, verify `MemoryIngest` event in session log.

**Test 8:** `memory_search_finds_ingested_content` — Ingest a document about "OAuth tokens", search for "OAuth", verify result found.

---

## 10. Additional notes

- **Current limitation.** The existing `ingest_source()` uses a simple heuristic parser for entities/topics/decisions (looking for markdown sections with those headings). It doesn't use the LLM for extraction. This is fine for structured documents but weak for free-form text. A future improvement could use the LLM to extract entities/decisions from unstructured content — but that's a separate step.
- **Ingest vs. memory_write.** `memory_write` creates/updates a single page. `memory_ingest` processes a source document into multiple pages (summary + entities + topics + decisions). They're complementary — the brain uses `memory_write` for targeted updates and `memory_ingest` for bulk knowledge incorporation.
- **Contradiction detection.** The current implementation extracts contradictions from a `## Contradictions` section in the source. Future work: cross-reference ingested content against existing memory to detect actual contradictions (e.g., "the source says port 3000 but memory says port 8080").
- **Re-ingest handling.** If the same source is ingested twice, the existing implementation updates pages rather than duplicating them. The source page gets overwritten; derived pages get additional `## Source update` sections appended.
