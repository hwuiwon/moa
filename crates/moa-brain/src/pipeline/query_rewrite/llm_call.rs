//! LLM request construction and execution for query rewriting.

use std::collections::HashMap;

use moa_core::{
    CompletionRequest, JsonResponseFormat, ModelId, QueryRewriteResult, Result, WorkingContext,
};
use serde_json::json;

use super::QueryRewriter;
use super::input::RewriteInput;
use super::postprocess::{parse_rewrite_response, validate_rewrite_result};
use super::prompt::build_rewriter_prompt;

const REWRITER_OUTPUT_TOKENS: usize = 768;

impl QueryRewriter {
    pub(super) async fn rewrite(
        &self,
        input: &RewriteInput,
        ctx: &WorkingContext,
    ) -> Result<QueryRewriteResult> {
        let prompt = build_rewriter_prompt(input, ctx);
        let mut request = CompletionRequest::new(prompt);
        request.model = self
            .config
            .model
            .as_ref()
            .map(|model| ModelId::new(model.clone()));
        request.max_output_tokens = Some(REWRITER_OUTPUT_TOKENS);
        request.temperature = Some(0.0);
        request.response_format = Some(query_rewrite_response_format());
        request.metadata =
            HashMap::from([("moa.pipeline.stage".to_string(), json!("query_rewrite"))]);

        let stream = self.llm.complete(request).await?;
        let response = stream.collect().await?;
        let parsed = parse_rewrite_response(&response.text)?;
        Ok(validate_rewrite_result(parsed, input, ctx))
    }
}

fn query_rewrite_response_format() -> JsonResponseFormat {
    JsonResponseFormat::strict_json_schema(
        "query_rewrite_result",
        "Self-contained query rewrite, intent classification, and retrieval hints.",
        QueryRewriteResult::response_schema(),
    )
}
