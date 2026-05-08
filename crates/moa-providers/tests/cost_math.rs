//! Fixture-backed provider cost arithmetic tests.

use moa_test_support::pricing::{PricingTable, ProviderPricing};

const ONE_MTOK: u128 = 1_000_000;

const ANTHROPIC_SONNET_COUNTS: TokenCounts = TokenCounts {
    input: 125_000,
    output: 20_000,
    cached: 50_000,
};
const OPENAI_GPT41_COUNTS: TokenCounts = TokenCounts {
    input: 90_000,
    output: 15_000,
    cached: 30_000,
};
const GEMINI_PRO_COUNTS: TokenCounts = TokenCounts {
    input: 80_000,
    output: 12_000,
    cached: 24_000,
};
const CACHE_DISCOUNT_COUNTS: TokenCounts = TokenCounts {
    input: 500_000,
    output: 10_000,
    cached: 500_000,
};
const ROUNDING_COUNTS: TokenCounts = TokenCounts {
    input: 1,
    output: 1,
    cached: 1,
};

#[derive(Clone, Copy)]
struct TokenCounts {
    input: u64,
    output: u64,
    cached: u64,
}

#[test]
fn cost_cents_for_anthropic_sonnet_matches_pricing_table_v1_for_known_token_counts() {
    let table = PricingTable::load_v1();
    let row = pricing_row(&table, "anthropic", "claude-sonnet-4-6");
    let expected = expected_from_row(row, ANTHROPIC_SONNET_COUNTS);
    let actual = table_cost(
        &table,
        "anthropic",
        "claude-sonnet-4-6",
        ANTHROPIC_SONNET_COUNTS,
    );

    assert_eq!(actual, expected);
}

#[test]
fn cost_cents_for_openai_gpt41_matches_pricing_table_v1_for_known_token_counts() {
    let table = PricingTable::load_v1();
    let row = pricing_row(&table, "openai", "gpt-4.1");
    let expected = expected_from_row(row, OPENAI_GPT41_COUNTS);
    let actual = table_cost(&table, "openai", "gpt-4.1", OPENAI_GPT41_COUNTS);

    assert_eq!(actual, expected);
}

#[test]
fn cost_cents_for_gemini_pro_matches_pricing_table_v1_for_known_token_counts() {
    let table = PricingTable::load_v1();
    let row = pricing_row(&table, "gemini", "gemini-3-pro-preview");
    let expected = expected_from_row(row, GEMINI_PRO_COUNTS);
    let actual = table_cost(&table, "gemini", "gemini-3-pro-preview", GEMINI_PRO_COUNTS);

    assert_eq!(actual, expected);
}

#[test]
fn cost_cents_with_cached_input_tokens_uses_discounted_rate() {
    let table = PricingTable::load_v1();
    let cached = table_cost(
        &table,
        "anthropic",
        "claude-sonnet-4-6",
        CACHE_DISCOUNT_COUNTS,
    );
    let uncached = table_cost(
        &table,
        "anthropic",
        "claude-sonnet-4-6",
        TokenCounts {
            input: CACHE_DISCOUNT_COUNTS.input + CACHE_DISCOUNT_COUNTS.cached,
            output: CACHE_DISCOUNT_COUNTS.output,
            cached: 0,
        },
    );

    assert!(cached < uncached);
}

#[test]
fn cost_cents_rounds_up_to_nearest_cent_at_final_step_only() {
    let table = PricingTable::load_v1();
    let row = pricing_row(&table, "gemini", "gemini-3-flash-preview");
    let combined = row
        .cost_cents(
            ROUNDING_COUNTS.input,
            ROUNDING_COUNTS.output,
            ROUNDING_COUNTS.cached,
        )
        .expect("combined fixture cost");
    let separate = row
        .cost_cents(ROUNDING_COUNTS.input, 0, 0)
        .expect("input fixture cost")
        + row
            .cost_cents(0, ROUNDING_COUNTS.output, 0)
            .expect("output fixture cost")
        + row
            .cost_cents(0, 0, ROUNDING_COUNTS.cached)
            .expect("cached fixture cost");

    assert_eq!(combined, expected_from_row(row, ROUNDING_COUNTS));
    assert!(
        separate > combined,
        "rounding each token class separately would overstate the fixture total"
    );
}

fn pricing_row<'a>(table: &'a PricingTable, provider: &str, model: &str) -> &'a ProviderPricing {
    table
        .get(provider, model)
        .unwrap_or_else(|error| panic!("missing fixture pricing for {provider}/{model}: {error}"))
}

fn table_cost(table: &PricingTable, provider: &str, model: &str, counts: TokenCounts) -> u32 {
    table
        .cost_cents(provider, model, counts.input, counts.output, counts.cached)
        .unwrap_or_else(|error| panic!("fixture cost failed for {provider}/{model}: {error}"))
}

fn expected_from_row(row: &ProviderPricing, counts: TokenCounts) -> u32 {
    let standard = token_product(counts.input, row.input_per_mtok_cents);
    let output = token_product(counts.output, row.output_per_mtok_cents);
    let cached = token_product(
        counts.cached,
        row.cached_input_per_mtok_cents
            .unwrap_or(row.input_per_mtok_cents),
    );
    let total = standard + output + cached;

    u32::try_from(round_up(total, ONE_MTOK)).expect("fixture total should fit in u32")
}

fn token_product(tokens: u64, rate: u32) -> u128 {
    u128::from(tokens) * u128::from(rate)
}

fn round_up(numerator: u128, denominator: u128) -> u128 {
    if numerator == 0 {
        return 0;
    }

    numerator.div_ceil(denominator)
}
