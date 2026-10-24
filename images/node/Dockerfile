# Build context: images/
FROM gitlab/gitlab-runner:v18.11.4 AS runner

FROM node:24.22.1-bookworm-slim

LABEL org.opencontainers.image.source="https://gitlab.com/<namespace>/ciburst" \
      org.opencontainers.image.title="ciburst node pool image" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN apt-get update \
 && apt-get install -y --no-install-recommends bash ca-certificates curl git git-lfs \
 && rm -rf /var/lib/apt/lists/*

COPY --from=runner /usr/bin/gitlab-runner /usr/local/bin/gitlab-runner
COPY --chmod=0755 base/entrypoint.sh /usr/local/bin/ciburst-entrypoint

RUN useradd --create-home --shell /bin/bash runner \
 && mkdir -p /builds /cache \
 && chown runner:runner /builds /cache

USER runner
WORKDIR /home/runner
ENTRYPOINT ["/usr/local/bin/ciburst-entrypoint"]
