# Build context: the repository root (images/vm-agent/build.sh).
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
    cargo build --release --locked --target x86_64-unknown-linux-musl --package ciburst-vm-agent \
 && cp target/x86_64-unknown-linux-musl/release/ciburst-vm-agent /ciburst-vm-agent

FROM scratch

LABEL org.opencontainers.image.source="https://gitlab.com/<namespace>/ciburst" \
      org.opencontainers.image.title="ciburst VM agent" \
      org.opencontainers.image.licenses="Apache-2.0"

COPY --from=build /ciburst-vm-agent /ciburst-vm-agent
ENTRYPOINT ["/ciburst-vm-agent"]
