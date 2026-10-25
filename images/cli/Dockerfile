# Build context: the repository root (images/test/smoke.sh cli, scripts/agent-binary.sh).
# Both binaries are static x86_64-unknown-linux-musl builds: `ciburst apply` uploads the agent
# from next to its own executable, and VMs run it on any Linux distribution.
FROM rust:1.96.0-bookworm AS build

RUN apt-get update \
 && apt-get install -y --no-install-recommends musl-tools \
 && rm -rf /var/lib/apt/lists/* \
 && rustup target add x86_64-unknown-linux-musl

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --target x86_64-unknown-linux-musl \
      --package ciburst --package ciburst-vm-agent \
 && mkdir -p /out \
 && cp target/x86_64-unknown-linux-musl/release/ciburst \
       target/x86_64-unknown-linux-musl/release/ciburst-vm-agent /out/

FROM alpine:3.24.3

LABEL org.opencontainers.image.source="https://gitlab.com/ciburst/ciburst" \
      org.opencontainers.image.title="ciburst CLI and VM agent" \
      org.opencontainers.image.licenses="Apache-2.0"

COPY --from=build /out/ciburst /out/ciburst-vm-agent /usr/local/bin/
