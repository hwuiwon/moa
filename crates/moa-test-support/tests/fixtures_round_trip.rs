//! Round-trip tests for bundled test-support fixtures.

use std::path::Path;

use moa_test_support::postgres::bootstrap_test_db;
use moa_test_support::pricing::PricingTable;
use moa_test_support::transcript::{
    ProviderEvent, Transcript, TranscriptError, Turn, UserUtterance,
};
use sqlx::Row;

#[test]
fn pricing_table_v1_loads_with_all_required_provider_model_pairs() {
    let table = PricingTable::load_v1();
    for (provider, model) in [
        ("anthropic", "claude-sonnet-4"),
        ("anthropic", "claude-haiku-4"),
        ("openai", "gpt-4.1"),
        ("openai", "gpt-4.1-mini"),
        ("gemini", "gemini-3.1-pro-preview"),
        ("gemini", "gemini-3-pro-preview"),
        ("gemini", "gemini-3-flash-preview"),
    ] {
        table
            .get(provider, model)
            .unwrap_or_else(|error| panic!("missing required fixture {provider}/{model}: {error}"));
    }
}

/// Anthropic publishes Claude Sonnet 4 at $3 input, $15 output, and $0.30
/// cached input per million tokens. The expected value is derived from those
/// fixture rates and rounded up only after summing all token classes.
#[test]
fn pricing_cost_cents_matches_published_anthropic_sonnet_for_known_token_counts() {
    let table = PricingTable::load_v1();
    let pricing = table
        .get("anthropic", "claude-sonnet-4")
        .expect("sonnet pricing fixture");
    let input_tokens = 125_000;
    let output_tokens = 20_000;
    let cached_input_tokens = 50_000;
    let expected = (input_tokens * u128::from(pricing.input_per_mtok_cents)
        + output_tokens * u128::from(pricing.output_per_mtok_cents)
        + cached_input_tokens
            * u128::from(
                pricing
                    .cached_input_per_mtok_cents
                    .expect("sonnet cached input pricing"),
            ))
    .div_ceil(1_000_000);

    assert_eq!(
        table
            .cost_cents(
                "anthropic",
                "claude-sonnet-4",
                input_tokens as u64,
                output_tokens as u64,
                cached_input_tokens as u64,
            )
            .expect("sonnet cost"),
        expected as u32
    );
}

#[test]
fn transcript_jsonl_round_trips_through_read_and_write() {
    let fixture = fixture_path("fixtures/transcripts/example_minimal.jsonl");
    let transcript = Transcript::read_jsonl(&fixture).expect("read example transcript");
    let temp = tempfile::tempdir().expect("tempdir");
    let output = temp.path().join("round_trip.jsonl");

    transcript.write_jsonl(&output).expect("write transcript");
    let round_tripped = Transcript::read_jsonl(&output).expect("read round-tripped transcript");

    assert_eq!(round_tripped, transcript);
}

#[test]
fn transcript_validate_rejects_turn_without_terminal_event() {
    let transcript = Transcript {
        version: 1,
        scenario: "missing_terminal".to_string(),
        turns: vec![Turn {
            user: UserUtterance {
                text: "hello".to_string(),
            },
            expected: vec![ProviderEvent::TextDelta {
                text: "hi".to_string(),
            }],
        }],
    };

    let error = transcript
        .validate()
        .expect_err("turn without terminal should fail validation");
    assert!(matches!(
        error,
        TranscriptError::MissingTerminalEvent { .. }
    ));
}

#[tokio::test]
#[ignore = "requires MOA_TEST_POSTGRES_URL and a reachable Postgres instance"]
async fn bootstrap_test_db_creates_isolated_schema_and_drops_on_drop() {
    let db = bootstrap_test_db().await.expect("bootstrap test db");
    let database_url = db.database_url().to_string();
    let schema_name = db.schema_name().to_string();
    let exists: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = $1)",
    )
    .bind(&schema_name)
    .fetch_one(db.store().pool())
    .await
    .expect("query schema existence")
    .try_get(0)
    .expect("read schema existence");
    assert!(exists, "expected isolated schema {schema_name} to exist");

    drop(db);

    let pool = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect after drop");
    let exists_after_drop: bool = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = $1)",
    )
    .bind(&schema_name)
    .fetch_one(&pool)
    .await
    .expect("query schema after drop")
    .try_get(0)
    .expect("read schema existence after drop");
    pool.close().await;
    assert!(
        !exists_after_drop,
        "expected isolated schema {schema_name} to be dropped"
    );
}

fn fixture_path(relative: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}
