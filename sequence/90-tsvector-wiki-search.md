# Step 90 — Wiki Memory Search via Postgres tsvector + GIN

_Step 83 deleted the SQLite FTS5 index. This step rebuilds it on Postgres using `tsvector + GIN` — which is faster, more expressive, and lives in the same database as session events (enabling cross-store joins in step 92). Includes phrase search, scope-filtered queries, recency + confidence re-ranking in a single SQL query._

---

## 1. What this step is about

Step 83 left `FileMemoryStore::search` returning `MoaError::NotImplemented`. The markdown files are still on disk and are still source of truth; we only lost the search index. Postgres gives us better tools than SQLite FTS5 had:

- `tsvector` columns with GIN indexes: sub-millisecond match queries at millions of rows.
- `ts_rank_cd` for BM25-adjacent ranking.
- `plainto_tsquery`, `phraseto_tsquery`, `websearch_to_tsquery` for different query grammars.
- `trigram` extensions for fuzzy matching (typo-tolerant search).
- JSON-typed tags column with GIN for `tags @> ARRAY['security']`-style filters.
- Cross-table joins: "memory pages referenced in the last 10 sessions" is one SQL query.

The schema lives in the same Postgres that holds session events. This sets up step 92 (generated columns + analytic views) and step 91 (pgvector semantic search), both of which join wiki pages with session data.

---

## 2. Files to read

- `moa-memory/src/lib.rs` — `FileMemoryStore` structure.
- `moa-memory/src/search.rs` — stub from step 83 that returns `NotImplemented`.
- `moa-memory/src/wiki.rs` — markdown parser; we need `WikiPage.title`, `content`, `tags`, etc.
- `moa-memory/src/index.rs` — index file semantics; unchanged.
- `moa-core/src/types/memory.rs` — `MemorySearchResult`, `MemoryScope`, `PageType`, `ConfidenceLevel`.
- `moa-session/src/schema.rs` — for the migration style conventions already in use.
- Postgres docs: `tsvector`, GIN indexes, `ts_rank_cd`, `websearch_to_tsquery`, `pg_trgm`.

---

## 3. Goal

1. A `wiki_pages` Postgres table holds every wiki page's metadata plus a `tsvector` column over title + content + tags.
2. `FileMemoryStore::search` returns ranked `MemorySearchResult`s with snippets, scoped by user/workspace, within 20ms at 10K pages.
3. Search grammar supports: bare keywords, quoted phrases, negations, OR operator. Uses `websearch_to_tsquery` under the hood (forgiving of malformed input).
4. Ranking combines: tsvector BM25-like score × recency boost (×0.5 if updated in last 7 days) × confidence (high=3, medium=2, low=1) × reference_count.
5. A `pg_trgm` fallback path handles typos: if the primary query returns zero results, run a trigram similarity query.
6. Rebuilding a scope's index from markdown files on disk completes in under 1 second per 1,000 pages.

---

## 4. Rules

- **Postgres is the derived index; files are still source of truth.** `rebuild_search_index(scope)` walks the markdown files on disk and upserts every page into Postgres. If the Postgres index is ever lost, we rebuild from files.
- **Upsert on every write.** `write_page` writes the markdown file AND upserts the Postgres row in the same logical operation. Failure in either is an error that rolls the whole operation back (delete the file if Postgres fails, so state doesn't diverge).
- **`ts_rank_cd`, not `ts_rank`.** `ts_rank_cd` uses cover density (considers term proximity) — more useful for short queries over long documents.
- **GIN index, not GiST.** GIN is faster for read-heavy workloads; GiST is faster for update-heavy. Wiki is read-heavy.
- **Snippet generation via `ts_headline`.** Built-in snippet extraction with `<mark>` tags.
- **Stopwords: default English config.** Configurable later per-workspace if needed. Don't add a language column yet.
- **Trigram fallback is explicit.** Don't silently transform every query to trigram; that degrades precision. Only fall back when the primary tsvector query returns zero rows AND the query length is ≤ 3 tokens.

---

## 5. Tasks

### 5a. Migration

Add `moa-memory/migrations/001_wiki_pages.sql` (or extend the session-store migration set — pick one; recommend a per-crate migration directory):

```sql
CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE TABLE wiki_pages (
    scope TEXT NOT NULL,
    path TEXT NOT NULL,
    title TEXT NOT NULL,
    page_type TEXT NOT NULL,
    confidence TEXT NOT NULL,
    created TIMESTAMPTZ NOT NULL,
    updated TIMESTAMPTZ NOT NULL,
    last_referenced TIMESTAMPTZ NOT NULL,
    reference_count INTEGER NOT NULL DEFAULT 0,
    tags TEXT[] NOT NULL DEFAULT '{}',
    content TEXT NOT NULL,
    -- Generated column: never written by app code
    search_tsv tsvector GENERATED ALWAYS AS (
        setweight(to_tsvector('english', coalesce(title, '')), 'A') ||
        setweight(to_tsvector('english', coalesce(array_to_string(tags, ' '), '')), 'B') ||
        setweight(to_tsvector('english', coalesce(content, '')), 'C')
    ) STORED,
    PRIMARY KEY (scope, path)
);

CREATE INDEX wiki_pages_tsv_gin ON wiki_pages USING GIN (search_tsv);
CREATE INDEX wiki_pages_title_trgm ON wiki_pages USING GIN (title gin_trgm_ops);
CREATE INDEX wiki_pages_tags_gin ON wiki_pages USING GIN (tags);
CREATE INDEX wiki_pages_updated ON wiki_pages (scope, updated DESC);
CREATE INDEX wiki_pages_type ON wiki_pages (scope, page_type);
```

Notes:
- `search_tsv` is a `GENERATED ALWAYS AS ... STORED` column. Postgres recomputes it automatically when title/tags/content change. App code never sets it.
- Three weights: A for title (highest), B for tags, C for content. Matches in the title rank higher than matches in the body.
- Stored (not virtual) so the GIN index stays current.

### 5b. `WikiSearchIndex` real implementation

Replace the stub in `moa-memory/src/search.rs`:

```rust
use sqlx::postgres::PgPool;

#[derive(Clone)]
pub struct WikiSearchIndex {
    pool: Arc<PgPool>,
}

impl WikiSearchIndex {
    pub fn new(pool: Arc<PgPool>) -> Self { Self { pool } }

    pub async fn upsert_page(&self, scope: &MemoryScope, path: &MemoryPath, page: &WikiPage) -> Result<()> {
        sqlx::query(r#"
            INSERT INTO wiki_pages
                (scope, path, title, page_type, confidence, created, updated, last_referenced, reference_count, tags, content)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            ON CONFLICT (scope, path) DO UPDATE SET
                title = EXCLUDED.title,
                page_type = EXCLUDED.page_type,
                confidence = EXCLUDED.confidence,
                updated = EXCLUDED.updated,
                last_referenced = EXCLUDED.last_referenced,
                reference_count = EXCLUDED.reference_count,
                tags = EXCLUDED.tags,
                content = EXCLUDED.content
        "#)
        .bind(scope_key(scope))
        .bind(path.as_str())
        .bind(&page.title)
        .bind(page_type_as_str(&page.page_type))
        .bind(confidence_as_str(&page.confidence))
        .bind(page.created)
        .bind(page.updated)
        .bind(page.last_referenced)
        .bind(page.reference_count as i32)
        .bind(&page.tags)
        .bind(&page.content)
        .execute(&*self.pool).await.map_err(memory_error)?;
        Ok(())
    }

    pub async fn delete_page(&self, scope: &MemoryScope, path: &MemoryPath) -> Result<()> {
        sqlx::query("DELETE FROM wiki_pages WHERE scope = $1 AND path = $2")
            .bind(scope_key(scope))
            .bind(path.as_str())
            .execute(&*self.pool).await.map_err(memory_error)?;
        Ok(())
    }

    pub async fn search(&self, query: &str, scope: &MemoryScope, limit: usize) -> Result<Vec<MemorySearchResult>> {
        let q = query.trim();
        if q.is_empty() { return Ok(Vec::new()); }

        // Primary: tsvector query
        let primary = self.search_tsvector(q, scope, limit).await?;
        if !primary.is_empty() { return Ok(primary); }

        // Fallback: trigram similarity for short queries
        if q.split_whitespace().count() <= 3 {
            return self.search_trigram(q, scope, limit).await;
        }
        Ok(Vec::new())
    }

    async fn search_tsvector(&self, q: &str, scope: &MemoryScope, limit: usize) -> Result<Vec<MemorySearchResult>> {
        let sql = r#"
            SELECT
                path, title, page_type, confidence, updated, reference_count,
                ts_headline(
                    'english', content, websearch_to_tsquery('english', $1),
                    'StartSel=<mark>, StopSel=</mark>, MaxFragments=2, MaxWords=20, MinWords=5'
                ) AS snippet,
                -- Composite rank: tsvector BM25 × recency × confidence × reference_count
                ts_rank_cd(search_tsv, websearch_to_tsquery('english', $1))
                    * CASE WHEN updated > NOW() - INTERVAL '7 days' THEN 2.0 ELSE 1.0 END
                    * CASE confidence WHEN 'high' THEN 3 WHEN 'medium' THEN 2 ELSE 1 END
                    * GREATEST(1, LOG(1 + reference_count))
                    AS score
            FROM wiki_pages
            WHERE scope = $2 AND search_tsv @@ websearch_to_tsquery('english', $1)
            ORDER BY score DESC
            LIMIT $3
        "#;

        let rows = sqlx::query(sql)
            .bind(q)
            .bind(scope_key(scope))
            .bind(limit as i64)
            .fetch_all(&*self.pool).await.map_err(memory_error)?;

        rows.into_iter().map(|r| -> Result<_> {
            Ok(MemorySearchResult {
                scope: scope.clone(),
                path: MemoryPath::new(r.get::<String, _>("path")),
                title: r.get("title"),
                page_type: parse_page_type(r.get("page_type"))?,
                snippet: r.get("snippet"),
                confidence: parse_confidence(r.get("confidence"))?,
                updated: r.get("updated"),
                reference_count: r.get::<i32, _>("reference_count") as u64,
            })
        }).collect()
    }

    async fn search_trigram(&self, q: &str, scope: &MemoryScope, limit: usize) -> Result<Vec<MemorySearchResult>> {
        // pg_trgm similarity for typo-tolerant matching against titles only
        let sql = r#"
            SELECT path, title, page_type, confidence, updated, reference_count,
                   title AS snippet,
                   similarity(title, $1) AS score
            FROM wiki_pages
            WHERE scope = $2 AND title % $1
            ORDER BY score DESC, updated DESC
            LIMIT $3
        "#;
        // similar extraction to search_tsvector
        // ...
        todo!()
    }

    pub async fn rebuild_scope(&self, scope: &MemoryScope, pages: &[(MemoryPath, WikiPage)]) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(memory_error)?;
        sqlx::query("DELETE FROM wiki_pages WHERE scope = $1")
            .bind(scope_key(scope))
            .execute(&mut *tx).await.map_err(memory_error)?;
        for (path, page) in pages {
            // same INSERT as upsert_page, but without ON CONFLICT
            // ... (or just call upsert_page in a loop; slower but cleaner)
        }
        tx.commit().await.map_err(memory_error)?;
        Ok(())
    }
}
```

### 5c. Wire `WikiSearchIndex` into `FileMemoryStore`

`FileMemoryStore` needs an `Arc<PgPool>`. Extend its constructor signatures:

```rust
impl FileMemoryStore {
    pub async fn new(base_dir: impl AsRef<Path>, pool: Arc<PgPool>) -> Result<Self> {
        // ...
        let search_index = WikiSearchIndex::new(pool);
        Ok(Self { base_dir: Arc::new(base_dir), search_index })
    }
}
```

Every caller of `FileMemoryStore::new` needs access to the `PgPool`. In `moa-orchestrator::local::LocalOrchestrator::new`, pass the pool that was already constructed for `PostgresSessionStore`. One pool, shared.

### 5d. Consistency: file-write with index-write

`write_page` currently writes markdown to disk then upserts the FTS row. Sequence matters:

```rust
async fn write_page(&self, scope: MemoryScope, path: &MemoryPath, mut page: WikiPage) -> Result<()> {
    let file_path = self.file_path(&scope, path)?;
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    page.path = Some(path.clone());
    let markdown = render_markdown(&page)?;

    // Write markdown first (idempotent — same content → same file)
    fs::write(&file_path, &markdown).await?;

    // Upsert index; on failure, leave the markdown in place (files are truth).
    // Log the failure; a future rebuild_search_index will reconcile.
    if let Err(e) = self.search_index.upsert_page(&scope, path, &page).await {
        tracing::warn!(scope=?scope, path=%path.as_str(), error=%e,
            "file written but search index upsert failed; run `moa memory rebuild-index` to reconcile");
    }

    Ok(())
}
```

The file-write is transactional-enough; the index is eventually-consistent. A future `moa memory rebuild-index` subcommand re-reads files and syncs. This tradeoff mirrors what SQLite FTS5 did too: the actual wiki lives on disk.

### 5e. CLI: `moa memory rebuild-index`

```rust
pub async fn cmd_memory_rebuild_index(orchestrator: &LocalOrchestrator, scope: Option<MemoryScope>) -> Result<()> {
    let scopes = match scope {
        Some(s) => vec![s],
        None => discover_all_scopes(&orchestrator.memory).await?,
    };
    for scope in scopes {
        let pages = orchestrator.memory.load_all_pages(&scope).await?;
        orchestrator.memory.search_index.rebuild_scope(&scope, &pages).await?;
        println!("rebuilt {} pages in {:?}", pages.len(), scope);
    }
    Ok(())
}
```

### 5f. Performance: index maintenance

For large rebuilds:
- Use `COPY ... FROM STDIN` instead of per-row INSERT. `sqlx::postgres::PgCopyIn` handles this.
- Or use `UNNEST` with array parameters: `INSERT INTO wiki_pages SELECT * FROM unnest($1::text[], $2::text[], ...)`.
- At 10K pages, COPY is ~50× faster than per-row INSERT.

Initial rebuild doesn't need this optimization (small datasets). Add it if someone complains.

### 5g. Tests

- Search "oauth refresh token" against a corpus → correct page in top 3.
- Search "oatuh" (typo) → trigram fallback finds "oauth" page.
- Tag filter: `SELECT ... WHERE tags @> ARRAY['security']`.
- Recency: a page updated today outranks an older page with same tsvector score.
- Scope isolation: user scope search returns zero rows for workspace-scoped pages.
- Rebuild-from-disk: populate Postgres with stale data, run rebuild_scope from a fresh disk state, assert Postgres matches disk.
- Concurrent writes: two `write_page` calls on different paths in same scope succeed (no deadlock).

Use `testcontainers` Postgres. Preload `pg_trgm` extension in the test setup.

### 5h. Documentation

`moa/docs/04-memory-architecture.md` gets an update: the "FTS5" section becomes "Postgres tsvector." Mention:
- Markdown files on disk are source of truth.
- Postgres index is derived; rebuildable via `moa memory rebuild-index`.
- Query grammar: `websearch_to_tsquery` — double quotes for phrases, minus for negation, OR keyword.
- Tag filters: `tags @> ARRAY[...]` available via a future API surface (not this step).

---

## 6. Deliverables

- [ ] `moa-memory/migrations/001_wiki_pages.sql` with the schema above.
- [ ] `moa-memory/src/search.rs` fully implemented against Postgres.
- [ ] `FileMemoryStore::new` takes an `Arc<PgPool>`.
- [ ] `write_page` upserts index; failure logged but non-fatal (file wins).
- [ ] `delete_page` removes index row.
- [ ] `rebuild_scope` repopulates Postgres from a list of pages.
- [ ] `moa memory rebuild-index` CLI subcommand.
- [ ] Trigram fallback for typo-tolerant short queries.
- [ ] Tests with testcontainers Postgres covering search, trigram, scope isolation, rebuild.
- [ ] `moa/docs/04-memory-architecture.md` updated.

---

## 7. Acceptance criteria

1. `FileMemoryStore::search("oauth refresh")` returns the correct page in <20ms at 1000 pages indexed (measured with p95).
2. `search("oatuh")` (typo) returns the OAuth page via trigram fallback.
3. Deleting `~/.moa/workspaces/*/memory/search.db` and dropping Postgres's `wiki_pages` rows, then running `moa memory rebuild-index`, restores full search functionality — same results as before the delete.
4. Search correctly isolates user-scope from workspace-scope results (no bleed).
5. A page updated 1 hour ago ranks higher than a page updated 1 year ago when both match the query equally.
6. Step 78's integration test — if it exercises memory search — passes. The `#[ignore]` from step 83 is removed.
7. `cargo test -p moa-memory` green.
8. Rebuilding a 10K-page scope completes under 5 seconds.
9. Query plans (via `EXPLAIN`) show `Bitmap Index Scan on wiki_pages_tsv_gin` — index is being used.
