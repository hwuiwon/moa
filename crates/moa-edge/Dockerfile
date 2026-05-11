# syntax=docker/dockerfile:1.6

FROM rust:1.95.0-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --locked --release -p moa-edge --bin moa-edge

FROM debian:12-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates wget \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 65532 --gid nogroup --home /nonexistent --shell /usr/sbin/nologin nonroot

COPY --from=builder /build/target/release/moa-edge /usr/local/bin/moa-edge

EXPOSE 8080
USER 65532:65534
ENTRYPOINT ["/usr/local/bin/moa-edge"]
