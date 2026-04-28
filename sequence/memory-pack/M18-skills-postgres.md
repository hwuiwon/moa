# Step M18 — Skills migration to Postgres rows (delete on-disk skill loader)

_Move skills from on-disk markdown files (`skills/*.md`) to a `moa.skill` Postgres table with FORCE-RLS. The on-disk loader is deleted in this step. Skills become first-class graph-adjacent entities so M19 can cross-reference them with Lesson nodes._

## 1 What this step is about

Today skills are markdown files plus a YAML frontmatter, loaded at startup by `moa-skills`. That model breaks multi-tenancy (no per-workspace skills), defeats RLS (the filesystem is shared), and prevents linking skills to graph entities (M19). We move skills into Postgres and delete the on-disk loader.

CLI `moa skills export` and `moa skills import` (added here) preserve the markdown-on-disk workflow for editing — author locally, import to a workspace.

## 2 Files to read

- `crates/moa-skills/src/lib.rs`
- `crates/moa-skills/src/loader.rs`
- `skills/` directory at repo root (will be deleted from runtime path)
- M02 RLS template

## 3 Goal

1. `moa.skill` table with FORCE-RLS.
2. Updated `SkillRegistry` reads from Postgres (with caching).
3. CLI `moa skills export <wid> --to dir/` and `moa skills import <wid> --from dir/`.
4. On-disk skill loader deleted; the `skills/` dir becomes a vestigial author-tooling location only.
5. Bootstrap script imports the existing `skills/*.md` corpus into a `Global` workspace seed.

## 4 Rules

- **Skills are scoped**: Global, Workspace, or User (same 3-tier as memory).
- **Skill body** stored as `body MARKDOWN`; rendered to JSON on read by parser if needed.
- **Versioning**: skills carry a `version` integer; bumping a skill creates a new row with previous_version FK and increments the version.
- **Hash dedup**: `body_hash BYTEA` UNIQUE per `(workspace_id, name)` to prevent accidental re-imports.

## 5 Tasks

### 5a Migration: `migrations/M18_skills.sql`

```sql
CREATE TABLE moa.skill (
    skill_uid     UUID PRIMARY KEY,
    workspace_id  UUID,
    user_id       UUID,
    scope         TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    name          TEXT NOT NULL,
    description   TEXT,
    body          TEXT NOT NULL,
    body_hash     BYTEA NOT NULL,
    version       INT  NOT NULL DEFAULT 1,
    previous_skill_uid UUID,
    tags          TEXT[],
    valid_to      TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX skill_active_name_uniq ON moa.skill (
    coalesce(workspace_id, '00000000-0000-0000-0000-000000000000'::uuid),
    coalesce(user_id, '00000000-0000-0000-0000-000000000000'::uuid),
    name
) WHERE valid_to IS NULL;

CREATE INDEX skill_tags_gin ON moa.skill USING GIN (tags);

ALTER TABLE moa.skill ENABLE ROW LEVEL SECURITY; ALTER TABLE moa.skill FORCE ROW LEVEL SECURITY;

CREATE POLICY rd_global ON moa.skill FOR SELECT TO moa_app USING (scope = 'global');
CREATE POLICY rd_workspace ON moa.skill FOR SELECT TO moa_app
  USING (scope = 'workspace' AND workspace_id = moa.current_workspace());
CREATE POLICY rd_user ON moa.skill FOR SELECT TO moa_app
  USING (scope = 'user' AND workspace_id = moa.current_workspace() AND user_id = moa.current_user_id());

CREATE POLICY wr_workspace ON moa.skill FOR ALL TO moa_app
  USING      (scope = 'workspace' AND workspace_id = moa.current_workspace())
  WITH CHECK (scope = 'workspace' AND workspace_id = moa.current_workspace());
CREATE POLICY wr_user ON moa.skill FOR ALL TO moa_app
  USING      (scope = 'user' AND workspace_id = moa.current_workspace() AND user_id = moa.current_user_id())
  WITH CHECK (scope = 'user' AND workspace_id = moa.current_workspace() AND user_id = moa.current_user_id());
CREATE POLICY wr_global ON moa.skill FOR ALL TO moa_promoter
  USING (scope = 'global') WITH CHECK (scope = 'global');

GRANT SELECT, INSERT, UPDATE, DELETE ON moa.skill TO moa_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.skill TO moa_promoter;
```

### 5b SkillRegistry reads from Postgres

`crates/moa-skills/src/registry.rs`:

```rust
pub struct SkillRegistry { pool: PgPool, cache: Cache<Uuid, Vec<Skill>> }

impl SkillRegistry {
    pub async fn load_for_scope(&self, scope: &MemoryScope) -> Result<Vec<Skill>> {
        // RLS + scope.ancestors() → returns global+workspace+user as applicable
        sqlx::query_as!(Skill,
            "SELECT skill_uid, workspace_id, user_id, scope, name, description, body, version, tags
             FROM moa.skill WHERE valid_to IS NULL"
        ).fetch_all(&self.pool).await.map_err(Into::into)
    }
    pub async fn create(&self, skill: NewSkill) -> Result<Uuid> { /* ... */ }
    pub async fn upsert_by_name(&self, skill: NewSkill) -> Result<Uuid> { /* version-bump on body_hash mismatch */ }
}
```

### 5c CLI

`crates/moa-cli/src/commands/skills.rs`:

```rust
#[derive(Subcommand)]
pub enum SkillsCmd {
    Export { workspace: Uuid, to: PathBuf },
    Import { workspace: Uuid, from: PathBuf, scope: String /* global|workspace|user */ },
    List   { workspace: Uuid },
}
```

Export writes `<name>.md` per skill with YAML frontmatter (uid, version, tags). Import parses frontmatter, computes `body_hash`, calls `upsert_by_name`.

### 5d Bootstrap script

`scripts/bootstrap_global_skills.rs`:

Reads any `skills/*.md` checked into the repo and imports them as Global scope using `moa_promoter` role.

### 5e Delete on-disk loader

```sh
git rm crates/moa-skills/src/loader.rs
```

Update `crates/moa-skills/src/lib.rs`:

```rust
pub mod registry;     // new
// pub mod loader;    // DELETED — no on-disk loading
pub mod skill;
```

## 6 Deliverables

- `migrations/M18_skills.sql`.
- `crates/moa-skills/src/registry.rs` (~300 lines).
- `crates/moa-cli/src/commands/skills.rs`.
- `scripts/bootstrap_global_skills.rs`.

## 7 Acceptance criteria

1. Migration applies; bootstrap imports existing skills as Global.
2. `moa skills list <wid>` shows global + workspace skills (RLS filtered).
3. Import is idempotent — same body hash → no new row.
4. Export round-trip: export then import to a fresh workspace produces identical bodies (sha-256 equal).
5. Per-workspace skill creation visible only to that workspace.

## 8 Tests

```sh
cargo run --bin migrate
cargo run -- skills bootstrap-global
cargo test -p moa-skills registry_round_trip
cargo test -p moa-skills cli_export_import
```

## 9 Cleanup

- **DELETE** `crates/moa-skills/src/loader.rs`.
- **DELETE** any auto-load on startup that walked the filesystem.
- **DELETE** `skills/_index.yml` style indexes if they exist.
- **Note**: the `skills/` directory at repo root remains as an authoring convenience but is no longer read by the runtime. Add a `README.md` clarifying this.
- **Remove** any `MEMORY.md` template skill — that's gone.

## 10 What's next

**M19 — Skill ↔ graph cross-references (Lesson nodes + skill_addendum table; mistake-avoidance dual-write).**
