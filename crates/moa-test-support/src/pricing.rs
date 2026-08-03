//! Canonical provider pricing fixtures for tests.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

const PRICING: &str = include_str!("../fixtures/pricing/v1.json");
const TOKENS_PER_MTOK: u128 = 1_000_000;

/// One provider/model pricing row, expressed in USD cents per million tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderPricing {
    /// Standard input-token price in cents per million tokens.
    pub input_per_mtok_cents: u32,
    /// Output-token price in cents per million tokens.
    pub output_per_mtok_cents: u32,
    /// Cached input-token price in cents per million tokens.
    pub cached_input_per_mtok_cents: Option<u32>,
}

/// A versioned provider/model pricing table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingTable {
    /// Fixture schema version.
    pub version: u32,
    /// Pricing rows indexed by provider id and then model id.
    pub providers: HashMap<String, HashMap<String, ProviderPricing>>,
}

/// Errors returned by pricing-table lookups and arithmetic.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PricingError {
    /// The provider id was not present in the fixture.
    #[error("pricing provider not found: {0}")]
    ProviderNotFound(String),
    /// The model id was not present for the provider.
    #[error("pricing model not found for provider {provider}: {model}")]
    ModelNotFound {
        /// Provider id.
        provider: String,
        /// Model id.
        model: String,
    },
    /// Integer arithmetic exceeded the supported range.
    #[error("pricing cost arithmetic overflowed")]
    ArithmeticOverflow,
}

impl PricingTable {
    /// Loads the bundled v1 pricing fixture.
    ///
    /// The fixture is embedded with `include_str!`, so tests do not need
    /// filesystem access to load canonical pricing.
    #[must_use]
    pub fn load() -> Self {
        let json = strip_json_comments(PRICING);
        match serde_json::from_str(&json) {
            Ok(table) => table,
            Err(error) => panic!("bundled pricing fixture v1.json is invalid: {error}"),
        }
    }

    /// Returns one provider/model pricing row.
    pub fn get(&self, provider: &str, model: &str) -> Result<&ProviderPricing, PricingError> {
        let models = self
            .providers
            .get(provider)
            .ok_or_else(|| PricingError::ProviderNotFound(provider.to_string()))?;
        models
            .get(model)
            .ok_or_else(|| PricingError::ModelNotFound {
                provider: provider.to_string(),
                model: model.to_string(),
            })
    }

    /// Computes total cost in cents using checked integer math.
    ///
    /// `input_tokens` are billed at the standard input rate, `cached_input_tokens`
    /// are billed at the cached-input rate when the fixture has one, and
    /// `output_tokens` are billed at the output rate. The function sums the
    /// exact cent-token products and rounds up to the nearest whole cent only
    /// after the final division by one million tokens.
    pub fn cost_cents(
        &self,
        provider: &str,
        model: &str,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
    ) -> Result<u32, PricingError> {
        let pricing = self.get(provider, model)?;
        pricing.cost_cents(input_tokens, output_tokens, cached_input_tokens)
    }
}

impl ProviderPricing {
    /// Computes total cost in cents for one pricing row.
    ///
    /// The calculation uses checked integer math and rounds up to the nearest
    /// whole cent only after summing all token-class subtotals.
    pub fn cost_cents(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
    ) -> Result<u32, PricingError> {
        let input = checked_cent_tokens(input_tokens, self.input_per_mtok_cents)?;
        let cached = checked_cent_tokens(
            cached_input_tokens,
            self.cached_input_per_mtok_cents
                .unwrap_or(self.input_per_mtok_cents),
        )?;
        let output = checked_cent_tokens(output_tokens, self.output_per_mtok_cents)?;
        let total = input
            .checked_add(cached)
            .and_then(|value| value.checked_add(output))
            .ok_or(PricingError::ArithmeticOverflow)?;
        let rounded = ceil_div(total, TOKENS_PER_MTOK)?;
        u32::try_from(rounded).map_err(|_| PricingError::ArithmeticOverflow)
    }
}

fn checked_cent_tokens(tokens: u64, cents_per_mtok: u32) -> Result<u128, PricingError> {
    u128::from(tokens)
        .checked_mul(u128::from(cents_per_mtok))
        .ok_or(PricingError::ArithmeticOverflow)
}

fn ceil_div(numerator: u128, denominator: u128) -> Result<u128, PricingError> {
    if numerator == 0 {
        return Ok(0);
    }
    numerator
        .checked_add(denominator - 1)
        .map(|value| value / denominator)
        .ok_or(PricingError::ArithmeticOverflow)
}

fn strip_json_comments(input: &str) -> String {
    input
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}
