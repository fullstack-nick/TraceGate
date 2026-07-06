FROM rust:1.96.1-bookworm AS builder

WORKDIR /workspace
COPY . .
RUN cargo build --release --workspace

FROM debian:12-slim

ARG TRACEGATE_GIT_SHA=dev
ENV TRACEGATE_GIT_SHA=${TRACEGATE_GIT_SHA}

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --home-dir /var/lib/tracegate tracegate

COPY --from=builder /workspace/target/release/tracegate /usr/local/bin/tracegate
COPY --from=builder /workspace/target/release/tracegate-demo-backend /usr/local/bin/tracegate-demo-backend

USER tracegate
CMD ["tracegate", "--help"]
