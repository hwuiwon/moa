# syntax=docker/dockerfile:1.6

FROM rust:1.95.0-bookworm AS builder
WORKDIR /build

COPY . .
RUN cargo build --locked --release -p moa-orchestrator

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /build/target/release/moa-orchestrator /usr/local/bin/moa-orchestrator
COPY --from=builder /build/crates/moa-session/migrations /migrations

USER nonroot
ENTRYPOINT ["/usr/local/bin/moa-orchestrator"]
CMD ["--port", "9080", "--health-port", "9081"]
