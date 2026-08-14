# moa-test-support

Internal-only fixtures and helpers for MOA tests. This crate is `publish = false` and should only be added as a `[dev-dependencies]` entry.

## Pricing Fixtures

`moa_test_support::pricing` embeds the canonical v1 provider pricing fixture and computes costs with checked integer math.

```rust
use moa_test_support::pricing::PricingTable;

let table = PricingTable::load();
let cents = table.cost_cents("anthropic", "claude-sonnet-4", 125_000, 20_000, 50_000)?;
```

## Recorded Transcripts

`moa_eval_core::transcript` reads and writes JSONL transcripts with one metadata line followed by one turn per line. Every turn must end with a terminal provider event.

```rust
use moa_eval_core::transcript::Transcript;

let transcript = Transcript::read_jsonl("crates/moa-test-support/fixtures/transcripts/example_minimal.jsonl".as_ref())?;
transcript.validate()?;
```

## Postgres Helpers

`moa_test_support::postgres` supports `MOA_DATABASE_URL` for an explicit database, or the Docker Compose default `postgres://moa_owner:dev@127.0.0.1:10040/moa` after `docker compose up -d postgres`. `bootstrap_test_db` creates a fresh `test_<uuid>` schema and drops it when the returned `TestDb` is dropped.

```rust
use moa_test_support::postgres::bootstrap_test_db;

let db = bootstrap_test_db().await?;
let store = db.store();
```
