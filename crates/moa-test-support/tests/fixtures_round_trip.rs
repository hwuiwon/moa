//! Round-trip tests for bundled test-support fixtures.

use std::path::Path;

use moa_core::transcript::{ProviderEvent, Transcript, TranscriptError, Turn, UserUtterance};
use moa_test_support::postgres::bootstrap_test_db;
use moa_test_support::pricing::PricingTable;
use sqlx::Row;

#[test]
fn pricing_table_loads_with_all_required_provider_model_pairs() {
    let table = PricingTable::load();
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
    let table = PricingTable::load();
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
#[ignore = "requires MOA_DATABASE_URL and a reachable Postgres instance"]
async fn bootstrap_test_db_creates_isolated_database_and_drops_on_drop() {
    // Pins: bootstrap clones an isolated per-test database that holds the session
    // schema, and dropping the `TestDb` drops that whole database.
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

    // Derive the per-test database name and a maintenance URL to inspect the
    // server catalog after the database is dropped.
    let (prefix, db_name) = database_url
        .rsplit_once('/')
        .map(|(prefix, db)| {
            (
                prefix.to_string(),
                db.split('?').next().unwrap_or(db).to_string(),
            )
        })
        .expect("per-test database url has a database segment");
    let maintenance_url = format!("{prefix}/moa");

    drop(db);

    let pool = sqlx::PgPool::connect(&maintenance_url)
        .await
        .expect("connect to maintenance database after drop");
    let exists_after_drop: bool =
        sqlx::query("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&db_name)
            .fetch_one(&pool)
            .await
            .expect("query database after drop")
            .try_get(0)
            .expect("read database existence after drop");
    pool.close().await;
    assert!(
        !exists_after_drop,
        "expected isolated database {db_name} to be dropped"
    );
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL and a reachable Postgres instance"]
async fn test_db_drop_completes_when_a_checked_out_connection_never_returns() {
    // Pins: `TestDb::drop` blocks the test runtime's thread while it joins its
    // cleanup thread, so a background task still holding a checked-out pooled
    // connection can never be polled again to return it. A graceful
    // `pool.close()` in that cleanup therefore waited forever — a three-party
    // deadlock (main thread joins cleanup, cleanup awaits close, close awaits
    // a connection owned by a task only the blocked main thread could poll)
    // that hung the owning test until the harness killed it. The bounded close
    // plus `DROP DATABASE ... WITH (FORCE)` must complete cleanup anyway.
    let db = bootstrap_test_db().await.expect("bootstrap test db");
    // Model the frozen holder faithfully: a live task that has checked out a
    // connection and parks forever. Once `drop(db)` blocks this runtime's
    // thread, the task can never be polled again, so the connection is never
    // released — `mem::forget` is not equivalent because sqlx's `close()`
    // waits only on live holders, not leaked permits.
    let pool = db.store().pool().clone();
    let _holder = tokio::spawn(async move {
        let _conn = pool.acquire().await.expect("check out a pooled connection");
        std::future::pending::<()>().await;
    });
    // Let the holder actually acquire before the drop freezes the runtime.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let started = std::time::Instant::now();
    drop(db);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "TestDb cleanup must complete despite an unreturned connection; took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL and a reachable Postgres instance"]
async fn test_db_drop_returns_within_its_bound_when_cleanup_cannot_connect() {
    // Pins: the whole drop-time cleanup future is bounded. The destructor
    // blocks the test runtime's thread while joining its cleanup thread, so an
    // unbounded await in cleanup (observed in the wild parked in the io driver
    // before Postgres ever saw a connection) hangs the owning test until the
    // harness kills it at 240s. With the bound, a cleanup that cannot connect
    // gives up, warns, and leaves the clone to the provisioning orphan sweep.
    let mut db = bootstrap_test_db().await.expect("bootstrap test db");
    // Blackholed RFC1918 address: connect attempts hang rather than refuse,
    // modeling the observed stall. The real clone this bootstrap created is
    // deliberately orphaned and reaped by the >1h sweep.
    db.override_cleanup_url_for_tests(
        "postgres://moa_owner:dev@10.255.255.1:10040/moa_test_cleanup_blackhole".to_string(),
    );

    let started = std::time::Instant::now();
    drop(db);
    let elapsed = started.elapsed();
    // The macOS SYN-retry timeout alone returns in ~35s, so the assertion sits
    // between the cleanup bound (15s) and that OS floor: only the explicit
    // bound can satisfy it.
    assert!(
        elapsed < std::time::Duration::from_secs(25),
        "bounded cleanup must return even when its maintenance connect hangs; took {elapsed:?}"
    );
}

fn fixture_path(relative: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}
