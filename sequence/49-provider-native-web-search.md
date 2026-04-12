# Step 49 — Provider-Native Web Search

_Remove web_search/web_fetch stubs. Pass through the LLM provider's built-in search capability instead of implementing our own._

---

## 1. What this step is about

MOA currently registers `web_search` and `web_fetch` as stub tools that return `Err(Unsupported)`. Rather than implementing a custom search API integration (Brave, SearXNG, etc.), this step leverages the LLM provider's built-in web search — Anthropic's `web_search_20250305` tool type, OpenAI's web search, and similar.

The LLM already knows how to search the web. We just need to stop blocking it.

The approach: when building the `CompletionRequest`, include the provider's native search tool definition alongside MOA's custom tools. The provider handles search execution internally — MOA never sees the raw search API call. Search results flow back as normal assistant content.

---

## 2. Files/directories to read

- **`moa-hands/src/tools/stub.rs`** — The stubs to remove.
- **`moa-hands/src/router.rs`** — Where stubs are registered (lines 227–246). The default loadout includes `web_search` and `web_fetch`.
- **`moa-providers/src/anthropic.rs`** — `build_request_body()` function (~line 243). Where the `tools` array is built for the API request. This is where provider-native tools need to be injected.
- **`moa-providers/src/openai.rs`** — Same pattern for OpenAI.
- **`moa-providers/src/openrouter.rs`** — Same pattern for OpenRouter.
- **`moa-core/src/types.rs`** — `CompletionRequest`, `ModelCapabilities`. Need to express provider-native tool support.
- **`moa-brain/src/pipeline/tools.rs`** — `ToolDefinitionProcessor`. May need to handle native tools differently from custom tools.
- **`moa-core/src/config.rs`** — Need a config flag for enabling/disabling web search per provider.

Also reference:
- Anthropic API docs: the `web_search_20250305` tool type uses `{"type": "web_search_20250305", "name": "web_search"}` in the tools array — NOT the standard `{"type": "function"}` format used for custom tools.
- OpenAI API docs: web search as a built-in tool.

---

## 3. Goal

After this step:
1. The brain can ask the LLM to search the web, and it works.
2. No MOA code calls any external search API — the LLM provider handles it.
3. Search results appear as normal assistant content in the conversation, visible in the TUI/messaging.
4. The web_search and web_fetch stubs are removed entirely.
5. Provider-native tools are clearly separated from MOA's custom tools in the request format.

---

## 4. Rules

- **Do not implement a search API client.** The whole point is to delegate to the LLM provider.
- **Provider-native tools are NOT in the ToolRouter.** They don't go through MOA's approval/execution pipeline. They're handled entirely by the LLM provider's API. MOA just includes them in the `CompletionRequest` and receives results.
- **Provider-native tools must be in the correct format.** Anthropic's native tools use `{"type": "web_search_20250305"}`, not `{"type": "function"}`. OpenAI has its own format. Each provider serializes its native tools differently.
- **Configurable.** Some deployments may want to disable web search (airgapped environments, cost control). Add a config flag.
- **Search results are not tool calls from MOA's perspective.** The LLM calls the search internally and incorporates results into its response. MOA sees the response text, not intermediate search calls. However, some providers expose search as visible tool_use blocks — handle both cases.
- **Remove the stubs cleanly.** Delete `stub.rs`, remove `web_search` and `web_fetch` from the default loadout, remove all references.

---

## 5. Tasks

### 5a. Add `ProviderNativeTool` concept to `moa-core`

```rust
/// A tool provided natively by the LLM provider, not executed by MOA.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderNativeTool {
    /// The provider-specific tool type identifier.
    /// e.g., "web_search_20250305" for Anthropic.
    pub tool_type: String,
    /// Human-readable name.
    pub name: String,
    /// Optional provider-specific configuration.
    pub config: Option<serde_json::Value>,
}
```

Add to `ModelCapabilities`:
```rust
pub struct ModelCapabilities {
    // ... existing fields ...
    /// Provider-native tools supported by this model.
    pub native_tools: Vec<ProviderNativeTool>,
}
```

### 5b. Add web search config

In `moa-core/src/config.rs`, add to `GeneralConfig` or a new section:

```toml
[general]
web_search_enabled = true  # Allow LLM to use its native web search
```

### 5c. Update `AnthropicProvider` to declare and inject native search

In `moa-providers/src/anthropic.rs`:

1. Add `web_search_20250305` to `ModelCapabilities::native_tools` for models that support it.
2. In `build_request_body()`, when building the `tools` array, append provider-native tools AFTER MOA's custom tools:

```rust
// After custom tools are serialized...
if config.web_search_enabled {
    for native_tool in &capabilities.native_tools {
        tools_array.push(json!({
            "type": native_tool.tool_type,
            "name": native_tool.name,
        }));
    }
}
```

3. In the response stream parser, handle `web_search` content blocks. Anthropic returns search results as `content_block` events with `type: "web_search_tool_result"`. These should be passed through as assistant content — the brain doesn't need to act on them.

### 5d. Update `OpenAIProvider` and `OpenRouterProvider`

Same pattern — declare native search tools in capabilities, inject into request, handle response format differences.

For OpenAI, the web search tool format differs. Check the current API docs and implement accordingly.

For OpenRouter, it depends on the downstream provider — pass through whatever the routed model supports.

### 5e. Handle provider search tool_use blocks in the harness

When the LLM response contains a tool_use block for a provider-native tool (like `web_search`), the brain harness must NOT route it to the ToolRouter. Currently, `moa-brain/src/harness.rs` treats every `CompletionContent::ToolCall` as something to execute. Add a check:

```rust
match block {
    CompletionContent::ToolCall(call) => {
        if is_provider_native_tool(&call.name, &capabilities) {
            // Provider handles this internally — just record as an event
            store.emit_event(session_id, Event::ProviderToolUse {
                tool_name: call.name.clone(),
                input_summary: truncate(&call.input, 200),
            }).await?;
            // Results come back as part of the next content block
            continue;
        }
        // ... normal MOA tool execution path ...
    }
}
```

Or better: the provider should handle these internally and never surface them as `ToolCall` content blocks to the harness. The provider stream parser can filter them out and incorporate search results into the text response.

### 5f. Remove stubs

1. Delete `moa-hands/src/tools/stub.rs`
2. Remove `pub mod stub;` from `moa-hands/src/tools/mod.rs`
3. Remove the two `StubTool::new("web_search", ...)` and `StubTool::new("web_fetch", ...)` registrations from `moa-hands/src/router.rs`
4. Remove `"web_search"` and `"web_fetch"` from the default loadout in `ToolRouter::default_loadout()`
5. If `stub.rs` was only used for web tools, the entire file goes away. If other stubs exist, keep the module.

### 5g. Add a `ProviderToolUse` event type (optional)

If you want visibility into when the LLM uses its built-in search:

```rust
Event::ProviderToolUse {
    tool_name: String,
    input_summary: String,
}
```

This appears in the session log so the TUI/messaging can show "🔍 Searching the web..." status.

---

## 6. How it should be implemented

The cleanest approach is to handle provider-native tools entirely within the provider layer. The stream parser in `moa-providers/src/anthropic.rs` already processes content blocks. When it encounters a `web_search_tool_result` block, it can:

1. Emit it as a `CompletionContent::Text` with the search results formatted as text
2. Or emit it as a new `CompletionContent::ProviderToolResult` variant that the harness logs but doesn't act on

Option 1 is simpler — the brain sees search results as part of the assistant's text response, exactly as it would in the Anthropic console. The harness needs zero changes.

Option 2 gives more observability — you can see exactly when search was used and what it found. Better for the eval framework.

Recommendation: **Option 2** — add `CompletionContent::ProviderToolResult { tool_name, result_summary }` and handle it in the harness as a no-op (just emit an event, don't route to ToolRouter).

---

## 7. Deliverables

- [ ] `moa-core/src/types.rs` — `ProviderNativeTool` struct, added to `ModelCapabilities`
- [ ] `moa-core/src/types.rs` — `CompletionContent::ProviderToolResult` variant (optional)
- [ ] `moa-core/src/events.rs` — `Event::ProviderToolUse` variant (optional)
- [ ] `moa-core/src/config.rs` — `web_search_enabled` config flag
- [ ] `moa-providers/src/anthropic.rs` — Native search tool injection in `build_request_body()`, response parsing for search result blocks
- [ ] `moa-providers/src/openai.rs` — Same for OpenAI
- [ ] `moa-providers/src/openrouter.rs` — Same for OpenRouter (pass-through)
- [ ] `moa-brain/src/harness.rs` — Handle `ProviderToolResult` blocks (if Option 2)
- [ ] `moa-hands/src/tools/stub.rs` — **Deleted**
- [ ] `moa-hands/src/router.rs` — Stubs removed from registration and default loadout

---

## 8. Acceptance criteria

1. **Web search works.** `moa exec "What happened in the news today?"` returns current information from the web.
2. **No MOA search API calls.** MOA makes zero HTTP requests to any search engine. The LLM provider handles it.
3. **Stubs are gone.** `stub.rs` deleted. No `StubTool` references remain.
4. **Search visible in TUI.** When the LLM searches, the TUI shows a status indicator (via `ProviderToolUse` event or text content).
5. **Configurable.** `web_search_enabled = false` in config prevents search tool from being included.
6. **No approval needed.** Provider-native tools don't go through MOA's approval flow — they're executed by the provider, not by MOA.
7. **Custom tools still work.** `bash`, `file_read`, etc. continue to function through the normal ToolRouter path.
8. **All existing tests pass.** Removing stubs doesn't break anything.

---

## 9. Testing

**Test 1:** `native_tools_in_anthropic_request` — Build a CompletionRequest with web search enabled, verify the serialized body includes `{"type": "web_search_20250305"}` alongside custom tool definitions.

**Test 2:** `native_tools_excluded_when_disabled` — Set `web_search_enabled = false`, verify no native tools in request body.

**Test 3:** `stubs_removed_from_registry` — Create a `ToolRouter::with_defaults()`, verify `web_search` and `web_fetch` are NOT registered as tools.

**Test 4:** `provider_tool_result_not_routed` — Simulate a `CompletionContent::ProviderToolResult` in the harness, verify it emits an event but does NOT call `ToolRouter::execute()`.

**Test 5:** `model_capabilities_include_native_tools` — Verify `AnthropicProvider::capabilities()` lists `web_search_20250305` in `native_tools`.

**Test 6 (integration, requires API key):** `e2e_web_search_works` — Run `moa exec "What is the current weather in NYC?"`, verify response contains current information (not just training data).

---

## 10. Additional notes

- **Why not implement our own search?** Three reasons: (a) LLM providers optimize search for their models — query reformulation, result ranking, citation formatting are all tuned. We can't match that. (b) No API key management for a search provider. (c) No additional cost — Anthropic includes web search in the per-token price.
- **Anthropic's web search format.** The tool is declared as `{"type": "web_search_20250305", "name": "web_search"}` (not `type: function`). Results come back as a content block with `type: "web_search_tool_result"` containing `search_results` array with `url`, `title`, `snippet`, and `page_content` fields. The LLM then synthesizes these into its response.
- **Citation handling.** Anthropic's search results include citations. These appear as text in the assistant response. MOA doesn't need special citation handling — just pass through the text.
- **web_fetch removal.** With the LLM's native search, there's no need for a separate `web_fetch` tool. The LLM can browse and read pages as part of its search capability. If a user explicitly needs to fetch a specific URL, that can be a future addition — but the stub pretending it works is worse than not having it.
- **OpenAI/OpenRouter specifics.** Check the current API docs for each provider's native search format. The pattern is the same — declare the tool type, inject into the request, handle response blocks — but the serialization differs.
