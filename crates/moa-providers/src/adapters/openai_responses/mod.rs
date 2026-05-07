//! `OpenAI` Responses API request mapping and stream aggregation helpers.
//!
//! Internal adapter phases:
//! 1. build one provider request from MOA's `CompletionRequest`
//! 2. execute provider transport with retry handling
//! 3. normalize streamed provider events into `CompletionContent`
//! 4. finalize one normalized `CompletionResponse`
//! 5. record provider-private raw response details for tracing/debugging

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use async_openai::Client as OpenAiClient;
use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use async_openai::types::responses::ReasoningEffort;
use async_openai::types::responses::{
    CreateResponse, EasyInputContent, EasyInputMessage, FunctionCallOutput,
    FunctionCallOutputItemParam, FunctionTool, FunctionToolCall, InputContent, InputItem,
    InputParam, InputTextContent, Item, OutputItem, OutputMessageContent, PromptCacheRetention,
    Reasoning, Response, ResponseFormatJsonSchema, ResponseStream, ResponseStreamEvent,
    ResponseTextParam, ResponseUsage, Role as OpenAiRole, Status as OpenAiStatus,
    TextResponseFormatConfiguration, Tool, ToolChoiceOptions, ToolChoiceParam, WebSearchTool,
    WebSearchToolCallStatus,
};
use futures_util::StreamExt;
use moa_core::{
    CompletionContent, CompletionRequest, CompletionResponse, ContextMessage, JsonResponseFormat,
    MessageRole, MoaError, ModelId, ProviderNativeTool, Result, StopReason, TokenUsage,
    ToolCallContent, ToolContent, ToolInvocation, stable_prefix_fingerprint,
};
use reqwest::StatusCode;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::core::instrumentation::LLMSpanRecorder;
use crate::core::retry::RetryPolicy;
use crate::core::schema::compile_for_openai_strict;

const OPENAI_METADATA_VALUE_LIMIT: usize = 512;

mod request;
mod response;
mod streaming;
mod tools;

#[cfg(test)]
mod streaming_tests;
#[cfg(test)]
mod tests;

pub(crate) use request::{build_openai_client, build_responses_request};
pub(crate) use streaming::stream_responses_with_retry;

#[cfg(test)]
use response::token_usage_from_openai_usage;
#[cfg(test)]
use streaming::is_ignorable_openai_stream_error;
#[cfg(test)]
use tools::metadata_as_strings;
