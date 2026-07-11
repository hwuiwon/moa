//! LLM request construction and execution for query rewriting.

use std::collections::HashMap;

use moa_core::{
    error::Result, types::completion::CompletionRequest, types::completion::JsonResponseFormat,
    types::context::ContextMessage, types::context::WorkingContext, types::identifiers::ModelId,
    types::query_rewrite::QueryRewriteResult, types::query_rewrite::RewriteReason,
};
use serde_json::json;

use super::QueryRewriter;
use super::input::RewriteInput;
use super::postprocess::{parse_rewrite_response, validate_rewrite_result};
use super::prompt::{REWRITER_SYSTEM_PROMPT, build_rewriter_user_prompt};

const REWRITER_OUTPUT_TOKENS: usize = 384;
const OPENAI_REASONING_EFFORT_METADATA_KEY: &str = "_moa.openai.reasoning_effort";

impl QueryRewriter {
    pub(super) async fn rewrite(
        &self,
        input: &RewriteInput,
        ctx: &WorkingContext,
        reason: RewriteReason,
    ) -> Result<QueryRewriteResult> {
        let request = CompletionRequest {
            model: self
                .config
                .model
                .as_ref()
                .map(|model| ModelId::new(model.clone())),
            messages: vec![
                ContextMessage::system(REWRITER_SYSTEM_PROMPT),
                ContextMessage::user(build_rewriter_user_prompt(input, ctx)),
            ],
            tools: Vec::new(),
            max_output_tokens: Some(REWRITER_OUTPUT_TOKENS),
            temperature: Some(0.0),
            response_format: Some(query_rewrite_response_format()),
            metadata: HashMap::from([
                ("moa.pipeline.stage".to_string(), json!("query_rewrite")),
                (
                    OPENAI_REASONING_EFFORT_METADATA_KEY.to_string(),
                    json!("none"),
                ),
            ]),
        };

        let stream = self.llm.complete(request).await?;
        let response = stream.collect().await?;
        let parsed = parse_rewrite_response(&response.text, reason)?;
        Ok(validate_rewrite_result(parsed, input, ctx, reason))
    }
}

fn query_rewrite_response_format() -> JsonResponseFormat {
    JsonResponseFormat::strict_json_schema(
        "query_rewrite_result",
        "Self-contained retrieval query and task-boundary signal.",
        QueryRewriteResult::response_schema(),
    )
}
