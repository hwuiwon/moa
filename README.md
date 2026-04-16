# MOA

MOA is a cloud-first general-purpose AI agent platform written in Rust.

## Quickstart

```bash
# Start Postgres
docker compose up -d

# Build and initialize
cargo build
cargo run --bin moa -- init
cargo run --bin moa -- doctor

# Run
cargo run --bin moa -- exec "hello"
```

Local development requires Docker and Postgres. MOA no longer supports SQLite or Turso.
