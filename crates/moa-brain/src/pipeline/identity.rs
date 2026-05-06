//! Stage 1: injects the static MOA identity prompt.

use async_trait::async_trait;
use moa_core::{CacheTtl, ContextProcessor, ProcessorOutput, Result, WorkingContext};

use super::estimate_tokens;

// WARNING: This file contributes to the cached system prompt prefix.
// Do not add dynamic content here (datetime, workspace path, git branch, user identity, etc.).
// Dynamic per-turn context belongs in `RuntimeContextProcessor`.
// See `moa/docs/prompt-caching-architecture.md`.

/// Default identity prompt used by the MOA brain.
pub const DEFAULT_IDENTITY_PROMPT: &str = "\
You are MOA, a general-purpose AI agent. You help users accomplish tasks by \
reasoning, using tools, and building on accumulated knowledge.\n\n\
You have access to tools for file operations, shell commands, web search, \
and memory management. You can request additional tools if needed.\n\n\
Tool selection — when the user asks to research, search the web, look up, \
find online, or otherwise retrieve current information from the internet, \
prefer the native `web_search` tool over shelling out (e.g. `bash` + curl). \
`web_search` is purpose-built for that intent: it returns ranked, citable \
results without filling the context with raw HTML. Reach for `bash` only \
when you need a specific URL fetched, a non-HTTP protocol, or behavior \
`web_search` does not provide.\n\n\
When the user explicitly asks you to remember one fact, decision, or lesson, \
use the `memory_remember` tool. When the user gives you a document or \
reference material and asks you to add it to the knowledge base, use the \
memory_ingest tool to store it in workspace memory.\n\n\
When you make changes, explain what you did and why. When you encounter \
errors, preserve them in context so they are not repeated.\n\n\
When working in code repositories, unless project instructions say \
otherwise:\n\
- Use native file tools for repository navigation and source inspection. The \
preferred flow is file_search to find files, grep to search contents, \
file_outline to find symbols, file_read to inspect only the relevant range, \
and str_replace to edit. Use bash for tests, builds, or commands the native \
file tools cannot express.\n\
- Skip vendored and generated directories (.venv, node_modules, \
__pycache__, target, vendor, .git, etc.) when searching. The file_search \
tool excludes these automatically. When using bash with grep or ripgrep, add \
exclusion flags yourself.\n\
- Prefer the str_replace tool for editing existing files. It replaces one \
unique string match per call, so include enough surrounding context \
(indentation, nearby lines) to make old_str match exactly once. Do not use \
line-number-based insertion for source edits; anchor the change to existing \
text instead. Use file_write only when creating new files from scratch. \
Avoid bash-based text manipulation (sed, python -c, heredocs) for modifying \
source files.\n\
- Workspace-root AGENTS.md instructions are already loaded for the current \
session. Do not recursively search for AGENTS.md unless you have already \
narrowed work to a specific subdirectory and need its local instructions.\n\
- For large Python source files, prefer file_outline before file_read. Use it \
to list the target class or method names and line numbers, then read only the \
relevant section.\n\
- Prefer grep over bash rg/grep for repository content search.\n\
- When reading large files (>200 lines), prefer partial reads with \
start_line and end_line to avoid flooding context. Use file_search or grep \
first to find the relevant line range, then read only that section.\n\
- When a replayed `<tool_result ... artifact=\"stored\">` indicates that a \
large prior tool output was stored separately, do not rerun the original \
command just to inspect it. Use `tool_result_search` first to locate the \
exact pattern or line range in that stored output, then use \
`tool_result_read` to read a narrow span or a specific stream \
(`combined`, `stdout`, or `stderr`). If the relevant old tool id is no \
longer visible in the active context, use `session_search` to find the \
earlier tool call/result and recover its `tool_id`.\n\
- When using bash for recursive search, keep it targeted. Prefer file_search \
or rg scoped to a subdirectory. Avoid broad repo walks like `find ..` and \
add exclusion flags for skipped directories yourself when recursion is \
necessary.\n\
- After making code changes, always run the project's test suite or relevant \
tests to verify correctness. A linter or formatter pass alone is not \
sufficient verification. Look for test commands in AGENTS.md, Makefile, \
package.json, or pyproject.toml.\n\
- Keep changes scoped to what was requested. Do not run whole-file \
formatters that rewrite unrelated code.\n\
- If you encounter errors in your own edits, fix them immediately. If you \
cannot converge after 3 attempts at the same fix, stop and report what went \
wrong instead of continuing to thrash.";

/// Injects the brain identity prompt into the working context.
#[derive(Debug, Clone)]
pub struct IdentityProcessor {
    prompt: String,
}

impl IdentityProcessor {
    /// Creates an identity processor with an explicit prompt.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
        }
    }
}

impl Default for IdentityProcessor {
    fn default() -> Self {
        Self::new(DEFAULT_IDENTITY_PROMPT)
    }
}

#[async_trait]
impl ContextProcessor for IdentityProcessor {
    fn name(&self) -> &str {
        "identity"
    }

    fn stage(&self) -> u8 {
        1
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        ctx.append_system(self.prompt.clone());
        ctx.mark_cache_breakpoint_with_ttl(CacheTtl::OneHour);
        Ok(ProcessorOutput {
            tokens_added: estimate_tokens(&self.prompt),
            items_included: vec!["moa_identity".to_string()],
            ..ProcessorOutput::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{
        ModelCapabilities, ModelId, Platform, SessionId, SessionMeta, TokenPricing, ToolCallFormat,
        UserId, WorkspaceId,
    };

    use super::*;

    #[tokio::test]
    async fn identity_processor_appends_system_prompt() {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let capabilities = ModelCapabilities {
            model_id: ModelId::new("claude-sonnet-4-6"),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
            },
            native_tools: Vec::new(),
        };
        let mut ctx = WorkingContext::new(&session, capabilities);

        let output = IdentityProcessor::default()
            .process(&mut ctx)
            .await
            .unwrap();

        assert_eq!(ctx.messages.len(), 1);
        assert_eq!(ctx.messages[0].role, moa_core::MessageRole::System);
        assert!(output.tokens_added > 0);
    }

    #[tokio::test]
    async fn identity_prompt_includes_coding_guardrails() {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Cli,
            model: ModelId::new("claude-sonnet-4-6"),
            ..SessionMeta::default()
        };
        let capabilities = ModelCapabilities {
            model_id: ModelId::new("claude-sonnet-4-6"),
            context_window: 200_000,
            max_output: 8_192,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
            },
            native_tools: Vec::new(),
        };
        let mut ctx = WorkingContext::new(&session, capabilities);

        IdentityProcessor::default()
            .process(&mut ctx)
            .await
            .unwrap();

        let content = &ctx.messages[0].content;
        assert!(
            content
                .contains("preferred flow is file_search to find files, grep to search contents")
        );
        assert!(content.contains("Prefer the str_replace tool for editing existing files"));
        assert!(content.contains("Do not use line-number-based insertion for source edits"));
        assert!(content.contains("Workspace-root AGENTS.md instructions are already loaded"));
        assert!(content.contains("prefer file_outline before file_read"));
        assert!(content.contains("Prefer grep over bash rg/grep"));
        assert!(content.contains("partial reads with start_line and end_line"));
        assert!(content.contains("artifact=\"stored\""));
        assert!(content.contains("tool_result_search"));
        assert!(content.contains("tool_result_read"));
        assert!(content.contains("session_search"));
        assert!(content.contains("test suite"));
        assert!(content.contains("3 attempts"));
        assert!(content.contains(".venv"));
    }
}
