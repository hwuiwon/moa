//! Suite-agnostic provider cost accounting for evaluation lanes.

use serde::{Deserialize, Serialize};

/// Date when the Cohere pricing constants in this module were last checked.
pub const PRICING_AS_OF: &str = "2026-06-11";

/// Estimated Cohere Embed v4 text input price in USD per million tokens.
pub const COHERE_EMBED_V4_INPUT_USD_PER_MILLION_TOKENS: f64 = 0.12;

/// Estimated Cohere Command A input price in USD per million tokens.
pub const COHERE_COMMAND_A_INPUT_USD_PER_MILLION_TOKENS: f64 = 2.50;

/// Estimated Cohere Command A output price in USD per million tokens.
pub const COHERE_COMMAND_A_OUTPUT_USD_PER_MILLION_TOKENS: f64 = 10.00;

/// Estimated Cohere Rerank v4 Fast price in USD per search.
pub const COHERE_RERANK_V4_FAST_USD_PER_SEARCH: f64 = 0.002;

/// Shared token and cost ledger for a live eval run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostLedger {
    /// Pricing date used for the estimate constants.
    pub pricing_as_of: String,
    /// Estimated embedding input tokens.
    pub embed_input_tokens: u64,
    /// Estimated chat input tokens.
    pub chat_input_tokens: u64,
    /// Estimated chat output tokens.
    pub chat_output_tokens: u64,
    /// Number of rerank searches.
    pub rerank_calls: u64,
    /// Estimated total cost in USD.
    pub est_usd: f64,
    /// Configured budget ceiling in USD.
    pub budget_usd: f64,
}

impl CostLedger {
    /// Creates an empty ledger with the provided budget ceiling.
    #[must_use]
    pub fn new(budget_usd: f64) -> Self {
        Self {
            pricing_as_of: PRICING_AS_OF.to_string(),
            embed_input_tokens: 0,
            chat_input_tokens: 0,
            chat_output_tokens: 0,
            rerank_calls: 0,
            est_usd: 0.0,
            budget_usd,
        }
    }

    /// Records estimated embedding input tokens.
    pub fn record_embed(&mut self, input_tokens: u64) {
        self.embed_input_tokens = self.embed_input_tokens.saturating_add(input_tokens);
        self.refresh_estimate();
    }

    /// Records estimated chat input and output tokens.
    pub fn record_chat(&mut self, input_tokens: u64, output_tokens: u64) {
        self.chat_input_tokens = self.chat_input_tokens.saturating_add(input_tokens);
        self.chat_output_tokens = self.chat_output_tokens.saturating_add(output_tokens);
        self.refresh_estimate();
    }

    /// Records Cohere rerank searches.
    pub fn record_rerank(&mut self, calls: u64) {
        self.rerank_calls = self.rerank_calls.saturating_add(calls);
        self.refresh_estimate();
    }

    /// Returns an error when the estimated spend is above the configured ceiling.
    pub fn check_budget(&self) -> std::result::Result<(), CostError> {
        if self.est_usd > self.budget_usd {
            return Err(CostError::OverBudget {
                est_usd: self.est_usd,
                budget_usd: self.budget_usd,
            });
        }
        Ok(())
    }

    fn refresh_estimate(&mut self) {
        self.est_usd = usd_per_million(
            self.embed_input_tokens,
            COHERE_EMBED_V4_INPUT_USD_PER_MILLION_TOKENS,
        ) + usd_per_million(
            self.chat_input_tokens,
            COHERE_COMMAND_A_INPUT_USD_PER_MILLION_TOKENS,
        ) + usd_per_million(
            self.chat_output_tokens,
            COHERE_COMMAND_A_OUTPUT_USD_PER_MILLION_TOKENS,
        ) + self.rerank_calls as f64 * COHERE_RERANK_V4_FAST_USD_PER_SEARCH;
    }
}

impl Default for CostLedger {
    fn default() -> Self {
        Self::new(0.0)
    }
}

/// Provider model provenance recorded beside eval reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProvenance {
    /// Eval lane that selected the providers.
    pub lane: String,
    /// Embedding model identifier.
    pub embedding_model: String,
    /// Embedding model version stored beside graph vectors.
    pub embedding_model_version: i32,
    /// Fact extractor implementation or model identifier.
    pub extractor_model: String,
    /// Extraction prompt version, when an LLM extractor is in use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction_prompt_version: Option<String>,
    /// Entity merge verifier implementation or model identifier.
    pub merge_verifier_model: String,
    /// Merge prompt version, when an LLM or recorded verifier is in use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_prompt_version: Option<String>,
    /// Reranker model identifier.
    pub reranker_model: String,
}

/// Cost-budget enforcement error.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CostError {
    /// The estimated cost exceeded the configured ceiling.
    #[error("eval cost estimate ${est_usd:.4} exceeds budget ${budget_usd:.4}")]
    OverBudget {
        /// Estimated spend in USD.
        est_usd: f64,
        /// Budget ceiling in USD.
        budget_usd: f64,
    },
}

/// Estimates token count using the documented fallback of four UTF-8 chars per token.
#[must_use]
pub fn estimate_tokens_from_chars(text: &str) -> u64 {
    let chars = text.chars().count() as u64;
    chars.div_ceil(4).max(1)
}

fn usd_per_million(tokens: u64, usd_per_million_tokens: f64) -> f64 {
    tokens as f64 * usd_per_million_tokens / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_ledger_accumulates_and_estimates_with_dated_pricing() {
        // Pins: live eval cost estimates include all provider families and expose the pricing date.
        let mut ledger = CostLedger::new(10.0);

        ledger.record_embed(1_000_000);
        ledger.record_chat(1_000_000, 1_000_000);
        ledger.record_rerank(2);

        assert_eq!(ledger.pricing_as_of, PRICING_AS_OF);
        assert_eq!(ledger.embed_input_tokens, 1_000_000);
        assert_eq!(ledger.chat_input_tokens, 1_000_000);
        assert_eq!(ledger.chat_output_tokens, 1_000_000);
        assert_eq!(ledger.rerank_calls, 2);
        assert_eq!(
            ledger.est_usd,
            COHERE_EMBED_V4_INPUT_USD_PER_MILLION_TOKENS
                + COHERE_COMMAND_A_INPUT_USD_PER_MILLION_TOKENS
                + COHERE_COMMAND_A_OUTPUT_USD_PER_MILLION_TOKENS
                + (2.0 * COHERE_RERANK_V4_FAST_USD_PER_SEARCH)
        );
    }

    #[test]
    fn budget_check_errors_when_estimate_exceeds_ceiling() {
        // Pins: budget enforcement fails on actual estimated spend, not on raw call counts.
        let mut ledger = CostLedger::new(0.001);
        ledger.record_rerank(1);

        let error = ledger
            .check_budget()
            .expect_err("one rerank call should exceed a tiny budget");

        assert!(matches!(
            error,
            CostError::OverBudget {
                est_usd,
                budget_usd
            } if est_usd > budget_usd
        ));
    }
}
