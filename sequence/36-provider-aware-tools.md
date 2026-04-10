# Step 36: Provider-Aware Tool Results + Schema Compilation

## What this step is about
Two related gaps in the provider layer:

1. **Tool results are always flattened to text.** The brain harness calls `to_text()` on every `ToolOutput` before feeding it back to the LLM, regardless of provider. But Anthropic's Messages API natively accepts content block arrays in `tool_result` messages (text + image), so structured results are unnecessarily degraded for Claude.

2. **Tool schemas are sent with `strict: false`.** The OpenAI/OpenRouter providers send `strict: false` because MOA's canonical schemas include optional properties — incompatible with OpenAI's strict mode which requires all properties to be `required` + nullable. This disables structured output guarantees.

This step adds a provider-aware rendering layer for tool results and a per-provider schema compiler for tool definitions.

## Files to read
- `moa-core/src/types.rs` — `ToolOutput`, `ToolContent`, `ContextMessage`, `ToolCallFormat`, `CompletionRequest`
- `moa-brain/src/harness.rs` — `format_tool_output()`, where tool results are turned into context messages
- `moa-brain/src/pipeline/history.rs` — how `Event::ToolResult` is rendered for history
- `moa-providers/src/anthropic.rs` — `anthropic_message()`, tool schema formatting
- `moa-providers/src/common.rs` — `openai_tool_from_schema()`, `build_function_tool()` with `strict: Some(false)`
- `moa-providers/src/openai.rs` — OpenAI provider, schema handling
- `moa-providers/src/openrouter.rs` — OpenRouter provider, shares `common.rs`

## Goal
Anthropic sees native content blocks for tool results (not flattened text). OpenAI/OpenRouter receive strict-mode-compatible schemas. The canonical schema format in the tool registry stays unchanged — transformation happens at the provider boundary.

## Rules
- The **tool registry stores one canonical schema** per tool (standard JSON Schema, optional params as genuinely optional). No provider-specific schemas stored.
- **Schema compilation happens in the provider layer**, not the registry or brain. Each provider transforms the canonical schema at request time.
- **Tool result rendering is provider-aware.** The brain harness (or the provider request builder) must know whether the target provider accepts content blocks or only strings.
- Do NOT change `ToolOutput` or `ToolContent` — step 26 already made them content-block based. This step changes how they're serialized for each provider.
- The `ContextMessage.content` field is currently a `String`. For Anthropic's content block support, either:
  - Add a `content_blocks: Option<Vec<ToolContent>>` field to `ContextMessage` (preferred — backward compat)
  - Or change `content` to an enum (more invasive)
- MCP image content blocks (`ImageContent`) should flow through to Anthropic when the provider supports vision. For OpenAI, images in tool results should either be dropped with a text placeholder or converted to a data URI in the text.

## Tasks

### 1. Add content block support to `ContextMessage`
In `moa-core/src/types.rs`, extend `ContextMessage`:
```rust
pub struct ContextMessage {
    pub role: MessageRole,
    pub content: String,
    pub tools: Option<Value>,
    /// Structured content blocks for providers that support them (e.g., Anthropic tool_result).
    /// When present, providers that support content blocks use these instead of `content`.
    /// Providers that only support strings use `content` as the fallback.
    pub content_blocks: Option<Vec<ToolContent>>,
    /// Tool use ID for tool result messages (required by Anthropic and OpenAI).
    pub tool_use_id: Option<String>,
}
```

Add a constructor:
```rust
impl ContextMessage {
    pub fn tool_result(
        tool_use_id: String,
        text: String,
        blocks: Option<Vec<ToolContent>>,
    ) -> Self {
        Self {
            role: MessageRole::Tool,
            content: text,
            tools: None,
            content_blocks: blocks,
            tool_use_id: Some(tool_use_id),
        }
    }
}
```

### 2. Update the brain harness to preserve content blocks
In `moa-brain/src/harness.rs`, where tool results are turned into context messages:

```rust
// Instead of:
let text = format_tool_output(&output);
messages.push(ContextMessage::tool(text));

// Do:
let text = output.to_text(); // fallback for string-only providers
let blocks = Some(output.content.clone()); // preserve structured blocks
messages.push(ContextMessage::tool_result(tool_use_id, text, blocks));
```

### 3. Update the Anthropic provider to use content blocks
In `moa-providers/src/anthropic.rs`, update `anthropic_message()`:

```rust
fn anthropic_message(message: &ContextMessage) -> Value {
    if message.role == MessageRole::Tool {
        // Anthropic tool_result: use content blocks when available
        let content = if let Some(blocks) = &message.content_blocks {
            anthropic_content_blocks(blocks)
        } else {
            json!(message.content)
        };
        return json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": message.tool_use_id,
                "content": content,
            }]
        });
    }
    // ... existing handling for other roles
}

fn anthropic_content_blocks(blocks: &[ToolContent]) -> Value {
    let mut result = Vec::new();
    for block in blocks {
        match block {
            ToolContent::Text { text } => {
                result.push(json!({"type": "text", "text": text}));
            }
            ToolContent::Json { data } => {
                // Anthropic doesn't have a JSON content type — serialize to text
                result.push(json!({"type": "text", "text": data.to_string()}));
            }
            // Future: ToolContent::Image { ... } → base64 image block
        }
    }
    json!(result)
}
```

### 4. Update OpenAI/OpenRouter providers to use text fallback
In `moa-providers/src/common.rs`, the OpenAI message formatter should use the `content` string (already the text fallback), ignoring `content_blocks`. This is already the current behavior — just verify it works with the new `ContextMessage` shape.

For tool results specifically, OpenAI's Responses API needs `function_call_output` items. Make sure the `tool_use_id` is passed through.

### 5. Add schema compilation functions
Create `moa-providers/src/schema.rs` (or add to `common.rs`):

```rust
/// Compiles a canonical tool schema for OpenAI strict mode.
pub fn compile_for_openai_strict(schema: &Value) -> Value {
    let mut compiled = schema.clone();
    if let Some(params) = compiled.get_mut("parameters").or_else(|| compiled.get_mut("input_schema")) {
        make_strict_compatible(params);
    }
    compiled
}

/// Recursively transforms a JSON Schema object for OpenAI strict mode:
/// - Move all properties to `required`
/// - Change optional types to `["original_type", "null"]`
/// - Add `additionalProperties: false` to every object
/// - Strip validation-only keywords (minimum, maximum, pattern, minItems, maxItems)
fn make_strict_compatible(schema: &mut Value) {
    if let Some(obj) = schema.as_object_mut() {
        // Add additionalProperties: false
        obj.insert("additionalProperties".into(), json!(false));

        // Get all property names
        if let Some(properties) = obj.get("properties").and_then(Value::as_object) {
            let all_props: Vec<String> = properties.keys().cloned().collect();
            let required: Vec<String> = obj.get("required")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
                .unwrap_or_default();

            // Make non-required properties nullable
            if let Some(props) = obj.get_mut("properties").and_then(Value::as_object_mut) {
                for prop_name in &all_props {
                    if !required.contains(prop_name) {
                        if let Some(prop) = props.get_mut(prop_name) {
                            make_nullable(prop);
                        }
                    }
                }
            }

            // All properties are now required
            obj.insert("required".into(), json!(all_props));
        }

        // Strip validation-only keywords
        for keyword in &["minimum", "maximum", "pattern", "minItems", "maxItems", "minLength", "maxLength"] {
            obj.remove(*keyword);
        }

        // Recurse into nested properties
        if let Some(props) = obj.get_mut("properties").and_then(Value::as_object_mut) {
            for (_, prop_schema) in props.iter_mut() {
                make_strict_compatible(prop_schema);
            }
        }
    }
}

fn make_nullable(schema: &mut Value) {
    if let Some(obj) = schema.as_object_mut() {
        if let Some(type_val) = obj.get("type") {
            if type_val.is_string() {
                // "string" → ["string", "null"]
                obj.insert("type".into(), json!([type_val, "null"]));
            } else if type_val.is_array() {
                // already an array, add "null" if not present
                if let Some(arr) = obj.get_mut("type").and_then(Value::as_array_mut) {
                    if !arr.contains(&json!("null")) {
                        arr.push(json!("null"));
                    }
                }
            }
        }
    }
}
```

### 6. Update `build_function_tool` to use strict mode
In `moa-providers/src/common.rs`:

```rust
fn openai_tool_from_schema(schema: &Value) -> Result<Tool> {
    let compiled = compile_for_openai_strict(schema);
    // ... existing logic but with strict: Some(true) and compiled params
    build_function_tool(
        compiled.get("name"),
        compiled.get("description"),
        compiled.get("parameters").or_else(|| compiled.get("input_schema")),
        true, // strict
    )
}
```

### 7. Update Anthropic schema formatting
In `moa-providers/src/anthropic.rs`, the schema compilation is simpler — Anthropic expects `input_schema` at the top level and doesn't need the strict-mode transforms:

```rust
fn anthropic_tool_from_schema(schema: &Value) -> Value {
    json!({
        "name": schema.get("name"),
        "description": schema.get("description"),
        "input_schema": schema.get("parameters")
            .or_else(|| schema.get("input_schema"))
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
    })
}
```

### 8. Update history compiler for consistency
In `moa-brain/src/pipeline/history.rs`, when formatting `Event::ToolResult` into context, use `ContextMessage::tool_result()` with the tool_use_id so the provider layer gets the structured data.

## Deliverables
```
moa-core/src/types.rs              # ContextMessage + content_blocks + tool_use_id
moa-providers/src/schema.rs        # compile_for_openai_strict(), make_strict_compatible()
moa-providers/src/common.rs        # Use strict: true, compiled schemas
moa-providers/src/anthropic.rs     # Native content blocks for tool_result, input_schema format
moa-brain/src/harness.rs           # Preserve content blocks in ContextMessage
moa-brain/src/pipeline/history.rs  # Use tool_result() constructor
```

## Acceptance criteria
1. Anthropic provider sends tool results as content block arrays (not flattened text).
2. OpenAI/OpenRouter provider sends `strict: true` with compiled schemas.
3. Optional schema properties are compiled to required + nullable for OpenAI.
4. `additionalProperties: false` is added to all objects in OpenAI schemas.
5. Validation-only keywords (`minimum`, `pattern`, etc.) are stripped for OpenAI.
6. Anthropic schemas use `input_schema` at the top level.
7. The canonical schema in the tool registry is unchanged — compilation is provider-side.
8. `ContextMessage::tool_result()` preserves both text fallback and content blocks.
9. All existing tests pass.

## Tests

**Unit tests (moa-providers/src/schema.rs):**
- `compile_for_openai_strict`: optional param → required + nullable
- `compile_for_openai_strict`: nested objects get `additionalProperties: false`
- `compile_for_openai_strict`: `minimum`/`maximum` stripped
- `compile_for_openai_strict`: already-required params stay required (not double-nullable)
- `compile_for_openai_strict`: array types with existing null → no duplicate null
- Round-trip: compile schema → validate it matches OpenAI strict requirements

**Unit tests (moa-providers/src/anthropic.rs):**
- `anthropic_content_blocks`: text block → `{"type": "text", "text": "..."}`
- `anthropic_content_blocks`: json block → serialized to text block
- `anthropic_message` with tool role → wraps in `tool_result` with `tool_use_id`
- `anthropic_tool_from_schema`: canonical schema → `input_schema` at top level

**Unit tests (moa-core):**
- `ContextMessage::tool_result()` stores both content and content_blocks
- `ContextMessage::tool()` (old constructor) still works with None content_blocks

**Integration:**
- Brain turn with Anthropic provider: tool result flows as content blocks
- Brain turn with OpenAI provider: tool result flows as text string
- Tool schemas pass OpenAI strict validation (mock or real API)

```bash
cargo test -p moa-core
cargo test -p moa-providers
cargo test -p moa-brain
```
