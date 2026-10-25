# Build context: images/
FROM gitlab/gitlab-runner:v18.11.4 AS runner

FROM rust:1.96.0-bookworm

LABEL org.opencontainers.image.source="https://gitlab.com/ciburst/ciburst" \
      org.opencontainers.image.title="ciburst rust pool image" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN apt-get update \
 && apt-get install -y --no-install-recommends bash ca-certificates curl git git-lfs \
 && rm -rf /var/lib/apt/lists/*

RUN rustup component add rustfmt clippy

COPY --from=runner /usr/bin/gitlab-runner /usr/local/bin/gitlab-runner
COPY --chmod=0755 base/entrypoint.sh /usr/local/bin/ciburst-entrypoint

RUN useradd --create-home --shell /bin/bash runner \
 && rm -f /home/runner/.bash_logout \
 && echo "export PATH=\"$PATH\"" > /etc/profile.d/ciburst-path.sh \
 && mkdir -p /builds /cache \
 && chown runner:runner /builds /cache \
 && chown -R runner:runner "$CARGO_HOME"

USER runner
WORKDIR /home/runner
ENTRYPOINT ["/usr/local/bin/ciburst-entrypoint"]
