# 07 — Context Compilation Pipeline

_7-stage compilation, stable prefix caching, failure mode guardrails, observability._

---

## Why this matters

KV-cache hit rate is the single most important cost and latency metric for a production agent. Cached tokens cost 10x less across all major providers (Anthropic: $0.30 vs $3.00/MTok). Agentic workflows have ~100:1 input-to-output ratio. A well-structured pipeline pays for itself immediately.

---

## Pipeline architecture

Seven stages, ordered for maximum cache prefix stability:

```
┌───────────────────────────────────────────────────────┐
│  STABLE PREFIX (identical across turns → cached)       │
│                                                        │
│  Stage 1: IdentityProcessor                           │
│  Stage 2: InstructionProcessor                        │
│  Stage 3: ToolDefinitionProcessor                     │
│  Stage 4: SkillInjector                               │
├────────── cache_breakpoint ───────────────────────────┤
│  DYNAMIC CONTENT (changes per turn)                    │
│                                                        │
│  Stage 5: MemoryRetriever                             │
│  Stage 6: HistoryCompiler                             │
│  Stage 7: CacheOptimizer (final pass)                 │
└───────────────────────────────────────────────────────┘
```

### Stage 1: IdentityProcessor

Injects brain identity and role. **Completely static** — identical across every turn in every session.

```
You are MOA, a general-purpose AI agent. You help users accomplish tasks by 
reasoning, using tools, and building on accumulated knowledge.

You have access to tools for file operations, shell commands, web search, 
and memory management. You can request additional tools if needed.

When you make changes, explain what you did and why. When you encounter 
errors, preserve them in context — they prevent repeated mistakes.
```

Token cost: ~200 tokens. Cached after first turn.

### Stage 2: InstructionProcessor

Injects workspace-specific and user-specific instructions. Semi-static — changes only when CLAUDE.md-equivalent files change.

```rust
fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
    let mut content = String::new();
    
    // Workspace instructions (equivalent to CLAUDE.md)
    if let Some(instructions) = load_workspace_instructions(ctx.workspace_id) {
        content.push_str(&format!("<workspace_instructions>\n{}\n</workspace_instructions>\n", instructions));
    }
    
    // User instructions
    if let Some(prefs) = load_user_instructions(ctx.user_id) {
        content.push_str(&format!("<user_preferences>\n{}\n</user_preferences>\n", prefs));
    }
    
    ctx.append_system(content);
    Ok(ProcessorOutput { tokens_added: ctx.count_last(), ..Default::default() })
}
```

### Stage 3: ToolDefinitionProcessor

Serializes active tool schemas. **Fixed for the entire session** — never add or remove tools mid-session (breaks KV cache and confuses the model).

```rust
fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
    let tools = self.tool_registry.get_loadout(ctx.workspace_id);
    
    // Cap at 30 tools
    if tools.len() > 30 {
        tracing::warn!("Tool loadout exceeds 30, truncating to avoid context confusion");
    }
    let tools: Vec<_> = tools.into_iter().take(30).collect();
    
    // Deterministic serialization (stable JSON key ordering)
    let schemas: Vec<serde_json::Value> = tools.iter()
        .map(|t| {
            let mut schema = t.schema.clone();
            // Sort keys deterministically for cache stability
            sort_json_keys(&mut schema);
            schema
        })
        .collect();
    
    ctx.set_tools(schemas);
    Ok(ProcessorOutput { 
        tokens_added: estimate_tool_tokens(&tools),
        items_included: tools.iter().map(|t| t.name.clone()).collect(),
        ..Default::default()
    })
}
```

### Stage 4: SkillInjector

Injects skill metadata for discovery. Only full skill bodies are loaded on activation.

```rust
fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
    let skills = self.skill_registry.list_for_workspace(ctx.workspace_id);
    
    // Tier 1: Metadata only (~100 tokens per skill)
    let skill_index: Vec<String> = skills.iter().map(|s| {
        format!("- {}: {} [tags: {}]", s.name, s.description, s.tags.join(", "))
    }).collect();
    
    ctx.append_system(format!(
        "<available_skills>\n{}\n\
         To use a skill, call memory_read with the skill path.\n\
         </available_skills>",
        skill_index.join("\n")
    ));
    
    // Mark cache breakpoint here — everything above is stable
    ctx.mark_cache_breakpoint();
    
    Ok(ProcessorOutput {
        tokens_added: skill_index.len() * 100, // rough estimate
        items_included: skills.iter().map(|s| s.name.clone()).collect(),
        ..Default::default()
    })
}
```

### Stage 5: MemoryRetriever

Injects relevant memory. **Dynamic per turn** — changes based on what the user asks.

```rust
fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
    let mut tokens_added = 0;
    let budget = ctx.token_budget / 5; // 20% of total budget
    
    // Always load: user + workspace MEMORY.md (already done at session start)
    // These are in ctx.messages from session initialization
    
    // Task-relevant search: extract keywords from recent user message
    if let Some(last_user_msg) = ctx.last_user_message() {
        let keywords = extract_search_keywords(last_user_msg);
        
        if !keywords.is_empty() {
            let results = self.memory.search(
                &keywords,
                MemoryScope::Workspace(ctx.workspace_id.clone()),
                3, // top 3 results
            ).await?;
            
            if !results.is_empty() {
                let mut memory_section = String::from("<relevant_memory>\n");
                for result in &results {
                    let page = self.memory.read_page(&result.path).await?;
                    let truncated = truncate_to_tokens(&page.body(), budget / 3);
                    memory_section.push_str(&format!(
                        "## {} ({})\n{}\n\n", 
                        result.title, result.path, truncated
                    ));
                    tokens_added += estimate_tokens(&truncated);
                }
                memory_section.push_str("</relevant_memory>");
                
                ctx.append_system(memory_section);
            }
        }
    }
    
    Ok(ProcessorOutput {
        tokens_added,
        ..Default::default()
    })
}
```

### Stage 6: HistoryCompiler

Compiles session history into the context. Most complex stage.

```rust
fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
    let remaining_budget = ctx.token_budget - ctx.token_count;
    let events = self.store.get_events(ctx.session_id, EventRange::all()).await?;
    
    let mut messages = Vec::new();
    let mut tokens_used = 0;
    
    // If there's a checkpoint, start from there
    if let Some(checkpoint) = find_last_checkpoint(&events) {
        messages.push(ContextMessage::system(format!(
            "<session_checkpoint>\n{}\n</session_checkpoint>",
            checkpoint.summary
        )));
        tokens_used += estimate_tokens(&checkpoint.summary);
    }
    
    // Recent turns: verbatim (last 5)
    let recent_turns = extract_recent_turns(&events, 5);
    for turn in &recent_turns {
        let msg = turn.to_context_message();
        tokens_used += estimate_tokens(&msg.content);
        messages.push(msg);
    }
    
    // If still under budget, add older turns in reverse chronological order
    let older_turns = extract_older_turns(&events, &recent_turns);
    for turn in older_turns.iter().rev() {
        let msg = turn.to_context_message();
        let turn_tokens = estimate_tokens(&msg.content);
        if tokens_used + turn_tokens > remaining_budget {
            break; // budget exhausted
        }
        tokens_used += turn_tokens;
        messages.insert(messages.len() - recent_turns.len(), msg);
    }
    
    // ALWAYS preserve errors — strongest signal for avoiding repeated mistakes
    for event in &events {
        if let Event::Error { message, .. } = &event.event {
            if !messages.iter().any(|m| m.content.contains(message)) {
                messages.insert(0, ContextMessage::system(format!(
                    "<previous_error>{}</previous_error>", message
                )));
            }
        }
    }
    
    ctx.extend_messages(messages);
    
    Ok(ProcessorOutput { tokens_added: tokens_used, ..Default::default() })
}
```

### Stage 7: CacheOptimizer

Final verification pass. Ensures stable prefix ordering and marks cache breakpoints.

```rust
fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
    // Verify stable prefix hasn't been corrupted
    let cache_break = ctx.cache_breakpoints.last().copied().unwrap_or(0);
    let prefix_messages = &ctx.messages[..cache_break];
    
    // Ensure deterministic serialization
    for msg in &mut ctx.messages[..cache_break] {
        if let Some(tools) = &mut msg.tools {
            sort_json_keys(tools);
        }
    }
    
    // Report cache efficiency
    let prefix_tokens = prefix_messages.iter().map(|m| estimate_tokens(&m.content)).sum::<usize>();
    let total_tokens = ctx.token_count;
    let cache_ratio = prefix_tokens as f64 / total_tokens as f64;
    
    tracing::info!(
        cache_ratio = %format!("{:.1}%", cache_ratio * 100.0),
        prefix_tokens,
        total_tokens,
        "Context cache efficiency"
    );
    
    Ok(ProcessorOutput {
        tokens_added: 0,
        metadata: json!({ "cache_ratio": cache_ratio }),
        ..Default::default()
    })
}
```

---

## Failure mode guardrails

Built into the pipeline as runtime checks:

| Failure mode | Detection | Mitigation |
|---|---|---|
| **Context Poisoning** (hallucinations enter context) | Validate tool outputs against expected schemas before appending | Reject malformed tool results; flag suspicious content |
| **Context Distraction** (model over-focuses on old history) | Monitor history length; Databricks found degradation >32K tokens | Aggressive compaction; trigger checkpoint when history exceeds model-specific threshold |
| **Context Confusion** (too many tools) | Count active tools | Hard cap at 30; warn at 20; use tool loadout filtering |
| **Context Clash** (contradictory information) | Check skill instructions vs tool descriptions for overlapping names | Flag contradictions during consolidation; prefer more recent info |

---

## Provider-specific cache behavior

| Provider | Cache mechanism | Cache TTL | Cache key |
|---|---|---|---|
| Anthropic | Prompt prefix caching | 5 min (auto-extends) | Exact prefix match |
| OpenAI | Automatic (50%+ prefix match) | ~5-10 min | Longest common prefix |
| Google Gemini | 1M tokens | 64K-65K tokens | Native Google Search + function calling |

The pipeline's stable prefix architecture works with all three — the first 4 stages produce an identical prefix on every turn.

---

## Observability

Every stage emits a `ProcessorOutput` with:
- `tokens_added` / `tokens_removed`
- `items_included` / `items_excluded` (what was put in / left out)
- `duration` (how long the stage took)

These are logged as structured spans:

```rust
#[instrument(skip(ctx), fields(stage = %self.name(), tokens_before = ctx.token_count))]
fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
    // ... processing ...
    let output = ProcessorOutput { /* ... */ };
    tracing::info!(
        tokens_added = output.tokens_added,
        tokens_removed = output.tokens_removed,
        items = ?output.items_included,
        "Pipeline stage completed"
    );
    Ok(output)
}
```

When agent behavior degrades, inspect the pipeline logs to see exactly what context was included/excluded and why.
