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
    error::MoaError, error::Result, types::completion::CompletionContent,
    types::completion::CompletionRequest, types::completion::CompletionResponse,
    types::completion::JsonResponseFormat, types::completion::StopReason,
    types::completion::TokenUsage, types::completion::ToolCallContent,
    types::completion::ToolInvocation, types::context::ContextMessage, types::context::MessageRole,
    types::identifiers::ModelId, types::model::ProviderNativeTool,
    types::observability::stable_prefix_fingerprint, types::tools::ToolContent,
};
use reqwest::StatusCode;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::core::instrumentation::LLMSpanRecorder;
use crate::core::retry::RetryPolicy;
use crate::core::schema::compile_for_openai_strict;

const OPENAI_METADATA_VALUE_LIMIT: usize = 512;

pub(crate) mod provider;
mod request;
mod response;
mod streaming;
mod tools;

#[cfg(test)]
mod streaming_tests;
#[cfg(test)]
mod tests;

pub use provider::{OpenAIProvider, debug_build_openai_request_body};
pub(crate) use request::build_responses_request;
pub(crate) use streaming::stream_responses_with_retry;

#[cfg(test)]
use response::token_usage_from_openai_usage;
#[cfg(test)]
use streaming::is_ignorable_openai_stream_error;
#[cfg(test)]
use tools::metadata_as_strings;
