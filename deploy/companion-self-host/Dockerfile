# syntax=docker/dockerfile:1.8

ARG RUST_VERSION=1.92.0

FROM rust:${RUST_VERSION}-bookworm AS builder
WORKDIR /src

# Use a two-member container workspace so Cargo never needs the manifests or
# multi-gigabyte native artifacts of the desktop/mobile workspace members.
COPY deploy/companion-self-host/container-workspace/Cargo.container.toml ./Cargo.toml
COPY deploy/companion-self-host/container-workspace/Cargo.lock ./Cargo.lock
COPY crates/aokie-protocol /src/crates/aokie-protocol
COPY crates/aokie-realtime /src/crates/aokie-realtime
RUN cargo build --locked --release -p aokie-realtime \
    && strip target/release/aokie-realtime

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 65532 aokie \
    && useradd --system --uid 65532 --gid 65532 --no-create-home --home-dir /nonexistent aokie

COPY --from=builder /src/target/release/aokie-realtime /usr/local/bin/aokie-realtime
COPY deploy/companion-self-host/gateway-entrypoint.sh /usr/local/bin/gateway-entrypoint
RUN chmod 0555 /usr/local/bin/aokie-realtime /usr/local/bin/gateway-entrypoint

USER 65532:65532
EXPOSE 18787/tcp

# This verifies the deployment invariant, not merely that a TCP port opened:
# v2 dynamic admissions are enabled, static v2 fallback is off, and this
# process remains a signalling-only service that does not bridge media.
HEALTHCHECK --interval=10s --timeout=3s --start-period=10s --retries=5 \
  CMD health="$(curl --fail --silent --show-error --max-time 2 http://127.0.0.1:18787/health)" \
   && version="$(curl --fail --silent --show-error --max-time 2 http://127.0.0.1:18787/version)" \
   && printf '%s' "$health" | grep --quiet '"status":"ok"' \
   && printf '%s' "$health" | grep --quiet '"v2Enabled":true' \
   && printf '%s' "$version" | grep --quiet '"v2DynamicAdmission":true' \
   && printf '%s' "$version" | grep --quiet '"v2StaticAdmissionFallback":false' \
   && printf '%s' "$version" | grep --quiet '"signallingOnly":true' \
   && printf '%s' "$version" | grep --quiet '"mediaBridge":false'

ENTRYPOINT ["/usr/local/bin/gateway-entrypoint"]
