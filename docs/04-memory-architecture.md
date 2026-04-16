# 04 — Memory Architecture

_File-wiki + FTS5, per-user + per-workspace scoping, consolidation, concurrent writes._

---

## Design principles

1. **Files are the source of truth.** Markdown on disk. No database is authoritative.
2. **Memory compounds.** Each session should leave the knowledge base richer.
3. **Zero-dependency locally.** FTS5 is the only index, derived from files, rebuildable.
4. **Inspectable and editable.** Users can browse, edit, and delete memory in any editor.
5. **Separate scopes compose at runtime.** User knowledge + workspace knowledge.

---

## Scoping

### User memory

Location: `~/.moa/memory/` (local) | `users/{user_id}/memory/` (cloud)

Contents: personal preferences, cross-project learnings, corrections, habits, communication style, timezone, language.

Travels with the user across all workspaces. Each user has their own memory — no sharing.

### Workspace memory

Location: `~/.moa/workspaces/{workspace_id}/memory/` (local) | `workspaces/{workspace_id}/memory/` (cloud)

Contents: project architecture, conventions, domain knowledge, decisions, entity pages, skills.

Shared by all users in the workspace.

### Runtime composition

At session start:
```
User MEMORY.md (≤200 lines)  → loaded into context
Workspace MEMORY.md (≤200 lines) → loaded into context
= ~8K tokens maximum base memory load
```

User memory takes precedence on conflicts.

---

## File structure

```
memory/
├── MEMORY.md              # Index (≤200 lines, ≤25KB). Loaded every session.
├── _schema.md             # Wiki conventions for this scope
├── _log.md                # Append-only chronological change log
│
├── topics/                # Conceptual pages
│   ├── architecture.md
│   ├── testing.md
│   └── ...
│
├── entities/              # Concrete things (services, tools, APIs, people)
│   ├── auth-service.md
│   ├── stripe-api.md
│   └── ...
│
├── decisions/             # Timestamped decision records
│   ├── 2026-03-15-switch-to-fastify.md
│   └── ...
│
├── skills/                # Distilled procedures from successful runs
│   ├── debug-memory-leaks.md
│   ├── deploy-to-fly.md
│   └── ...
│
└── sources/               # Summaries of ingested raw materials
    ├── rfc-0042-auth-redesign.md
    └── ...
```

### Page format

```markdown
---
type: topic           # topic | entity | decision | skill | source
created: 2026-04-09T14:30:00Z
updated: 2026-04-09T16:45:00Z
confidence: high      # high | medium | low
related:              # explicit cross-references (the file graph)
  - entities/auth-service.md
  - decisions/2026-03-15-switch-to-fastify.md
sources:              # provenance
  - sources/rfc-0042-auth-redesign.md
tags: [security, api, auth]
auto_generated: false # true if distilled from a run
last_referenced: 2026-04-09T16:00:00Z
reference_count: 7
---

# Authentication Architecture

The auth system uses JWT with rotating refresh tokens. Access tokens expire
after 15 minutes. Refresh tokens are single-use with a 30-day lifetime.

## Token flow
1. Client sends credentials to /auth/login
2. Server returns { access_token, refresh_token }
3. Client includes access_token in Authorization header
4. On 401, client sends refresh_token to /auth/refresh
5. Server invalidates old refresh_token, issues new pair

## Key decisions
- [[2026-03-15-switch-to-fastify]]: Moved from Express to Fastify for performance
- Chose JWT over session cookies for stateless horizontal scaling

## Known issues
- Refresh token rotation has a race condition under concurrent requests (see #142)

## Cross-references
- See [[auth-service]] for implementation details
- See [[stripe-api]] for payment auth integration
```

### MEMORY.md (index)

```markdown
# Workspace: webapp

Quick reference for the webapp project. This file is loaded at every session start.

## Critical commands
- `npm run dev` — start dev server (port 3000)
- `npm test` — run test suite
- `npm run deploy:staging` — deploy to staging via Fly.io

## Architecture summary
Fastify API + React SPA + PostgreSQL. Auth via JWT with refresh tokens.
See [[topics/architecture]] for full details.

## Active decisions
- [[decisions/2026-04-16-postgres-everywhere]]: Moving all persistence to Postgres
- [[decisions/2026-03-15-switch-to-fastify]]: Completed migration from Express

## Key entities
- [[entities/auth-service]]: JWT auth with refresh tokens
- [[entities/stripe-api]]: Payment integration
- [[entities/postgres]]: Primary database

## Recent skills
- [[skills/deploy-to-fly]]: Staging + production deploy procedure
- [[skills/debug-memory-leaks]]: Node.js heap analysis workflow

## Topics index
| Topic | Last updated | Confidence |
|-------|-------------|------------|
| [[topics/architecture]] | 2026-04-09 | high |
| [[topics/testing]] | 2026-04-05 | medium |
| [[topics/deployment]] | 2026-04-01 | high |
```

### _log.md (change log)

```markdown
## [2026-04-09T16:45:00Z] memory_write | Updated auth architecture
- Updated: topics/architecture.md (added race condition note)
- Updated: MEMORY.md (updated topics index)
- Brain: session-abc123, turn 7

## [2026-04-09T14:30:00Z] ingest | RFC-0042 Auth Redesign
- Created: sources/rfc-0042-auth-redesign.md
- Updated: topics/architecture.md (added token flow section)
- Updated: entities/auth-service.md (revised endpoint docs)
- Created: decisions/2026-04-09-adopt-single-use-refresh.md
- Updated: MEMORY.md (added new decision)
- Brain: session-def456, turn 3

## [2026-04-09T10:00:00Z] consolidation | Dream cycle
- Pruned: 3 stale entries from topics/testing.md
- Resolved: contradiction between architecture.md and deployment.md on port numbers
- Normalized: 5 relative dates to absolute
- MEMORY.md: 187 → 162 lines
```

---

## Operations

### Session start (automatic)

```rust
async fn load_session_memory(
    user_id: &UserId,
    workspace_id: &WorkspaceId,
    memory: &dyn MemoryStore,
) -> Result<Vec<ContextMessage>> {
    let user_index = memory.get_index(MemoryScope::User(user_id.clone())).await?;
    let workspace_index = memory.get_index(MemoryScope::Workspace(workspace_id.clone())).await?;
    
    // Truncate each to 200 lines / 25KB
    let user_index = truncate_index(&user_index, 200, 25_000);
    let workspace_index = truncate_index(&workspace_index, 200, 25_000);
    
    Ok(vec![
        ContextMessage::system(format!(
            "<user_memory>\n{}\n</user_memory>",
            user_index
        )),
        ContextMessage::system(format!(
            "<workspace_memory>\n{}\n</workspace_memory>",
            workspace_index
        )),
    ])
}
```

### On-demand search (tool call)

The brain calls `memory_search` as a tool when it needs more context:

```rust
pub struct MemorySearchTool;

impl Tool for MemorySearchTool {
    fn name(&self) -> &str { "memory_search" }
    
    fn schema(&self) -> ToolSchema {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search terms" },
                "scope": { "type": "string", "enum": ["user", "workspace", "both"] },
                "type_filter": { "type": "string", "enum": ["topic", "entity", "decision", "skill", "source"] },
                "limit": { "type": "integer", "default": 5 }
            },
            "required": ["query"]
        })
    }
    
    async fn execute(&self, input: &str, ctx: &ToolContext) -> Result<String> {
        let params: SearchParams = serde_json::from_str(input)?;
        let results = ctx.memory.search(
            &params.query,
            params.scope.into_memory_scope(ctx.user_id, ctx.workspace_id),
            params.limit.unwrap_or(5),
        ).await?;
        
        // Return formatted results with snippets
        let mut output = String::new();
        for r in results {
            output.push_str(&format!(
                "## {} ({})\nConfidence: {} | Updated: {}\n{}\n\n",
                r.title, r.path, r.confidence, r.updated, r.snippet
            ));
        }
        Ok(output)
    }
}
```

### Memory write (during session)

Three triggers:

**1. Correction capture**: User corrects the agent → agent writes to relevant page.

**2. Discovery filing**: Agent discovers something worth remembering → creates/updates wiki page.

**3. Skill distillation**: After successful multi-step run (≥5 tool calls) → creates skill page.

```rust
pub struct MemoryWriteTool;

impl Tool for MemoryWriteTool {
    fn name(&self) -> &str { "memory_write" }
    
    async fn execute(&self, input: &str, ctx: &ToolContext) -> Result<String> {
        let params: WriteParams = serde_json::from_str(input)?;
        
        // Parse or create wiki page
        let mut page = if ctx.memory.page_exists(&params.path).await? {
            ctx.memory.read_page(&params.path).await?
        } else {
            WikiPage::new(params.page_type, &params.title)
        };
        
        // Update content
        page.update_content(&params.content);
        page.update_frontmatter("updated", Utc::now());
        
        // Update cross-references
        for related in &params.related {
            page.add_related(related);
        }
        
        // Write page
        ctx.memory.write_page(&params.path, page).await?;
        
        // Update MEMORY.md index
        update_index(ctx.memory, ctx.workspace_id, &params.path, &params.title).await?;
        
        // Append to _log.md
        append_log(ctx.memory, ctx.workspace_id, &LogEntry {
            timestamp: Utc::now(),
            operation: "memory_write",
            description: format!("Updated {}", params.path),
            affected_pages: vec![params.path.clone()],
            brain_session: ctx.session_id,
        }).await?;
        
        Ok(format!("Memory updated: {}", params.path))
    }
}
```

### Ingest (source compilation)

When user provides a new source document:

```rust
async fn ingest_source(
    memory: &dyn MemoryStore,
    llm: &dyn LLMProvider,
    workspace_id: &WorkspaceId,
    source: &str,          // raw source content
    source_name: &str,     // e.g., "RFC-0042"
) -> Result<IngestReport> {
    let scope = MemoryScope::Workspace(workspace_id.clone());
    let mut affected_pages = Vec::new();
    
    // 1. Generate summary page
    let summary = llm.complete(CompletionRequest::new(
        format!("Summarize this source for a wiki page. Extract key facts, decisions, and entities.\n\nSource:\n{}", source)
    )).await?;
    
    let summary_path = format!("sources/{}.md", slugify(source_name));
    let summary_page = WikiPage::new_source(source_name, &summary.text);
    memory.write_page(&summary_path.into(), summary_page).await?;
    affected_pages.push(summary_path);
    
    // 2. Extract entities and update existing pages
    let extraction = llm.complete(CompletionRequest::new(
        format!("Given this source and the existing wiki index, identify:\n\
                 1. Entities mentioned (services, tools, APIs, people)\n\
                 2. Topics that need updating\n\
                 3. Decisions that were made\n\
                 4. Contradictions with existing knowledge\n\
                 \nExisting index:\n{}\n\nSource:\n{}",
                 memory.get_index(scope.clone()).await?,
                 source)
    )).await?;
    
    // 3. Apply updates to existing pages (parsed from LLM response)
    let updates = parse_wiki_updates(&extraction.text)?;
    for update in updates {
        apply_wiki_update(memory, &scope, &update).await?;
        affected_pages.push(update.path);
    }
    
    // 4. Update index
    update_index_after_ingest(memory, workspace_id, &affected_pages).await?;
    
    // 5. Log
    append_log(memory, workspace_id, &LogEntry {
        timestamp: Utc::now(),
        operation: "ingest",
        description: format!("Ingested: {}", source_name),
        affected_pages: affected_pages.clone(),
        brain_session: SessionId::system(),
    }).await?;
    
    Ok(IngestReport { source_name: source_name.to_string(), affected_pages })
}
```

---

## Consolidation ("Dream")

### Trigger conditions

Both must be true:
- ≥3 sessions completed since last consolidation
- ≥24 hours since last consolidation

### Process

Runs as a scheduled brain task (Temporal timer or local cron):

```rust
async fn run_consolidation(
    memory: &dyn MemoryStore,
    llm: &dyn LLMProvider,
    scope: MemoryScope,
) -> Result<ConsolidationReport> {
    let all_pages = memory.list_pages(scope.clone(), None).await?;
    let index = memory.get_index(scope.clone()).await?;
    let log = memory.read_page(&"_log.md".into()).await?;
    
    let prompt = format!(
        "You are maintaining a knowledge wiki. Review the following pages and perform:\n\
         1. TEMPORAL NORMALIZATION: Convert relative dates to absolute\n\
         2. CONTRADICTION RESOLUTION: If pages disagree, update the wrong one\n\
         3. STALE PRUNING: Remove entries about deleted files, completed tasks, outdated info\n\
         4. DEDUPLICATION: Merge overlapping entries\n\
         5. ORPHAN DETECTION: Flag pages with no inbound references\n\
         6. CONFIDENCE DECAY: Lower confidence on unreferenced entries older than 30 days\n\
         7. INDEX MAINTENANCE: Keep MEMORY.md under 200 lines\n\
         \nCurrent index:\n{}\n\nRecent log:\n{}\n\nPage summaries:\n{}",
        index,
        last_n_lines(&log.content, 50),
        format_page_summaries(&all_pages)
    );
    
    let result = llm.complete(CompletionRequest::new(prompt)).await?;
    let actions = parse_consolidation_actions(&result.text)?;
    
    let mut report = ConsolidationReport::new();
    
    for action in actions {
        match action {
            ConsolidationAction::UpdatePage { path, new_content } => {
                memory.write_page(&path, WikiPage::from_content(&new_content)).await?;
                report.pages_updated += 1;
            }
            ConsolidationAction::DeletePage { path, reason } => {
                memory.delete_page(&path).await?;
                report.pages_deleted += 1;
                report.deletions.push((path, reason));
            }
            ConsolidationAction::FlagOrphan { path } => {
                report.orphans.push(path);
            }
            ConsolidationAction::UpdateIndex { new_index } => {
                memory.write_page(&"MEMORY.md".into(), WikiPage::index(&new_index)).await?;
                report.index_updated = true;
            }
        }
    }
    
    Ok(report)
}
```

---

## Concurrent writes (git-branch model)

### Problem

In cloud mode, multiple brains may write to the same workspace memory simultaneously.

### Solution

Each brain writes to a named branch. A reconciler merges periodically.

```
memory/                         # main branch (source of truth)
memory/.branches/
├── brain-abc123/              # brain A's pending writes
│   ├── topics/architecture.md  # modified
│   └── _changes.json          # manifest of changes
├── brain-def456/              # brain B's pending writes
│   └── ...
```

### Write flow

```rust
impl FileMemoryStore {
    async fn write_page_branched(
        &self,
        brain_id: &BrainId,
        path: &MemoryPath,
        page: WikiPage,
    ) -> Result<()> {
        // Write to branch directory instead of main
        let branch_path = self.branch_dir(brain_id).join(path);
        write_file(&branch_path, &page.serialize()).await?;
        
        // Record in change manifest
        self.append_change_manifest(brain_id, ChangeRecord {
            path: path.clone(),
            operation: ChangeOp::Write,
            timestamp: Utc::now(),
        }).await?;
        
        Ok(())
    }
}
```

### Reconciliation (LLM-powered merge)

Runs as a cron job (every 15 minutes or after each session completes):

```rust
async fn reconcile_branches(
    memory: &FileMemoryStore,
    llm: &dyn LLMProvider,
    scope: MemoryScope,
) -> Result<ReconcileReport> {
    let branches = memory.list_branches(scope.clone()).await?;
    if branches.is_empty() { return Ok(ReconcileReport::empty()); }
    
    let mut report = ReconcileReport::new();
    
    for branch in branches {
        let changes = memory.read_change_manifest(&branch).await?;
        
        for change in changes {
            let branch_content = memory.read_branch_page(&branch, &change.path).await?;
            let main_content = memory.read_page(&change.path).await.ok();
            
            if let Some(main) = main_content {
                if main.updated > branch_content.updated {
                    // Main was updated after branch wrote — conflict
                    let resolved = resolve_conflict(llm, &main, &branch_content).await?;
                    memory.write_page(&change.path, resolved).await?;
                    report.conflicts_resolved += 1;
                } else {
                    // No conflict — branch is newer
                    memory.write_page(&change.path, branch_content).await?;
                    report.pages_merged += 1;
                }
            } else {
                // New page — no conflict possible
                memory.write_page(&change.path, branch_content).await?;
                report.pages_created += 1;
            }
        }
        
        // Clean up branch
        memory.delete_branch(&branch).await?;
    }
    
    Ok(report)
}

async fn resolve_conflict(
    llm: &dyn LLMProvider,
    main: &WikiPage,
    branch: &WikiPage,
) -> Result<WikiPage> {
    let prompt = format!(
        "Two versions of a wiki page need to be merged. Combine the information, \
         keeping the most accurate and complete version. Resolve contradictions \
         by preferring the more recent or more specific information.\n\n\
         Version A (main):\n{}\n\nVersion B (branch):\n{}\n\n\
         Output the merged page in the same format.",
        main.serialize(), branch.serialize()
    );
    
    let result = llm.complete(CompletionRequest::new(prompt)).await?;
    WikiPage::parse(&result.text)
}
```

---

## FTS5 search index

### Schema

```sql
CREATE VIRTUAL TABLE wiki_search USING fts5(
    path,
    scope,          -- 'user:{id}' or 'workspace:{id}'
    title,
    page_type,      -- topic, entity, decision, skill, source
    tags,
    content,
    tokenize='porter unicode61'
);

-- Metadata table for non-FTS queries
CREATE TABLE wiki_pages (
    path TEXT NOT NULL,
    scope TEXT NOT NULL,
    title TEXT,
    page_type TEXT,
    confidence TEXT,
    created TEXT,
    updated TEXT,
    last_referenced TEXT,
    reference_count INTEGER DEFAULT 0,
    PRIMARY KEY (scope, path)
);
```

### Rebuild

```rust
async fn rebuild_fts_index(memory: &FileMemoryStore, scope: &MemoryScope) -> Result<()> {
    let db = memory.search_db();
    let scope_key = scope.to_key();
    
    // Clear existing entries for this scope
    sqlx::query("DELETE FROM wiki_search WHERE scope = ?")
        .bind(&scope_key)
        .execute(db).await?;
    
    sqlx::query("DELETE FROM wiki_pages WHERE scope = ?")
        .bind(&scope_key)
        .execute(db).await?;
    
    // Walk all markdown files in the scope directory
    let pages = memory.list_all_files(scope).await?;
    
    for page_path in pages {
        let page = memory.read_page(&page_path).await?;
        let fm = page.frontmatter();
        
        sqlx::query(
            "INSERT INTO wiki_search (path, scope, title, page_type, tags, content) \
             VALUES (?, ?, ?, ?, ?, ?)"
        )
        .bind(page_path.as_str())
        .bind(&scope_key)
        .bind(&fm.title())
        .bind(&fm.page_type())
        .bind(&fm.tags_joined())
        .bind(&page.body())
        .execute(db).await?;
        
        sqlx::query(
            "INSERT INTO wiki_pages (path, scope, title, page_type, confidence, created, updated, last_referenced, reference_count) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(page_path.as_str())
        .bind(&scope_key)
        .bind(&fm.title())
        .bind(&fm.page_type())
        .bind(&fm.confidence())
        .bind(&fm.created())
        .bind(&fm.updated())
        .bind(&fm.last_referenced())
        .bind(fm.reference_count())
        .execute(db).await?;
    }
    
    Ok(())
}
```

### Search with ranking

```rust
async fn search_memory(
    db: &PgPool,
    query: &str,
    scope: &str,
    limit: usize,
) -> Result<Vec<MemorySearchResult>> {
    let results = sqlx::query_as::<_, MemorySearchResult>(
        "SELECT 
            ws.path,
            ws.title,
            ws.page_type,
            snippet(wiki_search, 5, '<mark>', '</mark>', '...', 40) as snippet,
            wp.confidence,
            wp.updated,
            wp.reference_count,
            rank
         FROM wiki_search ws
         JOIN wiki_pages wp ON ws.path = wp.path AND ws.scope = wp.scope
         WHERE wiki_search MATCH ? AND ws.scope = ?
         ORDER BY 
            -- Boost by recency (pages updated in last 7 days get 2x)
            CASE WHEN wp.updated > datetime('now', '-7 days') THEN rank * 2 ELSE rank END,
            -- Boost by confidence
            CASE wp.confidence WHEN 'high' THEN 3 WHEN 'medium' THEN 2 ELSE 1 END DESC,
            -- Boost by reference count
            wp.reference_count DESC
         LIMIT ?"
    )
    .bind(query)
    .bind(scope)
    .bind(limit as i64)
    .fetch_all(db).await?;
    
    Ok(results)
}
```

---

## Local vs cloud memory behavior

| Aspect | Local | Cloud |
|---|---|---|
| Storage | `~/.moa/memory/` filesystem | Synced filesystem or mounted volume |
| Search index | Postgres tsvector + GIN (step 90) | Postgres tsvector + GIN (step 90) |
| Concurrent writes | Single brain — no branching needed | Git-branch model with LLM reconciler |
| Consolidation | Local cron (tokio-cron-scheduler) | Temporal timer workflow |
| Editing | Any text editor | Web dashboard or messaging commands |
| Backup | Git (wiki is markdown files) | Git + cloud backup |
| Migration | Copy `memory/` directory plus Postgres data | Managed via Postgres backups / Neon branches |
