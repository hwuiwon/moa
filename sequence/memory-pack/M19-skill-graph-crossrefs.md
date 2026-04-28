# Step M19 — Skill ↔ graph cross-references (Lesson nodes + skill_addendum table; mistake-avoidance dual-write)

_When the agent learns "don't do X again" (a Lesson), we want it written to BOTH a graph node (so it appears in retrieval) AND as an addendum to the relevant skill (so it shows up at skill load time). This prompt builds the dual-write flow and the FK-linked `moa.skill_addendum` table._

## 1 What this step is about

Skills (M18) are static reference content; the graph holds dynamic lessons. A lesson should appear in both: searched as a graph fact AND prepended to the skill prompt. We don't duplicate text — the addendum stores a reference to the Lesson node uid plus a short summary; on skill render, the loader looks up the linked Lesson(s) and inlines them.

## 2 Files to read

- M18 `moa.skill`
- M07 GraphStore (Lesson label)
- M08 write protocol (we're calling it twice in dual-write)

## 3 Goal

1. `moa.skill_addendum` table linking a skill to a lesson_node_uid.
2. `learn_lesson(skill_uid, lesson_text, ctx)` API in `moa-skills` that creates a Lesson node + addendum row in one transaction.
3. Skill renderer prepends addenda content (joined from sidecar) to the skill body at load time.

## 4 Rules

- **Dual-write is one Postgres transaction**: M08's atomic write protocol covers the Lesson create; the addendum INSERT joins the same tx.
- **`linked_lesson_uid` is FK** to `moa.node_index(uid)` with `ON DELETE CASCADE`.
- **Addendum has `summary`** for fast render; the full Lesson body lives in the graph node.
- **Skill render is read-only**: addenda ordered by `created_at` DESC, top-N (default 5).

## 5 Tasks

### 5a Migration: `migrations/M19_skill_addendum.sql`

```sql
CREATE TABLE moa.skill_addendum (
    addendum_uid       UUID PRIMARY KEY,
    skill_uid          UUID NOT NULL REFERENCES moa.skill(skill_uid) ON DELETE CASCADE,
    linked_lesson_uid  UUID NOT NULL REFERENCES moa.node_index(uid) ON DELETE CASCADE,
    workspace_id       UUID,
    user_id            UUID,
    scope              TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    summary            TEXT NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    valid_to           TIMESTAMPTZ
);

CREATE INDEX skill_addendum_skill_idx ON moa.skill_addendum (skill_uid) WHERE valid_to IS NULL;
CREATE INDEX skill_addendum_lesson_idx ON moa.skill_addendum (linked_lesson_uid);

ALTER TABLE moa.skill_addendum ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.skill_addendum FORCE ROW LEVEL SECURITY;

-- Standard 3-tier RLS template (abbreviated; same pattern as M02)
CREATE POLICY rd_global ON moa.skill_addendum FOR SELECT TO moa_app USING (scope = 'global');
CREATE POLICY rd_ws     ON moa.skill_addendum FOR SELECT TO moa_app
  USING (scope = 'workspace' AND workspace_id = moa.current_workspace());
CREATE POLICY rd_user   ON moa.skill_addendum FOR SELECT TO moa_app
  USING (scope = 'user' AND workspace_id = moa.current_workspace() AND user_id = moa.current_user_id());
CREATE POLICY wr_ws ON moa.skill_addendum FOR ALL TO moa_app
  USING (workspace_id = moa.current_workspace())
  WITH CHECK (workspace_id = moa.current_workspace());
CREATE POLICY wr_global ON moa.skill_addendum FOR ALL TO moa_promoter
  USING (scope = 'global') WITH CHECK (scope = 'global');

GRANT SELECT, INSERT, UPDATE, DELETE ON moa.skill_addendum TO moa_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.skill_addendum TO moa_promoter;
```

### 5b learn_lesson API

`crates/moa-skills/src/lessons.rs`:

```rust
pub async fn learn_lesson(
    skill_uid: Uuid, lesson_text: String, summary: String,
    scope: MemoryScope, actor: Uuid, ctx: &Ctx,
) -> Result<(Uuid /* lesson */, Uuid /* addendum */)> {
    // Open one tx
    let mut tx = ctx.pool.begin().await?;
    // 1. Create Lesson via GraphStore (M08 write protocol — but with caller-managed tx)
    let lesson_uid = Uuid::now_v7();
    let intent = NodeWriteIntent {
        uid: lesson_uid,
        label: NodeLabel::Lesson,
        workspace_id: scope.workspace_id(),
        user_id: scope.user_id(),
        scope: scope.tier_str().into(),
        name: summary.chars().take(80).collect(),
        properties: serde_json::json!({"text": lesson_text}),
        pii_class: PiiClass::None,         // assume non-PII; route through PII classifier when called from agent
        confidence: Some(1.0),
        valid_from: Utc::now(),
        embedding: None,                   // optional: embed via embedder before this call
        embedding_model: None, embedding_model_version: None,
        actor_id: actor, actor_kind: "agent".into(),
    };
    ctx.graph.create_node_in_tx(&mut tx, intent).await?;

    // 2. Create addendum row
    let addendum_uid = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO moa.skill_addendum
         (addendum_uid, skill_uid, linked_lesson_uid, workspace_id, user_id, summary)
         VALUES ($1, $2, $3, $4, $5, $6)",
        addendum_uid, skill_uid, lesson_uid,
        scope.workspace_id(), scope.user_id(), summary,
    ).execute(&mut *tx).await?;

    tx.commit().await?;
    Ok((lesson_uid, addendum_uid))
}
```

(Note: `GraphStore::create_node_in_tx` is a helper that takes a borrowed transaction — add this in M07 retroactively if not yet present.)

### 5c Skill renderer hook

`crates/moa-skills/src/render.rs`:

```rust
pub async fn render(skill: &Skill, scope: &MemoryScope, ctx: &Ctx) -> Result<String> {
    let addenda = sqlx::query!(
        "SELECT summary, linked_lesson_uid FROM moa.skill_addendum
         WHERE skill_uid = $1 AND valid_to IS NULL
         ORDER BY created_at DESC LIMIT 5",
        skill.skill_uid,
    ).fetch_all(&ctx.pool).await?;

    if addenda.is_empty() { return Ok(skill.body.clone()); }
    let mut out = String::with_capacity(skill.body.len() + 256);
    out.push_str("<!-- learned lessons -->\n");
    for a in &addenda { out.push_str("- "); out.push_str(&a.summary); out.push('\n'); }
    out.push_str("\n---\n\n");
    out.push_str(&skill.body);
    Ok(out)
}
```

### 5d Cleanup of old skill self-modification

If pre-existing code rewrote skill bodies in place to add lessons, REMOVE that path entirely. Skills are now immutable once stored; lessons live in addenda.

## 6 Deliverables

- `migrations/M19_skill_addendum.sql`.
- `crates/moa-skills/src/lessons.rs` (~150 lines).
- `crates/moa-skills/src/render.rs` (~80 lines).
- Updated GraphStore with `create_node_in_tx` helper.

## 7 Acceptance criteria

1. `learn_lesson(skill, "don't deploy on Friday", "Friday-deploy hazard", ...)` creates one Lesson node + one addendum row in one tx.
2. Render returns body with lessons prepended.
3. `DELETE FROM moa.skill WHERE skill_uid = $1` cascades to addenda; `DELETE FROM moa.node_index WHERE uid = $lesson` cascades the addendum (FK).
4. RLS isolates addenda per scope.

## 8 Tests

```sh
cargo run --bin migrate
cargo test -p moa-skills learn_lesson_dual_write
cargo test -p moa-skills render_with_addenda
```

## 9 Cleanup

- Remove any old "skill self-update" code that rewrote files.
- Remove any in-memory caches that mixed body+lessons.

## 10 What's next

**M20 — Connector trait + MockConnector (interfaces only, no real integrations).**
