//! Shared helpers for provider-native tool exposure and status blocks.

use moa_core::{CompletionContent, ModelCapabilities, ProviderNativeTool};

const WEB_SEARCH_TOOL_NAME: &str = "web_search";

/// Returns the native tools exposed by a model when provider-hosted tools are enabled.
pub(crate) fn enabled_native_tools(
    capabilities: &ModelCapabilities,
    enabled: bool,
) -> &[ProviderNativeTool] {
    if enabled {
        &capabilities.native_tools
    } else {
        &[]
    }
}

/// Returns a normalized block for provider-native web search progress.
pub(crate) fn web_search_started_block() -> CompletionContent {
    CompletionContent::ProviderToolResult {
        tool_name: WEB_SEARCH_TOOL_NAME.to_string(),
        summary: "Searching the web...".to_string(),
    }
}

/// Returns a normalized block for provider-native web search completion.
pub(crate) fn web_search_completed_block() -> CompletionContent {
    CompletionContent::ProviderToolResult {
        tool_name: WEB_SEARCH_TOOL_NAME.to_string(),
        summary: "Web search completed.".to_string(),
    }
}
