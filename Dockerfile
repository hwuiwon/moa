FROM rust:1-bookworm AS builder
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        clang \
        libprotobuf-dev \
        pkg-config \
        protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

COPY . .
ENV PROTOC=/usr/bin/protoc
ENV PROTOC_INCLUDE=/usr/include
RUN cargo build --locked --release -p moa-cli --features "cloud"

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/moa /usr/local/bin/moa

ENV MOA__CLOUD__ENABLED=true \
    MOA__CLOUD__HANDS__DEFAULT_PROVIDER=local \
    MOA__LOCAL__SESSION_DB=/data/sessions.db \
    MOA__LOCAL__MEMORY_DIR=/data/memory \
    MOA__LOCAL__SANDBOX_DIR=/data/sandbox \
    MOA__CLOUD__MEMORY_DIR=/data/memory \
    MOA__CLOUD__FLYIO__INTERNAL_PORT=8080

EXPOSE 8080
VOLUME ["/data"]

ENTRYPOINT ["moa", "daemon", "serve"]
