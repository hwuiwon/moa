# 09 — Skills & Learning

_Agent Skills standard, auto-distillation, self-improvement, skill registry._

---

## Agent Skills format (agentskills.io)

MOA adopts the Agent Skills standard with MOA-specific extensions.

### Directory structure

```
skills/
└── deploy-to-fly/
    ├── SKILL.md          # YAML frontmatter + markdown instructions
    ├── scripts/          # Executable scripts (run, not read)
    │   └── deploy.sh
    ├── references/       # Supporting docs
    │   └── fly-config.md
    └── assets/           # Images, templates
```

### SKILL.md format

```markdown
---
name: deploy-to-fly
description: "Deploy applications to Fly.io staging and production"
compatibility: "Requires flyctl auth and repo write access"
allowed-tools: bash file_read
metadata:
  moa-version: "1.2"
  moa-one-liner: "Fly.io deploy workflow with health checks"
  moa-tags: "deployment, fly, devops"
  moa-created: "2026-04-09T14:30:00Z"
  moa-updated: "2026-04-09T16:00:00Z"
  moa-auto-generated: "true"
  moa-source-session: "abc123"
  moa-use-count: "7"
  moa-last-used: "2026-04-09T16:00:00Z"
  moa-success-rate: "0.86"
  moa-brain-affinity: "general"    # general | coding | research | devops
  moa-sandbox-tier: "container"    # none | container | microvm
  moa-estimated-tokens: "1200"     # full body token count
  moa-improved-from: ""            # previous version if self-improved
---

# Deploy to Fly.io

## When to use
When the user asks to deploy an application to Fly.io staging or production.

## Prerequisites
- `fly` CLI installed in the sandbox
- `fly.toml` present in project root
- User has configured Fly.io credentials

## Procedure
1. Check `fly.toml` exists and is valid
2. Run `fly status` to verify current state
3. For staging: `fly deploy --config fly.toml --app {app}-staging`
4. For production: `fly deploy --config fly.toml --app {app}`
5. Wait for health checks: `fly status --watch`
6. Verify deployment: `curl -s https://{app}.fly.dev/health`

## Pitfalls
- If deploy fails with "No machines running", run `fly machine start` first
- Production deploys should always be preceded by staging verification
- Watch for "context deadline exceeded" — increase `kill_timeout` in fly.toml

## Verification
- Health endpoint returns 200
- `fly status` shows all machines as "started"
- No error lines in `fly logs --last 50`

## Scripts
Run `scripts/deploy.sh` for the full automated flow. Do NOT read the script into context — execute it directly.
```

### Three-tier progressive disclosure

| Tier | Content | Token cost | Loaded when |
|---|---|---|---|
| Metadata | name, description, tags | ~100 tokens/skill | Always (Stage 4 of pipeline) |
| Instructions | Full SKILL.md body | ~1,000-5,000 tokens | When skill is activated for a task |
| Resources | scripts/, references/, assets/ | Varies | When executing the skill |

This is why `estimated_tokens` is in the frontmatter — the pipeline uses it for budget planning.

---

## Skill distillation (auto-generation from runs)

After a successful multi-step run (≥5 tool calls), the brain considers distilling a skill:

```rust
async fn maybe_distill_skill(
    session_id: SessionId,
    events: &[EventRecord],
    memory: &dyn MemoryStore,
    llm: &dyn LLMProvider,
) -> Result<Option<String>> {
    // Count tool calls in this session
    let tool_calls: Vec<_> = events.iter()
        .filter(|e| matches!(e.event_type(), "ToolCall"))
        .collect();
    
    if tool_calls.len() < 5 {
        return Ok(None); // not complex enough
    }
    
    // Check if a similar skill already exists
    let task_summary = extract_task_summary(events);
    let existing = memory.search(&task_summary, MemoryScope::workspace(), 3).await?;
    
    for result in &existing {
        if result.page_type == "skill" && result.similarity > 0.8 {
            // Similar skill exists — consider improvement instead
            return maybe_improve_skill(result, events, memory, llm).await;
        }
    }
    
    // Generate new skill
    let prompt = format!(
        "Analyze this completed agent session and distill it into a reusable skill.\n\
         Use the Agent Skills YAML frontmatter format.\n\n\
         Include:\n\
         - When to use this skill\n\
         - Step-by-step procedure\n\
         - Pitfalls encountered (especially any errors)\n\
         - Verification steps\n\n\
         Session events:\n{}",
        format_events_for_distillation(events)
    );
    
    let skill_content = llm.complete(CompletionRequest::simple(prompt)).await?;
    
    // Parse and validate the generated skill
    let skill = parse_skill_md(&skill_content.text)?;
    let skill_name = slugify(&skill.name);
    let skill_path = format!("skills/{}/SKILL.md", skill_name);
    
    // Write to workspace memory
    memory.write_page(
        &skill_path.into(),
        WikiPage::from_skill(skill_content.text, true /* auto_generated */),
    ).await?;
    
    // Update MEMORY.md
    update_index_with_skill(memory, &skill_name, &skill.description).await?;
    
    Ok(Some(skill_name))
}
```

---

## Skill self-improvement

When the brain uses an existing skill and discovers a better approach, it updates the skill:

```rust
async fn maybe_improve_skill(
    existing: &MemorySearchResult,
    events: &[EventRecord],
    memory: &dyn MemoryStore,
    llm: &dyn LLMProvider,
) -> Result<Option<String>> {
    let current_skill = memory.read_page(&existing.path).await?;
    
    // Compare current execution with skill's documented procedure
    let prompt = format!(
        "You just completed a task using this skill, but your execution differed from \
         the documented procedure. Review both and update the skill if the new approach \
         is better.\n\n\
         Current skill:\n{}\n\n\
         Actual execution:\n{}\n\n\
         If the skill should be updated, output the complete updated SKILL.md.\n\
         If no changes needed, output UNCHANGED.",
        current_skill.content,
        format_events_for_distillation(events)
    );
    
    let result = llm.complete(CompletionRequest::simple(prompt)).await?;
    
    if result.text.trim() == "UNCHANGED" {
        // Just update use_count and last_used
        update_skill_metadata(&existing.path, memory).await?;
        return Ok(None);
    }
    
    // Parse updated skill
    let mut updated = parse_skill_md(&result.text)?;
    
    // Preserve history: bump version, track improvement
    updated.frontmatter.insert("improved_from".to_string(), 
        current_skill.frontmatter.get("version").cloned().unwrap_or_default());
    updated.frontmatter.insert("version".to_string(), 
        bump_version(current_skill.frontmatter.get("version")));
    
    memory.write_page(&existing.path, WikiPage::from_skill(result.text, true)).await?;
    
    Ok(Some(existing.path.to_string()))
}
```

---

## Skill registry

Discovers and manages skills across workspaces:

```rust
pub struct SkillRegistry {
    skills: RwLock<HashMap<String, SkillMetadata>>,
    memory: Arc<dyn MemoryStore>,
}

impl SkillRegistry {
    /// Load skills from workspace memory
    pub async fn load(&self, workspace_id: &WorkspaceId) -> Result<()> {
        let skill_pages = self.memory.list_pages(
            MemoryScope::Workspace(workspace_id.clone()),
            Some(PageType::Skill),
        ).await?;
        
        let mut skills = self.skills.write().await;
        skills.clear();
        
        for page in skill_pages {
            if let Ok(metadata) = parse_skill_metadata(&page) {
                skills.insert(metadata.name.clone(), metadata);
            }
        }
        
        Ok(())
    }
    
    /// Get skill metadata for the context pipeline (Tier 1: metadata only)
    pub fn list_for_pipeline(&self) -> Vec<SkillMetadata> {
        self.skills.read().blocking_lock()
            .values()
            .cloned()
            .collect()
    }
    
    /// Load full skill body for execution (Tier 2)
    pub async fn load_full(&self, skill_name: &str) -> Result<String> {
        let path = format!("skills/{}/SKILL.md", skill_name);
        let page = self.memory.read_page(&path.into()).await?;
        Ok(page.body())
    }
}

pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub estimated_tokens: usize,
    pub use_count: u32,
    pub success_rate: f32,
    pub auto_generated: bool,
}
```

---

## Skill lifecycle

```
1. DISCOVERY
   - Brain encounters a task matching no existing skill
   - Brain completes the task using tools

2. DISTILLATION (auto, post-run)
   - ≥5 tool calls → consider distillation
   - LLM generates SKILL.md from session events
   - Written to workspace memory under skills/

3. ACTIVATION (during future sessions)
   - Pipeline Stage 4 shows skill metadata to brain
   - Brain recognizes task matches a skill
   - Brain calls memory_read to load full skill body
   - Brain follows skill procedure

4. IMPROVEMENT (during use)
   - Brain uses skill but finds a better approach
   - LLM generates updated SKILL.md
   - Version bumped, improvement tracked

5. DECAY (during consolidation)
   - Skills not used in 90+ days get confidence lowered
   - Skills with <50% success rate get flagged for review
   - Skills about deleted tools/services get pruned
```

---

## User-authored skills

Users can manually create skills by placing SKILL.md files in `~/.moa/workspaces/{id}/memory/skills/`. The same format applies. Set `auto_generated: false` in frontmatter.

Skills can also be installed from community registries (agentskills.io, ClawHub) — these are read-only copies placed in `skills/` with a `source: community` frontmatter field.
