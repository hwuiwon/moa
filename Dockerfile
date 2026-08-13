# syntax=docker/dockerfile:1.6

FROM rust:1.95.0-bookworm AS builder
WORKDIR /build

COPY . .
ARG MOA_ORCHESTRATOR_FEATURES=""
# `.cargo/config.toml` adds `-C prefer-dynamic` to speed up local dev builds, but the release
# profile enables thin LTO and rustc rejects that combination outright ("cannot prefer dynamic
# linking when performing LTO"). Only release builds hit it, so it breaks image builds while
# leaving every local `cargo build` working, which is why it can go unnoticed.
#
# `RUSTFLAGS` is the only setting that reliably wins here: it sits above `build.rustflags` in
# cargo's precedence order, whereas `CARGO_BUILD_RUSTFLAGS` is merely the environment spelling
# of that same config key and does not displace it.
#
# It replaces the flags wholesale rather than appending, so `tokio_unstable` has to be restated:
# the runtime metrics hooks the code compiles against are gated on it, and dropping it fails the
# build on missing cfg items. A statically linked release binary is also what the runtime stage
# needs, since it copies the binary into a slim image without the toolchain's shared objects.
ENV RUSTFLAGS="--cfg tokio_unstable"
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
