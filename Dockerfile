FROM rust:1.96.1-bookworm AS builder

WORKDIR /workspace
COPY . .
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain 1.96.1 --profile minimal --target wasm32-wasip2
RUN . /usr/local/cargo/env \
    && cargo build --release --workspace \
    && cargo build --manifest-path examples/plugins/api-key-guard/Cargo.toml --target wasm32-wasip2 --release \
    && cargo build --manifest-path examples/plugins/header-normalizer/Cargo.toml --target wasm32-wasip2 --release

FROM debian:12-slim

ARG TRACEGATE_GIT_SHA=dev
ENV TRACEGATE_GIT_SHA=${TRACEGATE_GIT_SHA}

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --home-dir /var/lib/tracegate tracegate \
    && mkdir -p /usr/local/share/tracegate/plugins

COPY --from=builder /workspace/target/release/tracegate /usr/local/bin/tracegate
COPY --from=builder /workspace/target/release/tracegate-demo-backend /usr/local/bin/tracegate-demo-backend
COPY --from=builder /workspace/examples/plugins/api-key-guard/target/wasm32-wasip2/release/tracegate_api_key_guard.wasm /usr/local/share/tracegate/plugins/api-key-guard.wasm
COPY --from=builder /workspace/examples/plugins/header-normalizer/target/wasm32-wasip2/release/tracegate_header_normalizer.wasm /usr/local/share/tracegate/plugins/header-normalizer.wasm

USER tracegate
CMD ["tracegate", "--help"]
