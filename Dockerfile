# syntax=docker/dockerfile:1.6

FROM rust:1.95.0-bookworm AS builder
WORKDIR /build

COPY . .
ARG MOA_ORCHESTRATOR_FEATURES=""
RUN if [ -n "${MOA_ORCHESTRATOR_FEATURES}" ]; then \
      cargo build --locked --release -p moa-orchestrator --bin moa-orchestrator-bin --features "${MOA_ORCHESTRATOR_FEATURES}"; \
    else \
      cargo build --locked --release -p moa-orchestrator --bin moa-orchestrator-bin; \
    fi

FROM debian:12-slim
ARG MOA_BUILD_REVISION=development
LABEL org.opencontainers.image.revision="${MOA_BUILD_REVISION}"
ENV MOA_OBSERVABILITY_RELEASE="${MOA_BUILD_REVISION}"
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates liblzma5 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 65532 --gid nogroup --home /var/lib/moa --shell /usr/sbin/nologin nonroot \
    && mkdir -p /var/lib/moa \
    && chown -R 65532:65534 /var/lib/moa

COPY --from=builder /build/target/release/moa-orchestrator-bin /usr/local/bin/moa-orchestrator

EXPOSE 9080 9081

USER 65532:65534
ENTRYPOINT ["/usr/local/bin/moa-orchestrator"]
CMD ["--port", "9080", "--health-port", "9081"]
