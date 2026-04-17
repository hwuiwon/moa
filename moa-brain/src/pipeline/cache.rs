//! Stage 8: plans explicit prompt-cache breakpoints and reports cache efficiency.

use async_trait::async_trait;
use moa_core::{
    CacheBreakpoint, CacheBreakpointTarget, CacheTtl, ContextProcessor, MessageRole, MoaError,
    ProcessorOutput, Result, WorkingContext,
};

use crate::pipeline::history::HISTORY_END_INDEX_METADATA_KEY;

use super::{estimate_tokens, sort_json_keys};

const MAX_CACHE_BREAKPOINTS: usize = 4;
const MIN_BLOCKS_BEFORE_BP4: usize = 3;
const MAX_BLOCKS_AFTER_BP4: usize = 18;

/// Final cache-planning pass.
#[derive(Debug, Default)]
pub struct CacheOptimizer;

#[async_trait]
impl ContextProcessor for CacheOptimizer {
    fn name(&self) -> &str {
        "cache"
    }

    fn stage(&self) -> u8 {
        9
    }

    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
        if let Some(history_end) = history_end_index(ctx)
            && history_end > ctx.messages.len()
        {
            return Err(MoaError::ValidationError(
                "history end index exceeds the number of compiled messages".to_string(),
            ));
        }

        let planned_cache_controls = plan_cache_controls(ctx);
        ctx.cache_controls = planned_cache_controls.clone();
        ctx.cache_breakpoints = planned_cache_controls
            .iter()
            .filter_map(CacheBreakpoint::message_index)
            .collect();

        let Some(cache_breakpoint) = ctx.cache_breakpoints.last().copied() else {
            return Err(MoaError::ValidationError(
                "cache breakpoint must be set before the cache optimizer runs".to_string(),
            ));
        };
        if cache_breakpoint > ctx.messages.len() {
            return Err(MoaError::ValidationError(
                "cache breakpoint exceeds the number of compiled messages".to_string(),
            ));
        }

        for message in &mut ctx.messages[..cache_breakpoint] {
            if let Some(tools) = &mut message.tools {
                sort_json_keys(tools);
            }
        }

        for schema in ctx.tools_mut() {
            sort_json_keys(schema);
        }

        let prefix_tokens = ctx.messages[..cache_breakpoint]
            .iter()
            .map(|message| estimate_tokens(&message.content))
            .sum::<usize>();
        let tool_tokens = ctx
            .tools()
            .iter()
            .map(|schema| estimate_tokens(&schema.to_string()))
            .sum::<usize>();
        let total_tokens = (ctx.token_count + tool_tokens).max(1);
        let cache_ratio = (prefix_tokens + tool_tokens) as f64 / total_tokens as f64;

        tracing::info!(
            cache_breakpoint,
            cache_control_count = ctx.cache_controls.len(),
            prefix_tokens,
            tool_tokens,
            total_tokens,
            cache_ratio = %format!("{:.1}%", cache_ratio * 100.0),
            "context cache efficiency"
        );

        Ok(ProcessorOutput::default())
    }
}

fn plan_cache_controls(ctx: &WorkingContext) -> Vec<CacheBreakpoint> {
    let static_message_breakpoints = static_message_breakpoints(ctx);
    let conversation_breakpoint = place_bp4_conversation(ctx);

    let mut planned = Vec::with_capacity(MAX_CACHE_BREAKPOINTS);
    if !ctx.tools().is_empty() {
        planned.push(CacheBreakpoint::tools(CacheTtl::OneHour));
    }
    planned.extend(
        static_message_breakpoints
            .into_iter()
            .map(|index| CacheBreakpoint::message(index, CacheTtl::OneHour)),
    );
    if let Some(index) = conversation_breakpoint {
        planned.push(CacheBreakpoint::message(index, CacheTtl::FiveMinutes));
    }

    planned.sort_by_key(cache_breakpoint_sort_key);
    planned.dedup();

    if planned.len() > MAX_CACHE_BREAKPOINTS {
        let mut trimmed = Vec::with_capacity(MAX_CACHE_BREAKPOINTS);
        let mut message_breakpoints = planned
            .iter()
            .filter(|breakpoint| {
                matches!(
                    breakpoint.target,
                    CacheBreakpointTarget::MessageBoundary { .. }
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        message_breakpoints.sort_by_key(cache_breakpoint_sort_key);

        if let Some(tool_breakpoint) = planned
            .iter()
            .find(|breakpoint| matches!(breakpoint.target, CacheBreakpointTarget::ToolDefinitions))
            .cloned()
        {
            trimmed.push(tool_breakpoint);
        }
        if let Some(identity_breakpoint) = message_breakpoints.first().cloned() {
            trimmed.push(identity_breakpoint);
        }
        if let Some(conversation_breakpoint) = message_breakpoints
            .iter()
            .find(|breakpoint| breakpoint.ttl == CacheTtl::FiveMinutes)
            .cloned()
            && !trimmed.contains(&conversation_breakpoint)
        {
            trimmed.push(conversation_breakpoint);
        }
        if let Some(workspace_breakpoint) = message_breakpoints
            .iter()
            .rev()
            .find(|breakpoint| breakpoint.ttl == CacheTtl::OneHour)
            .cloned()
            && !trimmed.contains(&workspace_breakpoint)
        {
            trimmed.push(workspace_breakpoint);
        }

        trimmed.sort_by_key(cache_breakpoint_sort_key);
        trimmed.dedup();
        return trimmed;
    }

    planned
}

fn static_message_breakpoints(ctx: &WorkingContext) -> Vec<usize> {
    let history_end = history_end_index(ctx).unwrap_or(ctx.messages.len());
    let mut breakpoints = ctx
        .cache_controls
        .iter()
        .filter(|breakpoint| breakpoint.ttl == CacheTtl::OneHour)
        .filter_map(CacheBreakpoint::message_index)
        .filter(|index| *index > 0 && *index <= history_end)
        .collect::<Vec<_>>();
    if breakpoints.is_empty() {
        breakpoints.extend(
            ctx.cache_breakpoints
                .iter()
                .copied()
                .filter(|index| *index > 0 && *index <= history_end),
        );
    }
    breakpoints.sort_unstable();
    breakpoints.dedup();
    breakpoints
}

fn place_bp4_conversation(ctx: &WorkingContext) -> Option<usize> {
    let history_end = history_end_index(ctx)?.min(ctx.messages.len());
    if history_end < MIN_BLOCKS_BEFORE_BP4 {
        return None;
    }

    let static_floor = static_message_breakpoints(ctx)
        .into_iter()
        .max()
        .unwrap_or_default();
    let latest_candidate = latest_conversation_cache_candidate(&ctx.messages[..history_end])?;
    if latest_candidate <= static_floor {
        return None;
    }

    let blocks_after = history_end.saturating_sub(latest_candidate);
    if blocks_after <= MAX_BLOCKS_AFTER_BP4 {
        return Some(latest_candidate);
    }

    replan_on_lookback_overflow(&ctx.messages[..history_end], static_floor)
}

fn replan_on_lookback_overflow(
    messages: &[moa_core::ContextMessage],
    floor: usize,
) -> Option<usize> {
    let history_end = messages.len();
    messages
        .iter()
        .enumerate()
        .rev()
        .find(|(index, message)| {
            *index + 1 > floor
                && history_end.saturating_sub(index + 1) <= MAX_BLOCKS_AFTER_BP4
                && matches!(message.role, MessageRole::Assistant | MessageRole::Tool)
        })
        .map(|(index, _)| index + 1)
}

fn latest_conversation_cache_candidate(messages: &[moa_core::ContextMessage]) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| matches!(message.role, MessageRole::Assistant | MessageRole::Tool))
        .map(|(index, _)| index + 1)
}

fn history_end_index(ctx: &WorkingContext) -> Option<usize> {
    ctx.metadata()
        .get(HISTORY_END_INDEX_METADATA_KEY)
        .and_then(serde_json::Value::as_u64)
        .map(|index| index as usize)
}

fn cache_breakpoint_sort_key(breakpoint: &CacheBreakpoint) -> (usize, usize) {
    match breakpoint.target {
        CacheBreakpointTarget::ToolDefinitions => (0, 0),
        CacheBreakpointTarget::MessageBoundary { index } => (1, index),
    }
}

#[cfg(test)]
mod tests {
    use moa_core::{
        ContextMessage, ModelCapabilities, ModelId, Platform, SessionId, SessionMeta, TokenPricing,
        ToolCallFormat, UserId, WorkspaceId,
    };
    use serde_json::json;

    use super::*;

    fn test_context() -> WorkingContext {
        let session = SessionMeta {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new("workspace"),
            user_id: UserId::new("user"),
            platform: Platform::Desktop,
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
        WorkingContext::new(&session, capabilities)
    }

    #[tokio::test]
    async fn cache_optimizer_plans_tool_static_and_conversation_breakpoints() {
        let mut ctx = test_context();
        ctx.append_system("identity");
        ctx.mark_cache_breakpoint_with_ttl(CacheTtl::OneHour);
        ctx.append_system("workspace");
        ctx.mark_cache_breakpoint_with_ttl(CacheTtl::OneHour);
        ctx.set_tools(vec![json!({"name":"bash"})]);
        ctx.extend_messages(vec![
            ContextMessage::assistant("previous reply"),
            ContextMessage::tool_result("toolu_1", "tool output", None),
            ContextMessage::user("current question"),
        ]);
        ctx.insert_metadata(HISTORY_END_INDEX_METADATA_KEY, json!(4));

        CacheOptimizer.process(&mut ctx).await.unwrap();

        assert_eq!(
            ctx.cache_controls,
            vec![
                CacheBreakpoint::tools(CacheTtl::OneHour),
                CacheBreakpoint::message(1, CacheTtl::OneHour),
                CacheBreakpoint::message(2, CacheTtl::OneHour),
                CacheBreakpoint::message(4, CacheTtl::FiveMinutes),
            ]
        );
        assert_eq!(ctx.cache_breakpoints, vec![1, 2, 4]);
    }

    #[tokio::test]
    async fn cache_optimizer_skips_conversation_breakpoint_for_short_sessions() {
        let mut ctx = test_context();
        ctx.append_system("identity");
        ctx.mark_cache_breakpoint_with_ttl(CacheTtl::OneHour);
        ctx.extend_messages(vec![
            ContextMessage::assistant("only one turn"),
            ContextMessage::user("current question"),
        ]);
        ctx.insert_metadata(HISTORY_END_INDEX_METADATA_KEY, json!(2));

        CacheOptimizer.process(&mut ctx).await.unwrap();

        assert_eq!(
            ctx.cache_controls,
            vec![CacheBreakpoint::message(1, CacheTtl::OneHour)]
        );
        assert_eq!(ctx.cache_breakpoints, vec![1]);
    }

    #[test]
    fn replan_on_lookback_overflow_moves_to_recent_conversation_boundary() {
        let messages = (0..25)
            .map(|index| {
                if index % 2 == 0 {
                    ContextMessage::assistant(format!("assistant {index}"))
                } else {
                    ContextMessage::tool_result(format!("toolu_{index}"), "tool", None)
                }
            })
            .collect::<Vec<_>>();

        let replanned = replan_on_lookback_overflow(&messages, 0).expect("replanned breakpoint");
        assert!(messages.len().saturating_sub(replanned) <= MAX_BLOCKS_AFTER_BP4);
    }
}
